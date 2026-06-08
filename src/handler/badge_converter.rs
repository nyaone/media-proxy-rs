// Warning: These code are implemented by AI.
// I don't know much about it, but it seems to work.
// Also, please don't ask me anything about those magic numbers. I have no idea. They just work.

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageBuffer, Luma, Rgba, RgbaImage};

type Grayf32 = ImageBuffer<Luma<f32>, Vec<f32>>;

// sRGB piecewise gamma to linear light
fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

// linear light to sRGB
fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

// Gamma-correct Rec. 709 greyscale (matches sharp/libvips behaviour)
fn rgb_to_gray(img: &image::ImageBuffer<image::Rgb<f32>, Vec<f32>>) -> Grayf32 {
    Grayf32::from_fn(img.width(), img.height(), |x, y| {
        let p = img.get_pixel(x, y);
        let r = srgb_to_linear(p[0]);
        let g = srgb_to_linear(p[1]);
        let b = srgb_to_linear(p[2]);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        Luma([linear_to_srgb(y)])
    })
}

// contain_resize: place the image centered on a black [0.0] canvas of target size
fn contain_resize(img: &DynamicImage, target_w: u32, target_h: u32) -> Grayf32 {
    let (w, h) = img.dimensions();
    let scale = (target_w as f64 / w as f64).min(target_h as f64 / h as f64);
    let nw = ((w as f64 * scale).round() as u32).max(1);
    let nh = ((h as f64 * scale).round() as u32).max(1);

    // Resize in f32 to match libvips's float pipeline precision
    let resized = image::imageops::resize(&img.to_rgb32f(), nw, nh, FilterType::Lanczos3);
    let gray = rgb_to_gray(&resized);

    let mut canvas = Grayf32::from_pixel(target_w, target_h, Luma([0.0f32]));
    let x_off = (target_w - nw) / 2;
    let y_off = (target_h - nh) / 2;
    for cy in 0..nh {
        for cx in 0..nw {
            canvas.put_pixel(cx + x_off, cy + y_off, *gray.get_pixel(cx, cy));
        }
    }
    canvas
}

// linear(a, b) in [0,1] space (b is the [0,255] offset divided by 255)
fn linear(img: &Grayf32, a: f32, b: f32) -> Grayf32 {
    Grayf32::from_fn(img.width(), img.height(), |x, y| {
        let v = img.get_pixel(x, y)[0];
        Luma([(v * a + b).clamp(0.0, 1.0)])
    })
}

fn entropy(img: &Grayf32) -> f64 {
    let mut hist = [0u64; 256];
    for p in img.pixels() {
        hist[(p[0].clamp(0.0, 1.0) * 255.0).round() as usize] += 1;
    }
    let total = (img.width() * img.height()) as f64;
    hist.iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / total;
            -p * p.log2()
        })
        .sum()
}

// XOR of transparent RGBA (0,0,0,0) with a 1-channel mask broadcasts to all 4 channels:
// result[x,y] = Rgba([v, v, v, v]) where v is the grayscale mask value.
fn to_rgba_mask(img: &Grayf32) -> RgbaImage {
    RgbaImage::from_fn(img.width(), img.height(), |x, y| {
        let v = (img.get_pixel(x, y)[0].clamp(0.0, 1.0) * 255.0).round() as u8;
        Rgba([v, v, v, v])
    })
}

pub fn convert_to_badge(img: &DynamicImage) -> Result<DynamicImage, ()> {
    let img = contain_resize(img, 96, 96);
    // sharp's one-build pipeline: normalise is a passthrough for full-range images because
    // libvips's Lanczos undershoot keeps p1 near 0; skip it to match observed behaviour.
    // 1.75x contrast boost centred at 128: output = input * 1.75 - 96 (in [0,255] → /255 for [0,1])
    let img = linear(&img, 1.75, -96.0 / 255.0);

    if entropy(&img) < 0.1 {
        // Entropy too low, skipping
        return Err(());
    }

    let result = to_rgba_mask(&img);

    Ok(DynamicImage::ImageRgba8(result))
}

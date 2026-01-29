use bytes::Bytes;
use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, Delay, DynamicImage, Frame, ImageDecoder, ImageFormat, ImageReader};
use std::io::Cursor;
use tracing::warn;

#[cfg(not(feature = "anim"))]
use tracing::info;

fn static_image(
    ori: Result<image::metadata::Orientation, image::ImageError>,
    mut img: DynamicImage,
) -> Result<Vec<(DynamicImage, Delay)>, image::ImageError> {
    if let Ok(ori) = ori {
        img.apply_orientation(ori);
    }
    Ok(vec![(img, Delay::from_numer_denom_ms(0, 1))])
}

fn frames_to_images(
    ori: Result<image::metadata::Orientation, image::ImageError>,
    frames: Vec<Frame>,
) -> Vec<(DynamicImage, Delay)> {
    let mut images: Vec<(DynamicImage, Delay)> = Vec::new();
    for frame in frames {
        let delay = frame.delay();
        let mut img = DynamicImage::from(frame.into_buffer());
        if let Ok(ori) = ori {
            img.apply_orientation(ori);
        }
        images.push((img, delay));
    }
    images
}

// Inspired by https://github.com/image-rs/image/issues/2360#issuecomment-3092626301
fn decode_image_format(
    img_reader: ImageReader<Cursor<&Bytes>>,
    format: ImageFormat,
) -> Result<Vec<(DynamicImage, Delay)>, image::ImageError> {
    match format {
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(img_reader.into_inner())?;
            let ori = decoder.orientation();
            decoder
                .into_frames()
                .collect_frames()
                .map(|f| frames_to_images(ori, f))
        }
        ImageFormat::Png => {
            let mut decoder = PngDecoder::new(img_reader.into_inner())?;
            let ori = decoder.orientation();
            if decoder.is_apng()? {
                decoder
                    .apng()?
                    .into_frames()
                    .collect_frames()
                    .map(|f| frames_to_images(ori, f))
            } else {
                static_image(ori, DynamicImage::from_decoder(decoder)?)
            }
        }
        ImageFormat::WebP => {
            let mut decoder = WebPDecoder::new(img_reader.into_inner())?;
            let ori = decoder.orientation();
            if decoder.has_animation() {
                decoder
                    .into_frames()
                    .collect_frames()
                    .map(|f| frames_to_images(ori, f))
            } else {
                static_image(ori, DynamicImage::from_decoder(decoder)?)
            }
        }
        _ => {
            let mut decoder = img_reader.into_decoder()?;
            static_image(decoder.orientation(), DynamicImage::from_decoder(decoder)?)
        }
    }
}

pub enum DecodeImageError {
    Unsupported,
    ImageError(image::ImageError),
}

pub fn decode_image(
    downloaded_bytes: &Bytes,
) -> Result<Vec<(DynamicImage, Delay)>, DecodeImageError> {
    // Check whether the file is an image (don't trust the content-type header or filename)
    // hint: misskey need to detect whether the file is manipulatable manually,
    // but here we are using image crate's format guessing feature
    let img_reader = ImageReader::new(Cursor::new(downloaded_bytes))
        .with_guessed_format()
        .unwrap();

    match img_reader.format() {
        Some(format) => {
            let decoded =
                decode_image_format(img_reader, format).map_err(DecodeImageError::ImageError);

            #[cfg(feature = "anim")]
            {
                decoded
            }

            #[cfg(not(feature = "anim"))]
            if let Ok(decoded) = decoded {
                if decoded.len() > 1 {
                    info!("Animated image support not enabled");
                    Err(DecodeImageError::Unsupported)
                } else {
                    Ok(decoded)
                }
            } else {
                decoded
            }
        }
        None => {
            warn!("Unable to detect format");
            Err(DecodeImageError::Unsupported)
        }
    }
}

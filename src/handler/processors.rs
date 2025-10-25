use image::DynamicImage;

pub fn shrink_outside(image: DynamicImage, size: u32) -> DynamicImage {
    // image::math::resize_dimensions is not a public function,
    // and we can't call image.thumbnail with fill parameter `true`,
    // so we have to write the entire compare logic here.
    // Luckily, misskey only performs this action with height and width the same.
    let w = image.width();
    let h = image.height();
    if w > size && h > size {
        // need to shrink

        // init target sizes with input as default
        let mut w2 = size;
        let mut h2 = size;

        // check which side needs expansion
        if w > h {
            w2 = (f64::from(size) * f64::from(w) / f64::from(h)).round() as u32;
        } else {
            h2 = (f64::from(size) * f64::from(h) / f64::from(w)).round() as u32;
        }

        // Do the shrinking
        image.thumbnail_exact(w2, h2)
    } else {
        // keep as-is
        image
    }
}

#[inline]
pub fn shrink_inside(image: DynamicImage, width: u32, height: u32) -> DynamicImage {
    if image.width() > width || image.height() > height {
        image.thumbnail(width, height)
    } else {
        image // keep as-is
    }
}

#[inline]
pub fn shrink_outside_vec(images: Vec<DynamicImage>, size: u32) -> Vec<DynamicImage> {
    images.into_iter().map(|img| shrink_outside(img, size)).collect()
}

#[inline]
pub fn shrink_inside_vec(images: Vec<DynamicImage>, width: u32, height: u32) -> Vec<DynamicImage> {
    images.into_iter().map(|img| shrink_inside(img, width, height)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shrink_inside_skip() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::new(18, 18));
        let image = shrink_inside(image, 20, 20);
        assert_eq!(image.width(), 18);
        assert_eq!(image.height(), 18);
    }

    #[test]
    fn test_shrink_inside_resize() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::new(18, 9));
        let image = shrink_inside(image, 10, 10);
        assert_eq!(image.width(), 10);
        assert_eq!(image.height(), 5);
    }

    #[test]
    fn test_shrink_outside_skip() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::new(18, 9));
        let image = shrink_outside(image, 10);
        assert_eq!(image.width(), 18);
        assert_eq!(image.height(), 9);
    }

    #[test]
    fn test_shrink_outside_resize() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::new(24, 12));
        let image = shrink_outside(image, 10);
        assert_eq!(image.width(), 20);
        assert_eq!(image.height(), 10);
    }

}

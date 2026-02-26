use crate::handler::ProxyImageResult;
use bytes::Bytes;
use image::codecs::gif::GifEncoder;
use image::{Delay, DynamicImage, Frame, ImageFormat};
use std::io::Cursor;
use tracing::error;

#[cfg(feature = "anim")]
use image::GenericImageView;
#[cfg(feature = "anim")]
use std::io::Write;
#[cfg(feature = "anim")]
use webp_animation::WebPData;

#[inline]
fn images_to_frames(images: Vec<(DynamicImage, Delay)>) -> Vec<Frame> {
    images
        .into_iter()
        .map(|img| Frame::from_parts(img.0.to_rgba8(), 0, 0, img.1))
        .collect()
}

#[cfg(feature = "anim")]
fn encode_webp(images: Vec<(DynamicImage, Delay)>) -> Result<WebPData, webp_animation::Error> {
    let dimensions = images[0].0.dimensions();
    let frames = images_to_frames(images);

    let mut encoder = webp_animation::Encoder::new_with_options(
        dimensions,
        webp_animation::EncoderOptions {
            anim_params: webp_animation::AnimParams { loop_count: 0 },
            allow_mixed: true,
            encoding_config: Some(webp_animation::EncodingConfig {
                encoding_type: webp_animation::EncodingType::Lossy(
                    webp_animation::LossyEncodingConfig {
                        alpha_quality: 95,
                        ..Default::default()
                    },
                ),
                quality: 77f32,
                method: 2,
                ..Default::default()
            }),
            ..Default::default()
        },
    )?;

    let mut current_ts = 0;
    for frame in frames {
        // Encode one frame
        encoder.add_frame(&frame.buffer(), current_ts)?;

        // Calc the duration (delay)
        let frame_delay_tuple = frame.delay().numer_denom_ms();
        let frame_delay = (frame_delay_tuple.0 / frame_delay_tuple.1) as i32;
        current_ts += frame_delay;
    }

    encoder.finalize(current_ts)
}

pub fn encode_image(
    images: Vec<(DynamicImage, Delay)>,
    target_format: ImageFormat,
    original_filename: (String, Option<String>),
) -> Result<ProxyImageResult, ()> {
    let mut bytes: Vec<u8> = Vec::new();

    #[cfg(feature = "anim")]
    let mut buffer = Cursor::new(&mut bytes);

    #[cfg(not(feature = "anim"))]
    let buffer = Cursor::new(&mut bytes);

    match target_format {
        #[cfg(feature = "anim")]
        ImageFormat::WebP => {
            let webp_data = encode_webp(images).map_err(|err| {
                error!("Failed to encode webp image: {err}");
            })?;
            buffer
                .write_all(&webp_data)
                .map_err(|err| error!("Failed to write encoded bytes: {err}"))
        }
        ImageFormat::Gif => GifEncoder::new(buffer)
            .encode_frames(images_to_frames(images))
            .map_err(|err| error!("Failed to encode image: {err}")),
        // Others: non-dynamic, just process as static images
        _ => images[0]
            .0
            .write_to(buffer, target_format)
            .map_err(|err| error!("Failed to encode image: {err}")),
    }?;

    // Correct filename with target extension
    let mut filename = original_filename;
    let target_extension = &format!(".{}", target_format.extensions_str()[0]);
    if !filename.0.ends_with(target_extension) {
        filename.0 = format!("{}{target_extension}", filename.0);
    }
    if let Some(filename_encoded) = &filename.1 {
        if !filename_encoded.ends_with(target_extension) {
            filename.1 = Some(format!("{}{target_extension}", filename_encoded));
        }
    }

    // Return with encoded bytes
    Ok(ProxyImageResult {
        bytes: Bytes::from(bytes),
        content_type: target_format.to_mime_type().to_string(),
        filename,
    })
}

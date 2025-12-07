mod decode;
mod download;
mod processors;

use crate::downloader::{DownloadedFile, Downloader};
use bytes::Bytes;
use download::DownloadImageError;
use http::StatusCode;
use image::codecs::gif::GifEncoder;
use image::{Frame, GenericImageView, ImageFormat};
use processors::{shrink_inside_vec, shrink_outside_vec};
use std::collections::HashMap;
use std::default::Default;
use std::ffi::OsStr;
use std::io::{Cursor, Write};
use std::path::Path;
use tracing::error;

pub struct BytesAndMime(pub Bytes, pub String); // content bytes & content type

pub enum ProxyImageError {
    StatusCodeOnly(StatusCode),
    Redirectable(String),
    BytesOnly(DownloadedFile),
}

pub async fn proxy_image(
    downloader: &Downloader,
    path: &str,
    query: HashMap<String, String>,
    ua: Option<&str>,
) -> Result<BytesAndMime, ProxyImageError> {
    // Note: these logics come from
    // https://github.com/misskey-dev/misskey/blob/56cc89b/packages/backend/src/server/FileServerService.ts#L293-L479
    // Some of them have been modified to fit our needs.

    /**********************************/
    /* Step 1: Download initial image */
    /**********************************/
    let mut downloaded_image =
        match download::download_image(downloader, query.get("url"), query.get("host"), ua).await {
            Ok(value) => value,
            Err(err) => {
                return Err(match err {
                    DownloadImageError::MissingURL | DownloadImageError::MissingUA => {
                        ProxyImageError::StatusCodeOnly(StatusCode::BAD_REQUEST)
                    }
                    DownloadImageError::RecursiveProxy => {
                        ProxyImageError::StatusCodeOnly(StatusCode::FORBIDDEN)
                    }
                    DownloadImageError::DownloadErrorOversize(url) => {
                        ProxyImageError::Redirectable(url.to_string())
                    }
                    DownloadImageError::DownloadErrorInvalidStatus(status_code) => {
                        ProxyImageError::StatusCodeOnly(status_code)
                    }
                    DownloadImageError::DownloadErrorRequest => {
                        ProxyImageError::StatusCodeOnly(StatusCode::INTERNAL_SERVER_ERROR)
                    }
                    DownloadImageError::NotAnImage(file)
                    | DownloadImageError::DecodeError(file) => ProxyImageError::BytesOnly(file),
                });
            }
        };

    /******************************************/
    /* Step 2: Process the image as requested */
    /******************************************/

    // Check target format
    let target_format = if path.len() > 1 {
        // exclude the leading slash
        ImageFormat::from_extension(
            Path::new(path)
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or(""),
        )
        .unwrap_or(ImageFormat::WebP)
    } else {
        ImageFormat::WebP // No target format specified, use webp as default
    };

    // Manipulate image (this may change the target format)
    if query.contains_key("emoji") || query.contains_key("avatar") {
        let target_size = if query.contains_key("emoji") {
            128
        } else {
            320
        };
        // Only shrink, not enlarge
        downloaded_image = shrink_outside_vec(downloaded_image, target_size);
        if query.contains_key("static") {
            // Prevent animation by only keep the first frame
            downloaded_image.truncate(1);
        }
    } else if query.contains_key("static") {
        downloaded_image = shrink_inside_vec(downloaded_image, 498, 422);
    } else if query.contains_key("preview") {
        downloaded_image = shrink_inside_vec(downloaded_image, 200, 200);
    } else if query.contains_key("badge") {
        // Here's the thing: I'm not sure what this function is for,
        // and neither can I implement this easily as many advanced operations
        // (resize with position fit, normalize, flatten, b-w color space, entropy calc)
        // are involved.
        // I've tried to let AI to implement, but the result turned out to be not good enough.
        // This should mean something, but looks not that important for now.
        // So I'll leave a wrong result here to see if something really breaks.
        // todo: implement as https://github.com/misskey-dev/misskey/blob/56cc89b/packages/backend/src/server/FileServerService.ts#L386-L415
        return Err(ProxyImageError::StatusCodeOnly(StatusCode::NOT_IMPLEMENTED));
    };

    // image crate can't process SVG files here,
    // and it should be returned as-is when decoding fails above.
    // Rejected type also provided unchanged (I guess).

    // Encode image using target format
    let mut bytes: Vec<u8> = Vec::new();
    let mut buffer = Cursor::new(&mut bytes);
    let first_frame = downloaded_image[0].0.clone(); // todo: find a better way
    let frames: Vec<Frame> = downloaded_image
        .into_iter()
        .map(|img| Frame::from_parts(img.0.to_rgba8(), 0, 0, img.1))
        .collect();

    if let Err(err) = match target_format {
        ImageFormat::WebP => {
            let mut encoder = webp_animation::Encoder::new_with_options(
                first_frame.dimensions(),
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
            )
            .unwrap();

            let mut current_ts = 0;
            for frame in frames {
                // Encode one frame
                encoder.add_frame(&frame.buffer(), current_ts).unwrap();

                // Calc the duration (delay)
                let frame_delay_tuple = frame.delay().numer_denom_ms();
                let frame_delay = (frame_delay_tuple.0 / frame_delay_tuple.1) as i32;
                current_ts += frame_delay;
            }

            let webp_data = encoder.finalize(current_ts).unwrap();
            buffer.write_all(&webp_data).unwrap();
            Ok(())
        }
        ImageFormat::Gif => GifEncoder::new(buffer).encode_frames(frames),
        _ => first_frame.write_to(buffer, target_format),
    } {
        // Image encoder failed
        error!("Failed to encode image: {err}");
        return Err(ProxyImageError::StatusCodeOnly(
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    } // else: nothing happens

    // Return with encoded bytes
    Ok(BytesAndMime(
        Bytes::from(bytes),
        target_format.to_mime_type().to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_process_webp() {
        let downloader = Downloader::new(None);
        let query = HashMap::from([
            ("emoji".to_string(), "1".to_string()),
            (
                "url".to_string(),
                "https://sh.nfs.pub/nyaone/7006d5af-fe08-4f50-93ef-0aabd1ec155b.webp".to_string(),
            ),
        ]);
        let file = proxy_image(&downloader, "image.webp", query, Some("MediaProxyRS@Debug")).await;
        assert!(file.is_ok());
        if let Ok(BytesAndMime(bytes, ct)) = file {
            assert!(bytes.len() > 0);
            assert_eq!(ct, "image/webp".to_string())
        }
    }

    #[tokio::test]
    async fn test_process_gif() {
        let downloader = Downloader::new(None);
        let query = HashMap::from([
            ("emoji".to_string(), "1".to_string()),
            (
                "url".to_string(),
                "https://sh.nfs.pub/nyaone/d35b447f-0bfe-4383-97a2-c878557efd90.gif".to_string(),
            ),
        ]);
        let file = proxy_image(&downloader, "image.webp", query, Some("MediaProxyRS@Debug")).await;
        assert!(file.is_ok());
        if let Ok(BytesAndMime(bytes, ct)) = file {
            assert!(bytes.len() > 0);
            assert_eq!(ct, "image/webp".to_string())
        }
    }
}

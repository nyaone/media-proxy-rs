
mod processors;
mod utils;

use std::default::Default;
use image::{ImageReader, ImageDecoder, Frame, DynamicImage, ImageFormat, AnimationDecoder};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Cursor;
use std::path::Path;
use http_body_util::{Full};
use bytes::Bytes;
use hyper::{Request, Response};
use url::form_urlencoded;
use hyper::StatusCode;
use http_body_util::{combinators::BoxBody, BodyExt};
use image::codecs::gif::{GifDecoder, GifEncoder};
use image::codecs::png::{PngDecoder};
use image::codecs::webp::{WebPDecoder};
use tracing::{info, warn, error};
use crate::downloader::{Downloader, FileDownloadError};
use processors::{shrink_inside_vec, shrink_outside_vec};
use utils::{empty, full, response_raw};

enum DecodeImageError {
    Unsupported,
    ImageError(image::ImageError),
}

fn static_image(ori: Result<image::metadata::Orientation, image::ImageError>, mut img: DynamicImage) -> Result<Vec<DynamicImage>, image::ImageError> {
    if let Ok(ori) = ori {
        img.apply_orientation(ori);
    }
    Ok(vec![img])
}

fn frames_to_images(ori: Result<image::metadata::Orientation, image::ImageError>, frames: Vec<Frame>) -> Vec<DynamicImage> {
    let mut images: Vec<DynamicImage> = Vec::new();
    for frame in frames {
        let mut img = DynamicImage::from(frame.into_buffer());
        if let Ok(ori) = ori {
            img.apply_orientation(ori);
        }
        images.push(img);
    }
    images
}

// Inspired by https://github.com/image-rs/image/issues/2360#issuecomment-3092626301
fn decode_image_format(img_reader: ImageReader<Cursor<&Bytes>>, format: ImageFormat) -> Result<Vec<DynamicImage>, image::ImageError> {
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
fn decode_image(url: &String, downloaded_bytes: &Bytes) -> Result<Vec<DynamicImage>, DecodeImageError> {
    // Check whether the file is an image (don't trust the content-type header or filename)
    // hint: misskey need to detect whether the file is manipulatable manually,
    // but here we are using image crate's format guessing feature
    let img_reader = ImageReader::new(Cursor::new(downloaded_bytes)).with_guessed_format().unwrap();

    match img_reader.format() {
        Some(format) => decode_image_format(img_reader, format).map_err(DecodeImageError::ImageError),
        None => {
            info!("Unable to detect format of {url}");
            Err(DecodeImageError::Unsupported)
        }
    }
}

async fn download_image(downloader: &Downloader, url: Option<&String>, host: Option<&String>, ua: Option<&str>) -> Result<Vec<DynamicImage>, Response<BoxBody<Bytes, hyper::Error>>> {
    // Check if url parameter is specified
    if url.is_none() {
        // Missing url
        warn!("Request missing url");
        let mut response = Response::new(empty());
        *response.status_mut() = StatusCode::BAD_REQUEST;
        return Err(response);
    }

    // Check if UserAgent is valid
    if ua.is_none() {
        // Missing UserAgent
        warn!("Request missing UserAgent");
        let mut response = Response::new(full("User-Agent is required"));
        *response.status_mut() = StatusCode::BAD_REQUEST;
        return Err(response);
    }

    let ua = ua.unwrap(); // Shadow the parameter value
    if ua.to_lowercase().contains("misskey/") {
        // Recursive proxying
        warn!("Recursive proxying from {ua}");
        let mut response = Response::new(full("Refusing to proxy a request from another proxy"));
        *response.status_mut() = StatusCode::FORBIDDEN;
        return Err(response);
    }

    // Start download
    let url = url.unwrap();
    let downloaded_file = match downloader.download_file(url, host, ua).await {
        Ok(b) => b,
        Err(e) => {
            let mut response = Response::new(empty());
            match e {
                FileDownloadError::Oversize => {
                    // too large to process, redirect instead
                    warn!("File too large: {url}");
                    *response.status_mut() = StatusCode::FOUND;
                    response.headers_mut().insert("Location", url.parse().unwrap());
                }
                FileDownloadError::InvalidStatusCode(status_code) => {
                    warn!("Invalid status code: {url}, {status_code}");
                    *response.status_mut() = status_code; // inherit status code
                    // should we pass the exact same body from remote server?
                    // note: misskey will return the dummy.png if the status code is 404, but we don't implement that feature here
                }
                FileDownloadError::RequestError(err) => {
                    // request failed, return 500
                    error!("Failed to download file: {url}, {err}");
                    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                }
            }
            return Err(response);
        }
    };

    // Check possible mimetype of downloaded file
    if let Some(ct) = downloaded_file.1.as_ref() {
        if !ct.starts_with("image/") {
            // Not image, return raw bytes
            warn!("Not an image ({ct}): {url}");
            return Err(response_raw(downloaded_file));
        }
    }

    // Decode image
    match decode_image(url, &downloaded_file.0) {
        Ok(decoded_image) => Ok(decoded_image),
        Err(err) => {
            if let DecodeImageError::ImageError(err) = err {
                error!("Failed to decode image: {url}, {err}");
            } // else is unsupported, which has already been reported
            Err(response_raw(downloaded_file))
        }
    }
}

async fn proxy_image(downloader: &Downloader, path: &str, query: HashMap<String, String>, ua: Option<&str>) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    // Note: these logics come from
    // https://github.com/misskey-dev/misskey/blob/56cc89b/packages/backend/src/server/FileServerService.ts#L293-L479
    // Some of them have been modified to fit our needs.

    /**********************************/
    /* Step 1: Download initial image */
    /**********************************/
    let mut downloaded_image = match download_image(downloader, query.get("url"), query.get("host"), ua).await {
        Ok(value) => value,
        Err(value) => return Ok(value),
    };

    /******************************************/
    /* Step 2: Process the image as requested */
    /******************************************/

    // Check target format
    let target_format = if path.len() > 1 { // exclude the leading slash
        ImageFormat::from_extension(Path::new(path).extension().and_then(OsStr::to_str).unwrap_or("")).unwrap_or(ImageFormat::WebP)
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
        let mut response = Response::new(empty());
        *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
        return Ok(response);
    };

    // image crate can't process SVG files here,
    // and it should be returned as-is when decoding fails above.
    // Rejected type also provided unchanged (I guess).

    // Encode image using target format
    let mut bytes: Vec<u8> = Vec::new();
    let buffer = Cursor::new(&mut bytes);
    let frames: Vec<Frame> = downloaded_image.into_iter().map(|img| Frame::new(img.to_rgba8())).collect();

    if let Err(err) = match target_format {
        ImageFormat::WebP => {
            let encoder = webp_animation::Encoder::new_with_options(
                (downloaded_image[0].width(), downloaded_image[0].height()),
                webp_animation::EncoderOptions{
                    anim_params: webp_animation::AnimParams {
                        loop_count: 0,
                    },
                    allow_mixed: true,
                    encoding_config: Some(webp_animation::EncodingConfig{
                        encoding_type: webp_animation::EncodingType::Lossy(
                            webp_animation::LossyEncodingConfig {
                                alpha_quality: 95,
                                ..Default::default()
                            }
                        ),
                        quality: 77f32,
                        method: 2,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ).map_err(|e| never)?;
            
        },
        ImageFormat::Gif => GifEncoder::new(buffer).encode_frames(frames),
        _ => downloaded_image[0].write_to(buffer, target_format),
    } {
        // Image encoder failed
        error!("Failed to encode image: {err}");
        let mut response = Response::new(empty());
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        return Ok(response);
    } // else: nothing happens

    // Return with encoded bytes
    let mut response = Response::new(
        Full::new(Bytes::from(bytes))
            .map_err(|never| match never {}).
            boxed()
    );
    response.headers_mut().insert(http::header::CONTENT_TYPE, target_format.to_mime_type().parse().unwrap());
    response.headers_mut().insert(http::header::CACHE_CONTROL, "max-age=31536000, immutable".parse().unwrap());
    // response.headers_mut().insert(http::header::CONTENT_DISPOSITION, "inline".parse().unwrap()); // not sure whether this is needed
    Ok(response)
}

pub async fn handle(downloader: &Downloader, req: Request<hyper::body::Incoming>) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let uri = req.uri();
    match uri.query() {
        None => Ok(Response::new(full("OK"))), // healthcheck
        Some(query) => proxy_image(
            downloader,
            uri.path(),
            form_urlencoded::parse(query.as_bytes()).into_owned().collect(),
            req.headers().get(http::header::USER_AGENT).map(|ua| ua.to_str().unwrap())
        ).await,
    }
}

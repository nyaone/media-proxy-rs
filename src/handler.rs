use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Cursor;
use std::path::Path;
use http_body_util::{Empty, Full};
use bytes::Bytes;
use hyper::{Request, Response};
use url::form_urlencoded;
use hyper::StatusCode;
use http_body_util::{combinators::BoxBody, BodyExt};
use tracing::{error, warn};

use crate::downloader::{Downloader, FileDownloadError};

// We create some utility functions to make Empty and Full bodies
// fit our broadened Response body type.
fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}
fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

fn response_raw((bytes, ct): (Bytes, Option<String>)) -> Response<BoxBody<Bytes, hyper::Error>> {
    let mut response = Response::new(
        Full::new(bytes)
        .map_err(|never| match never {}).
        boxed()
    );
    if let Some(ct) = ct {
        response.headers_mut().insert(http::header::CONTENT_TYPE, ct.parse().unwrap());
    }
    response
}

enum DecodeImageError {
    Unsupported,
    ImageError(image::ImageError),
}
fn decode_image(url: &String, downloaded_bytes: &Bytes) -> Result<DynamicImage, DecodeImageError> {
    // Check whether the file is an image (don't trust the content-type header or filename)
    // hint: misskey need to detect whether the file is manipulatable manually,
    // but here we are using image crate's format guessing feature
    let img_reader = ImageReader::new(Cursor::new(downloaded_bytes)).with_guessed_format().unwrap();

    // Check if is an unsupported format (like animated ones, usually gif)
    if let Some(format) = img_reader.format() {
        if format == ImageFormat::Gif {
            // Unable to process for now // todo: find a way to handle this properly
            warn!("Unable to process gif for now, provide as-is: {url}");
            return Err(DecodeImageError::Unsupported);
        }
    }

    let mut img_decoder = img_reader.into_decoder().map_err(DecodeImageError::ImageError)?;

    let ori = img_decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut downloaded_image = DynamicImage::from_decoder(img_decoder).map_err(DecodeImageError::ImageError)?;
    downloaded_image.apply_orientation(ori);
    Ok(downloaded_image)
}

async fn download_image(downloader: &Downloader, url: Option<&String>, host: Option<&String>, ua: Option<&str>) -> Result<DynamicImage, Response<BoxBody<Bytes, hyper::Error>>> {
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

fn shrink_outside(image: DynamicImage, size: u32) -> DynamicImage {
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
fn shrink_inside(image: DynamicImage, width: u32, height: u32) -> DynamicImage {
    if image.width() > width || image.height() > height {
        image.thumbnail(width, height)
    } else {
        image // keep as-is
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
    let mut target_format = if path.len() > 1 { // exclude the leading slash
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
        downloaded_image = shrink_outside(downloaded_image, target_size);
        if query.contains_key("static") {
            // Prevent animation by only keep the first frame
            // I actually made it wrong 😅
            // The library will by default keep only static image,
            // I'll need to find out how to server dynamic images.
            target_format = ImageFormat::WebP;
        }
    } else if query.contains_key("static") {
        downloaded_image = shrink_inside(downloaded_image, 498, 422);
    } else if query.contains_key("preview") {
        downloaded_image = shrink_inside(downloaded_image, 200, 200);
    } else if query.contains_key("badge") {
        // Here's the thing: I'm not sure what this function is for,
        // and neither can I implement this easily as many advanced operations
        // (resize with position fit, normalize, flatten, b-w color space, entropy calc)
        // are involved.
        // This should mean something, but looks not that important for now.
        // So I'll leave a wrong result here to see if something really breaks.
        // todo: implement as https://github.com/misskey-dev/misskey/blob/56cc89b/packages/backend/src/server/FileServerService.ts#L386-L415
        let mut response = Response::new(empty());
        *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
        return Ok(response);
    };

    // image crate can't process SVG files here,
    // and it should be returned as-is when decoding fails above.
    // Rejected type also provided as-is (I guess).

    // Encode image using target format
    let mut bytes: Vec<u8> = Vec::new();
    if let Err(err) = downloaded_image.write_to(&mut Cursor::new(&mut bytes), target_format) {
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

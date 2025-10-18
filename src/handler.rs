use image::{ImageReader, ImageDecoder, DynamicImage, ImageFormat};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Cursor;
use std::path::Path;
use http_body_util::{Empty, Full};
use bytes::Bytes;
use hyper::{Request, Response};
use url::form_urlencoded;
use hyper::{StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt};
use tracing::{warn, error};

mod download_file;
use download_file::{download_file, FileDownloadError};

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

fn response_raw(bytes: Bytes, ct: Option<String>) -> Response<BoxBody<Bytes, hyper::Error>> {
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

async fn download_image(url: Option<&String>, host: Option<&String>, ua: Option<&str>) -> Result<DynamicImage, Response<BoxBody<Bytes, hyper::Error>>> {
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
    let downloaded_file = match download_file(url, host, ua).await {
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

    // Check whether the file is an image (don't trust the content-type header or filename)
    // hint: misskey need to detect whether the file is manipulatable manually,
    // but here we are using image crate's format guessing feature
    let downloaded_image = match ImageReader::new(Cursor::new(downloaded_file.0.as_ref())).with_guessed_format() {
        Ok(img_reader) => {
            // Check if is an unsupported format (like animated ones, usually gif)
            if let Some(format) = img_reader.format() {
                if format == ImageFormat::Gif {
                    // Unable to process for now // todo: find a way to handle this properly
                    warn!("Unable to process gif for now, provide as-is: {url}");
                    return Err(response_raw(downloaded_file.0, downloaded_file.1));
                }
            }

            let mut img_decoder = img_reader.into_decoder().unwrap();
            let ori = img_decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
            match DynamicImage::from_decoder(img_decoder) {
                Ok(mut img) => {
                    // apply the rotation orientation from exif data
                    img.apply_orientation(ori);
                    img
                },
                Err(err) => {
                    // return raw bytes
                    error!("Failed to decode image: {url}, {err}");
                    return Err(response_raw(downloaded_file.0, downloaded_file.1));
                }
            }
        },
        Err(err) => {
            // return raw bytes
            error!("Failed to create image reader: {url}, {err}");
            return Err(response_raw(downloaded_file.0, downloaded_file.1));
        }
    };
    Ok(downloaded_image)
}

fn shrink_image(image: DynamicImage, width: u32, height: u32) -> DynamicImage {
    if image.width() > width || image.height() > height {
        image.thumbnail(width, height)
    } else {
        image // keep as-is
    }
}


async fn proxy_image(path: &str, query: HashMap<String, String>, ua: Option<&str>) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    // Note: these logics come from
    // https://github.com/misskey-dev/misskey/blob/56cc89b/packages/backend/src/server/FileServerService.ts#L293-L479
    // Some of them have been modified to fit our needs.

    /**********************************/
    /* Step 1: Download initial image */
    /**********************************/
    let mut downloaded_image = match download_image(query.get("url"), query.get("host"), ua).await {
        Ok(value) => value,
        Err(value) => return Ok(value),
    };

    /******************************************/
    /* Step 2: Process the image as requested */
    /******************************************/

    // Check target format
    let mut target_format = ImageFormat::from_extension(Path::new(path).extension().and_then(OsStr::to_str).unwrap_or("")).unwrap_or(ImageFormat::WebP);

    // Manipulate image (this may change the target format)
    if query.contains_key("emoji") || query.contains_key("avatar") {
        // Actually, I'm not sure why misskey won't resize the image when not static,
        // so let's change the action to make this behavior looks more proper
        let target_size = if query.contains_key("emoji") {
            128
        } else {
            320
        };
        // Only shrink, not enlarge
        downloaded_image = shrink_image(downloaded_image, target_size, target_size);
        if query.contains_key("static") {
            // Prevent animation by only keep the first frame
            // I actually made it wrong 😅
            // The library will by default keep only static image,
            // I'll need to find out how to server dynamic images.
            target_format = ImageFormat::WebP;
        }
    } else if query.contains_key("static") {
        downloaded_image = shrink_image(downloaded_image, 498, 422);
        // from literary meaning, this operation should also convert to static image,
        // but misskey doesn't do that, so we neither.
    } else if query.contains_key("preview") {
        downloaded_image = shrink_image(downloaded_image, 200, 200);
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

pub async fn handle(req: Request<hyper::body::Incoming>) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let uri = req.uri();
    match uri.path() {
        "/" => Ok(Response::new(full("OK"))), // healthcheck
        proxy_filename => proxy_image(
            &proxy_filename[1..],
            form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).into_owned().collect(),
            req.headers().get(http::header::USER_AGENT).map(|ua| ua.to_str().unwrap())
        ).await,
    }
}

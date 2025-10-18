use std::collections::HashMap;
use std::io::Cursor;
use http_body_util::{Empty, Full};
use bytes::Bytes;
use hyper::{Request, Response};
use url::Url;
use hyper::{StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt};

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

fn response_raw(bytes: Bytes, ct: Option<String>) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let mut response = Response::new(
        Full::new(bytes)
        .map_err(|never| match never {}).
        boxed()
    );
    if let Some(ct) = ct {
        response.headers_mut().insert(http::header::CONTENT_TYPE, ct.parse().unwrap());
    }
    Ok(response)
}

async fn download_image(url: Option<&String>, host: Option<&String>, ua: Option<&str>) -> Result<image::DynamicImage, Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error>> {
    // Check if url parameter is specified
    if url.is_none() {
        // Missing url
        let mut response = Response::new(empty());
        *response.status_mut() = StatusCode::BAD_REQUEST;
        return Err(Ok(response));
    }

    // Check if UserAgent is valid
    if ua.is_none() {
        // Missing UserAgent
        let mut response = Response::new(full("User-Agent is required"));
        *response.status_mut() = StatusCode::BAD_REQUEST;
        return Err(Ok(response));
    }

    let ua = ua.unwrap(); // Shadow the parameter value
    if ua.to_lowercase().contains("misskey/") {
        // Recursive proxying
        let mut response = Response::new(full("Refusing to proxy a request from another proxy"));
        *response.status_mut() = StatusCode::FORBIDDEN;
        return Err(Ok(response));
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
                    *response.status_mut() = StatusCode::FOUND;
                    response.headers_mut().insert("Location", url.parse().unwrap());
                }
                FileDownloadError::InvalidStatusCode(status_code) => {
                    *response.status_mut() = status_code; // inherit status code
                    // todo: should we pass the exact same body from remote server?
                    // note: misskey will return the dummy.png if the status code is 404, but we don't implement that feature here
                }
                FileDownloadError::RequestError(_err) => {
                    // request failed, return 500
                    // todo: log error
                    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                }
            }
            return Err(Ok(response));
        }
    };

    // Check whether the file is an image (don't trust the content-type header or filename)
    // hint: misskey need to detect whether the file is manipulatable manually,
    // but here we are using image crate's format guessing feature
    let downloaded_image = match image::ImageReader::new(Cursor::new(downloaded_file.0.as_ref())).with_guessed_format() {
        Ok(img) => match img.decode() {
            Ok(img) => img,
            Err(_err) => {
                // failed to decode image, return raw bytes
                // todo: log error
                return Err(response_raw(downloaded_file.0, downloaded_file.1)); // todo: can we merge these 2 match clauses?
            }
        },
        Err(_err) => {
            // failed to open image, return raw bytes
            // todo: log error
            return Err(response_raw(downloaded_file.0, downloaded_file.1));
        }
    };
    Ok(downloaded_image)
}

fn shrink_image(image: image::DynamicImage, width: u32, height: u32) -> image::DynamicImage {
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
        Err(value) => return value,
    };

    /******************************************/
    /* Step 2: Process the image as requested */
    /******************************************/

    // Check target format
    let mut target_format = if path.ends_with(".png") {
        image::ImageFormat::Png
    } else {
        image::ImageFormat::WebP // Use as a fallback as webp has been supported widely
    };

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
            // Not sure how to use this library correctly
            // todo
            target_format = image::ImageFormat::WebP;
        }
    } else if query.contains_key("static") {
        downloaded_image = shrink_image(downloaded_image, 498, 422);
        // from literary meaning, this operation should also convert to static image,
        // but misskey doesn't do that, so we neither.
        // todo: Should also auto rotate, but don't know how to implement this
    } else if query.contains_key("preview") {
        downloaded_image = shrink_image(downloaded_image, 200, 200);
        // todo: Should also auto rotate, but don't know how to implement this
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
    if let Err(_err) = downloaded_image.write_to(&mut Cursor::new(&mut bytes), target_format) {
        // Image encoder failed
        // todo: log error
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
            // uri is a relative path, which cannot be parsed directly
            Url::parse(&format!("https://nya.one{}", uri))
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect(),
            req.headers().get(http::header::USER_AGENT).map(|ua| ua.to_str().unwrap())
        ).await,
    }
}

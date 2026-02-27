mod decode;
mod download;
mod encode;
mod processors;

use crate::downloader::{DownloadedFile, Downloader};
use crate::handler::decode::DecodeImageError;
use bytes::Bytes;
use download::DownloadImageError;
use http::StatusCode;
use image::ImageFormat;
use processors::{shrink_inside_vec, shrink_outside_vec};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use tracing::error;

pub struct ProxyImageResult {
    pub bytes: Bytes,
    pub content_type: String,
    pub filename: (String, Option<String>),
}

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
) -> Result<ProxyImageResult, ProxyImageError> {
    // Note: these logics come from
    // https://github.com/misskey-dev/misskey/blob/56cc89b/packages/backend/src/server/FileServerService.ts#L293-L479
    // Some of them have been modified to fit our needs.

    /**********************************/
    /* Step 1: Download initial image */
    /**********************************/
    let downloaded_file =
        download::download_image(downloader, query.get("url"), query.get("host"), ua)
            .await
            .map_err(|err| match err {
                DownloadImageError::MissingURL => {
                    ProxyImageError::StatusCodeOnly(StatusCode::BAD_REQUEST)
                }
                DownloadImageError::RecursiveProxy => {
                    ProxyImageError::StatusCodeOnly(StatusCode::FORBIDDEN)
                }
                DownloadImageError::DownloadErrorOversize(url) => {
                    ProxyImageError::Redirectable(url.to_string())
                }
                DownloadImageError::DownloadErrorInvalidUrl => {
                    ProxyImageError::StatusCodeOnly(StatusCode::BAD_REQUEST)
                }
                DownloadImageError::DownloadErrorInvalidStatus(status_code) => {
                    ProxyImageError::StatusCodeOnly(status_code)
                }
                DownloadImageError::DownloadErrorRequest => {
                    ProxyImageError::StatusCodeOnly(StatusCode::INTERNAL_SERVER_ERROR)
                }
                DownloadImageError::NotAnImage(file) => ProxyImageError::BytesOnly(file),
            })?;

    /******************************************/
    /* Step 2: Decode the downloaded image    */
    /******************************************/
    let mut downloaded_image = match decode::decode_image(&downloaded_file.bytes) {
        Ok(image) => image,
        Err(err) => {
            if let DecodeImageError::ImageError(err) = err {
                error!("Failed to decode image: {err}");
            } // else is unsupported, which has already been reported
            return Err(ProxyImageError::BytesOnly(downloaded_file));
        }
    };

    /******************************************/
    /* Step 3: Process the image as requested */
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

    /******************************************/
    /* Step 4: Encode into target format      */
    /******************************************/
    encode::encode_image(downloaded_image, target_format, downloaded_file.filename).map_err(|_| {
        ProxyImageError::BytesOnly(DownloadedFile {
            filename: ("".to_string(), None), // Omit
            ..downloaded_file
        })
    })
}

// ============================================================================
// HTTP Request Handler - exported for use in listener module
// ============================================================================

use http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, LOCATION, USER_AGENT};
use http::{Request, Response};
use http_body_util::{BodyExt, combinators::BoxBody, Empty, Full};
use url::form_urlencoded;

#[allow(dead_code)]
#[inline]
fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

#[allow(dead_code)]
#[inline]
fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

#[allow(dead_code)]
#[inline]
fn response_raw(
    (bytes, ct): (Bytes, Option<String>),
) -> Response<BoxBody<Bytes, hyper::Error>> {
    let mut response = Response::new(full(bytes));
    if let Some(ct) = ct {
        response
            .headers_mut()
            .insert(CONTENT_TYPE, ct.parse().unwrap());
    }
    response
}

#[allow(dead_code)]
pub async fn handle(
    downloader: &Downloader,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let uri = req.uri();
    match uri.query() {
        None => Ok(Response::new(full("OK"))), // healthcheck
        Some(query) => Ok(
            match proxy_image(
                downloader,
                uri.path(),
                form_urlencoded::parse(query.as_bytes())
                    .into_owned()
                    .collect(),
                req.headers().get(USER_AGENT).map(|ua| ua.to_str().unwrap()),
            )
            .await
            {
                Ok(file) => {
                    let mut response = response_raw((file.bytes, Some(file.content_type)));
                    response.headers_mut().insert(
                        CACHE_CONTROL,
                        "max-age=31536000, immutable".parse().unwrap(),
                    );

                    let mut content_disposition =
                        format!("inline; filename=\"{}\"", file.filename.0);
                    if let Some(filename_encoded) = file.filename.1 {
                        content_disposition =
                            format!("{content_disposition}; filename*={filename_encoded}");
                    }
                    response
                        .headers_mut()
                        .insert(CONTENT_DISPOSITION, content_disposition.parse().unwrap());
                    response
                }
                Err(err) => match err {
                    ProxyImageError::StatusCodeOnly(status_code) => {
                        let mut response = Response::new(empty());
                        *response.status_mut() = status_code;
                        response
                    }
                    ProxyImageError::Redirectable(url) => {
                        let mut response = Response::new(empty());
                        *response.status_mut() = StatusCode::FOUND;
                        response
                            .headers_mut()
                            .insert(LOCATION, url.parse().unwrap());
                        response
                    }
                    ProxyImageError::BytesOnly(file) => {
                        response_raw((file.bytes, file.content_type))
                    }
                },
            },
        ),
    }
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
                "https://public.nyaone-object-storage.com/nyaone/7006d5af-fe08-4f50-93ef-0aabd1ec155b.webp".to_string(),
            ),
        ]);
        let file = proxy_image(&downloader, "image.webp", query, Some("MediaProxyRS@Debug")).await;
        assert!(file.is_ok());
        if let Ok(image) = file {
            assert!(image.bytes.len() > 0);
            assert_eq!(image.content_type, "image/webp".to_string());
            assert_eq!(
                image.filename,
                ("LovelyFirefly_7.png.webp".to_string(), None)
            );
        }
    }

    #[tokio::test]
    async fn test_process_gif() {
        let downloader = Downloader::new(None);
        let query = HashMap::from([
            ("emoji".to_string(), "1".to_string()),
            (
                "url".to_string(),
                "https://public.nyaone-object-storage.com/nyaone/d35b447f-0bfe-4383-97a2-c878557efd90.gif".to_string(),
            ),
        ]);
        let file = proxy_image(&downloader, "image.webp", query, Some("MediaProxyRS@Debug")).await;
        assert!(file.is_ok());
        if let Ok(image) = file {
            assert!(image.bytes.len() > 0);
            assert_eq!(image.content_type, "image/webp".to_string());
            assert_eq!(image.filename, ("yuexia_shy.gif.webp".to_string(), None));
        }
    }
}

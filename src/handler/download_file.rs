use bytes::Bytes;
use reqwest::header::{HeaderMap, USER_AGENT, REFERER, CONTENT_TYPE, CONTENT_LENGTH};
use reqwest::{Client, StatusCode};
use tracing::{debug, warn};

pub enum FileDownloadError {
    Oversize,
    InvalidStatusCode(StatusCode),
    RequestError(reqwest::Error),
}

fn prepare_client(ua: &str) -> Result<Client, reqwest::Error> {
    // Prepare the request client
    let mut default_headers = HeaderMap::new();
    default_headers.insert(USER_AGENT, ua.parse().unwrap());
    let client = Client::builder().default_headers(default_headers).build()?;
    Ok(client)
}

const SIZE_LIMIT: u64 = 100_000_000; // 100MB // todo: make this configurable

pub async fn download_file(url: &str, host: Option<&String>, ua: &str) -> Result<(Bytes, Option<String>), FileDownloadError> {
    debug!("Downloading file: {url} with UserAgent: {ua}");

    let client = prepare_client(&ua).map_err(|e| FileDownloadError::RequestError(e))?;

    // First try: direct download
    let mut resp = client.get(url).send().await.map_err(|e| FileDownloadError::RequestError(e))?;

    // if is 4xx error (e.g., 403 for hotlink protect), retry with host specified
    if resp.status().is_client_error() {
        warn!("Direct download failed {} {}, retrying with host specified", resp.status(), url);
        if let Some(host) = host {
            let mut additional_headers = HeaderMap::new();
            additional_headers.insert(REFERER, host.parse().unwrap());

            resp = client.get(url).headers(additional_headers).send().await.map_err(|e| FileDownloadError::RequestError(e))?;
        }
    }

    // Check status code
    if !resp.status().is_success() || resp.status() == StatusCode::NO_CONTENT {
        return Err(FileDownloadError::InvalidStatusCode(resp.status()));
    }

    // Check response size (content length)
    if let Some(size) = resp.content_length() {
        if size > SIZE_LIMIT {
            return Err(FileDownloadError::Oversize);
        }
    } else if let Some(size_length) = resp.headers().get(CONTENT_LENGTH) {
        if let Ok(size) = size_length.to_str().unwrap().parse::<u64>() {
            if size > SIZE_LIMIT {
                return Err(FileDownloadError::Oversize);
            }
        }
    }

    // Nothing wrong, let's download the entire response body and return
    let ct = resp.headers().get(CONTENT_TYPE).map(|ct| ct.to_str().unwrap().to_string());
    Ok((resp.bytes().await.map_err(|e| FileDownloadError::RequestError(e))?, ct))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_download_file() {
        let file = download_file("https://sh.nfs.pub/nyaone/ff02042e-524e-48e8-bb27-17621d96b13a.png", None, "MediaProxyRS@Debug").await;
        assert!(file.is_ok());
        if let Ok((bytes, ct)) = file {
            assert!(bytes.len() > 0);
            assert_eq!(ct, Some(
                "image/png".to_string()
            ))
        }
    }
}

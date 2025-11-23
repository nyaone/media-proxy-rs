use futures_util::stream::StreamExt;
use bytes::Bytes;
use reqwest::header::{HeaderMap, USER_AGENT, REFERER, CONTENT_TYPE, CONTENT_LENGTH};
use reqwest::{Client, StatusCode};
use tracing::{debug, info, warn};

pub enum FileDownloadError {
    Oversize,
    InvalidStatusCode(StatusCode),
    RequestError(reqwest::Error),
}

const DEFAULT_SIZE_LIMIT: u64 = 100_000_000; // 100MB

pub struct Downloader {
    client: Client,
    size_limit: u64,
}

impl Clone for Downloader {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            size_limit: self.size_limit,
        }
    }
}

impl Downloader {
    pub fn new() -> Self {
        // Read size limit from env
        let size_limit = match std::env::var("SIZE_LIMIT") {
            Ok(size_limit_str) => match size_limit_str.parse::<u64>() {
                Ok(size_limit) => {
                    info!("Size limit set to {size_limit}");
                    size_limit
                },
                Err(err) => {
                    warn!("Failed to parse size limit {size_limit_str}: {err}, fallback to default");
                    DEFAULT_SIZE_LIMIT
                }
            },
            Err(err) => {
                if err == std::env::VarError::NotPresent {
                    info!("Size limit not set, using default");
                } else {
                    warn!("Failed to read size limit from env: {err}, fallback to default");
                }
                DEFAULT_SIZE_LIMIT
            },
        };

        Self {
            client: Client::new(),
            size_limit,
        }
    }

    pub async fn download_file(&self, url: &str, host: Option<&String>, ua: &str) -> Result<(Bytes, Option<String>), FileDownloadError> {
        debug!("Downloading file: {url}, Host: {host:?}, UserAgent: {ua}");

        let mut default_headers = HeaderMap::new();
        default_headers.insert(USER_AGENT, ua.parse().unwrap());

        // First try: direct download
        debug!("Trying direct download...");
        let mut resp = self.client.get(url).headers(default_headers).send().await.map_err(FileDownloadError::RequestError)?;

        // if is 4xx error (e.g., 403 for hotlink protect), retry with host specified
        if resp.status().is_client_error() {
            debug!("Direct download failed {} {}, retrying with host specified", resp.status(), url);
            if let Some(host) = host {
                let mut additional_headers = HeaderMap::new();
                additional_headers.insert(USER_AGENT, ua.parse().unwrap());
                additional_headers.insert(REFERER, host.parse().unwrap());

                resp = self.client.get(url).headers(additional_headers).send().await.map_err(FileDownloadError::RequestError)?;
            }
        }

        // Check status code
        debug!("Download finish, checking status code...");
        let resp_status = resp.status();
        if !resp_status.is_success() || resp_status == StatusCode::NO_CONTENT {
            return Err(FileDownloadError::InvalidStatusCode(resp_status));
        }

        // Check response size (content length)
        debug!("Status OK, checking content length (if any)...");
        if let Some(size) = resp.content_length() {
            if size > self.size_limit {
                return Err(FileDownloadError::Oversize);
            }
        } else if let Some(size_length) = resp.headers().get(CONTENT_LENGTH) {
            if let Ok(size) = size_length.to_str().unwrap().parse::<u64>() {
                if size > self.size_limit {
                    return Err(FileDownloadError::Oversize);
                }
            }
        }

        // Nothing wrong, let's download the entire response body and return
        debug!("Length pre-check OK, downloading entire body...");
        let ct = resp.headers().get(CONTENT_TYPE).map(|ct| ct.to_str().unwrap().to_string());
        let mut limited_buf = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            limited_buf.extend(chunk.map_err(FileDownloadError::RequestError)?);
            if limited_buf.len() as u64 > self.size_limit {
                return Err(FileDownloadError::Oversize);
            }
        }

        debug!("Response body downloaded, return. ContentType: {ct:?}");
        Ok((Bytes::from(limited_buf), ct))
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_download_file() {
        let downloader = Downloader::new();
        let file = downloader.download_file("https://sh.nfs.pub/nyaone/ff02042e-524e-48e8-bb27-17621d96b13a.png", None, "MediaProxyRS@Debug").await;
        assert!(file.is_ok());
        if let Ok((bytes, ct)) = file {
            assert!(bytes.len() > 0);
            assert_eq!(ct, Some(
                "image/png".to_string()
            ))
        }
    }
}

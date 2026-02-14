use bytes::Bytes;
use futures_util::stream::StreamExt;
use http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, REFERER, USER_AGENT};
use reqwest::header::HeaderMap;
use reqwest::{Client, StatusCode};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;
use url::Url;

pub enum FileDownloadError {
    Oversize,
    InvalidUrl,
    InvalidStatusCode(StatusCode),
    RequestError(reqwest::Error),
}

const DEFAULT_SIZE_LIMIT: u64 = 100_000_000; // 100MB

pub struct Downloader {
    client: Client,
    size_limit: u64,
    troublesome_instances: Arc<RwLock<Vec<String>>>,
}

impl Clone for Downloader {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            size_limit: self.size_limit,
            troublesome_instances: self.troublesome_instances.clone(),
        }
    }
}

pub struct DownloadedFile {
    pub bytes: Bytes,
    pub content_type: Option<String>,
    pub filename: String,
}

impl Downloader {
    pub fn new(size_limit: Option<u64>) -> Self {
        Self {
            client: Client::new(),
            size_limit: size_limit.unwrap_or(DEFAULT_SIZE_LIMIT),
            troublesome_instances: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn download_file(
        &self,
        url: &str,
        host: Option<&String>,
        ua: &str,
    ) -> Result<DownloadedFile, FileDownloadError> {
        debug!("Downloading file: {url}");

        // Get target host of instance
        let parsed_url = Url::parse(url).map_err(|_| FileDownloadError::InvalidUrl)?;
        let target_host = parsed_url
            .host_str()
            .ok_or(FileDownloadError::InvalidUrl)?
            .to_string();

        let mut resp: Option<reqwest::Response> = None;

        let worth_first_try = {
            // Put the lock into one specific block
            // for a quicker release (not sure whether this is necessary)
            !self
                .troublesome_instances
                .read()
                .await
                .contains(&target_host)
        };

        if worth_first_try {
            // First try: direct download
            let mut default_headers = HeaderMap::new();
            default_headers.insert(
                USER_AGENT,
                format!("MisskeyMediaProxy/{}~rs", env!("CARGO_PKG_VERSION"))
                    .parse()
                    .unwrap(),
            );

            debug!("Trying direct download...");
            resp = Some(
                self.client
                    .get(url)
                    .headers(default_headers)
                    .send()
                    .await
                    .map_err(FileDownloadError::RequestError)?,
            );
        }

        // if is 4xx error (e.g., 403 for hotlink protect), retry with host specified & request UA
        if !worth_first_try || resp.as_ref().is_some_and(|r| r.status().is_client_error()) {
            if let Some(host) = host {
                debug!(
                    "Direct download failed {} {url}, retrying with Host: {host:?}, UserAgent: {ua}",
                    resp.unwrap().status(),
                );
                let mut retry_headers = HeaderMap::new();
                retry_headers.insert(USER_AGENT, ua.parse().unwrap());
                retry_headers.insert(REFERER, host.parse().unwrap());

                resp = Some(
                    self.client
                        .get(url)
                        .headers(retry_headers)
                        .send()
                        .await
                        .map_err(FileDownloadError::RequestError)?,
                );

                if resp.as_ref().is_some_and(|r| r.status().is_success()) {
                    // It is really a nasty host
                    self.troublesome_instances.write().await.push(target_host);
                } // else: the target host might be dead
            }
        }

        let resp = resp.unwrap();

        // Check status code
        debug!("Download finish, checking status code...");
        let resp_status = resp.status();
        if !resp_status.is_success() || resp_status == StatusCode::NO_CONTENT {
            return Err(FileDownloadError::InvalidStatusCode(resp_status));
        }

        // Split response headers
        let resp_headers = resp.headers();

        // Check response size (content length)
        debug!("Status OK, checking content length (if any)...");
        if let Some(size) = resp.content_length() {
            if size > self.size_limit {
                return Err(FileDownloadError::Oversize);
            }
        } else if let Some(size_length) = resp_headers.get(CONTENT_LENGTH) {
            if let Ok(size) = size_length.to_str().unwrap().parse::<u64>() {
                if size > self.size_limit {
                    return Err(FileDownloadError::Oversize);
                }
            }
        }

        // Set filename // todo: handle encoded filenames
        debug!("Getting filename...");
        let mut filename = url.split('/').next_back().unwrap_or("unknown").to_string();
        if let Some(content_disposition) = resp_headers.get(CONTENT_DISPOSITION) {
            let field_parts = content_disposition.to_str().unwrap().split(';');
            for part in field_parts {
                let part = part.trim();
                if let Some(value) = part.strip_prefix("filename=") {
                    filename = value.trim_matches('"').to_string();
                    break;
                }
            }
        }

        // Nothing wrong, let's download the entire response body and return
        debug!("Length pre-check OK, downloading entire body...");
        let ct = resp_headers
            .get(CONTENT_TYPE)
            .map(|ct| ct.to_str().unwrap().to_string());
        let mut limited_buf = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            limited_buf.extend(chunk.map_err(FileDownloadError::RequestError)?);
            if limited_buf.len() as u64 > self.size_limit {
                return Err(FileDownloadError::Oversize);
            }
        }

        debug!("Response body downloaded, return. ContentType: {ct:?}");
        Ok(DownloadedFile {
            bytes: Bytes::from(limited_buf),
            content_type: ct,
            filename,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_download_file() {
        let downloader = Downloader::new(None); // use default size limit
        let file = downloader
            .download_file(
                "https://public.nyaone-object-storage.com/nyaone/ff02042e-524e-48e8-bb27-17621d96b13a.png",
                None,
                "MediaProxyRS@Debug",
            )
            .await;
        assert!(file.is_ok());
        if let Ok(downloaded) = file {
            assert!(downloaded.bytes.len() > 0);
            assert_eq!(downloaded.content_type, Some("image/png".to_string()));
            assert_eq!(downloaded.filename, "NyaOne_-_LOGO_-_256x_-_round.png");
        }
    }

    #[tokio::test]
    async fn test_size_limit() {
        let downloader = Downloader::new(Some(6));
        match downloader
            .download_file(
                "https://public.nyaone-object-storage.com/nyaone/ff02042e-524e-48e8-bb27-17621d96b13a.png",
                None,
                "MediaProxyRS@Debug",
            )
            .await
        {
            Err(FileDownloadError::Oversize) => (),
            _ => panic!("Wrong status"),
        };
    }
}

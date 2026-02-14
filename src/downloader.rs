use bytes::Bytes;
use futures_util::stream::StreamExt;
use http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, REFERER, USER_AGENT};
use reqwest::header::HeaderMap;
use reqwest::{Client, StatusCode};
use tracing::debug;

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
        }
    }

    pub async fn download_file(
        &self,
        url: &str,
        host: Option<&String>,
        ua: &str,
    ) -> Result<DownloadedFile, FileDownloadError> {
        debug!("Downloading file: {url}, Host: {host:?}, UserAgent: {ua}");

        let mut default_headers = HeaderMap::new();
        default_headers.insert(USER_AGENT, ua.parse().unwrap());

        // First try: direct download
        debug!("Trying direct download...");
        let mut resp = self
            .client
            .get(url)
            .headers(default_headers)
            .send()
            .await
            .map_err(FileDownloadError::RequestError)?;

        // if is 4xx error (e.g., 403 for hotlink protect), retry with host specified
        if resp.status().is_client_error() {
            debug!(
                "Direct download failed {} {}, retrying with host specified",
                resp.status(),
                url
            );
            if let Some(host) = host {
                let mut additional_headers = HeaderMap::new();
                additional_headers.insert(USER_AGENT, ua.parse().unwrap());
                additional_headers.insert(REFERER, host.parse().unwrap());

                resp = self
                    .client
                    .get(url)
                    .headers(additional_headers)
                    .send()
                    .await
                    .map_err(FileDownloadError::RequestError)?;
            }
        }

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

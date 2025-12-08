use crate::downloader::{DownloadedFile, Downloader, FileDownloadError};
use http::StatusCode;
use tracing::{error, warn};

pub enum DownloadImageError<'a> {
    MissingURL,
    MissingUA,
    RecursiveProxy,
    DownloadErrorOversize(&'a String),
    DownloadErrorInvalidStatus(StatusCode),
    DownloadErrorRequest,
    NotAnImage(DownloadedFile),
}

pub async fn download_image<'a>(
    downloader: &Downloader,
    url: Option<&'a String>,
    host: Option<&String>,
    ua: Option<&str>,
) -> Result<DownloadedFile, DownloadImageError<'a>> {
    // Check if url parameter is specified
    if url.is_none() {
        // Missing url
        warn!("Request missing url");
        return Err(DownloadImageError::MissingURL);
    }

    // Check if UserAgent is valid
    if ua.is_none() {
        // Missing UserAgent
        warn!("Request missing UserAgent");
        return Err(DownloadImageError::MissingUA);
    }

    let ua = ua.unwrap(); // Shadow the parameter value
    if ua.to_lowercase().contains("misskey/") {
        // Recursive proxying
        warn!("Recursive proxying from {ua}");
        return Err(DownloadImageError::RecursiveProxy);
    }

    // Start download
    let url = url.unwrap();
    let downloaded_file = match downloader.download_file(url, host, ua).await {
        Ok(b) => b,
        Err(e) => {
            return Err(match e {
                FileDownloadError::Oversize => {
                    // too large to process, redirect instead
                    warn!("File too large: {url}");
                    DownloadImageError::DownloadErrorOversize(url)
                }
                FileDownloadError::InvalidStatusCode(status_code) => {
                    warn!("Invalid status code: {url}, {status_code}");
                    // should we pass the exact same body from remote server?
                    // note: misskey will return the dummy.png if the status code is 404, but we don't implement that feature here
                    DownloadImageError::DownloadErrorInvalidStatus(status_code)
                }
                FileDownloadError::RequestError(err) => {
                    // request failed, return 500
                    error!("Failed to download file: {url}, {err}");
                    DownloadImageError::DownloadErrorRequest
                }
            });
        }
    };

    // Check possible mimetype of the downloaded file
    if let Some(ct) = downloaded_file.1.as_ref() {
        if !ct.starts_with("image/") {
            // Not image, return raw bytes
            warn!("Not an image ({ct}): {url}");
            return Err(DownloadImageError::NotAnImage(downloaded_file));
        }
    }

    Ok(downloaded_file)
}

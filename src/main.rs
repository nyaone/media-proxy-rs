mod downloader;
mod handler;
mod listener;

use crate::downloader::Downloader;
use tracing::info;

#[tokio::main]
async fn main() {
    // Initialize logger
    tracing_subscriber::fmt::init();

    // Get listen address from environment or use default
    let listen_addr = std::env::var("LISTEN").unwrap_or("127.0.0.1:3000".to_string());

    // Validate address early before initialization
    if let Err(e) = listener::parse_listen_addr(&listen_addr) {
        eprintln!("Configuration error: {}", e);
        std::process::exit(1);
    }

    // Initialize file downloader with optional size limit
    let size_limit = std::env::var("SIZE_LIMIT")
        .ok()
        .and_then(|size_limit_str| match size_limit_str.parse::<u64>() {
            Ok(size_limit) => {
                info!("Size limit set to {size_limit}");
                Some(size_limit)
            }
            Err(err) => {
                eprintln!(
                    "Warning: Failed to parse SIZE_LIMIT '{}': {}",
                    size_limit_str, err
                );
                None
            }
        });

    let downloader = Downloader::new(size_limit);

    // Start the server
    if let Err(e) = listener::start_listener(downloader, &listen_addr).await {
        eprintln!("Server failed to start: {}", e);
        std::process::exit(1);
    }
}

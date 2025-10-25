mod handler;
mod downloader;

use std::net::SocketAddr;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::downloader::Downloader;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn start_server(downloader: Downloader, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("MediaProxyRS@NyaOne #{VERSION} starting...");

    // We create a TcpListener and bind it to 127.0.0.1:3000
    let listener = TcpListener::bind(addr).await?;

    // We start a loop to continuously accept incoming connections
    loop {
        let (stream, _) = listener.accept().await?;

        // Use an adapter to access something implementing `tokio::io` traits as if they implement
        // `hyper::rt` IO traits.
        let io = TokioIo::new(stream);

        let downloader = downloader.clone();

        // Spawn a tokio task to serve multiple connections concurrently
        tokio::task::spawn(async move {
            // Finally, we bind the incoming connection to our `hello` service
            if let Err(err) = http1::Builder::new()
                // `service_fn` converts our function in a `Service`
                .serve_connection(io, service_fn(|req| handler::handle(&downloader, req)))
                .await
            {
                error!("Error serving connection: {:?}", err);
            }
        });
    }
}

#[tokio::main]
async fn main() {
    // Prepare logger
    tracing_subscriber::fmt::init();

    // Get address to listen from env or default
    let env_listen = std::env::var("LISTEN").unwrap_or("127.0.0.1:3000".to_string());

    // Parse to socket address
    let addr: SocketAddr = env_listen.parse().expect("Invalid listen address");

    // Init file downloader
    let downloader = Downloader::new();

    // Start server
    start_server(downloader, addr).await.expect("Server start failed");
}

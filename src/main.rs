mod downloader;
mod handler;

use crate::downloader::Downloader;
use crate::handler::{ProxyImageError, proxy_image};
use bytes::Bytes;
use http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, LOCATION, USER_AGENT};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, combinators::BoxBody};
use http_body_util::{Empty, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use url::form_urlencoded;

const VERSION: &str = env!("CARGO_PKG_VERSION");

// We create some utility functions to make Empty and Full bodies
// fit our broadened Response body type.

#[inline]
pub fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

#[inline]
pub fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

#[inline]
pub fn response_raw(
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

async fn handle(
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
                    response.headers_mut().insert(
                        CONTENT_DISPOSITION,
                        format!("inline; filename=\"{}\"", file.filename)
                            .parse()
                            .unwrap(),
                    );
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

async fn start_server(
    downloader: Downloader,
    addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
                .serve_connection(io, service_fn(|req| handle(&downloader, req)))
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

    // Init file downloader (Read size limit from env)
    let downloader = Downloader::new(match std::env::var("SIZE_LIMIT") {
        Ok(size_limit_str) => match size_limit_str.parse::<u64>() {
            Ok(size_limit) => {
                info!("Size limit set to {size_limit}");
                Some(size_limit)
            }
            Err(err) => {
                warn!("Failed to parse size limit {size_limit_str}: {err}, fallback to default");
                None
            }
        },
        Err(err) => {
            if err == std::env::VarError::NotPresent {
                info!("Size limit not set, using default");
            } else {
                warn!("Failed to read size limit from env: {err}, fallback to default");
            }
            None
        }
    });

    // Start server
    start_server(downloader, addr)
        .await
        .expect("Server start failed");
}

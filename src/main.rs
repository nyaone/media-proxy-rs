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
use socket2::{Domain, Socket, Type};
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use tokio::net::{TcpListener, UnixListener};
use tracing::{error, info, warn};
use url::form_urlencoded;

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

async fn start_server(
    downloader: Downloader,
    listen_addr: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!(
        "MediaProxyRS@NyaOne #{} starting...",
        env!("CARGO_PKG_VERSION")
    );

    // Validate listen address format
    validate_listen_addr(listen_addr)?;

    // Determine if it's a Unix socket or TCP address
    if listen_addr.starts_with('/') || listen_addr.starts_with("./") {
        // Unix socket path
        start_unix_socket_server(downloader, listen_addr).await
    } else {
        // TCP address (IPv4 or IPv6)
        start_tcp_server(downloader, listen_addr).await
    }
}

fn validate_listen_addr(addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if addr.is_empty() {
        return Err("Listen address cannot be empty".into());
    }

    // Unix socket validation
    if addr.starts_with('/') || addr.starts_with("./") {
        if addr.len() > 104 {
            // Unix socket max path length
            return Err(format!(
                "Unix socket path too long (max 104 chars): {}",
                addr
            )
            .into());
        }
        return Ok(());
    }

    // TCP address validation
    let parse_result: Result<SocketAddr, _> = addr.parse();
    match parse_result {
        Ok(_) => Ok(()),
        Err(e) => Err(format!(
            "Invalid TCP address '{}': {}. \
             Expected format like '0.0.0.0:3000', '[::1]:3000', or '127.0.0.1:8080'",
            addr, e
        )
        .into()),
    }
}

async fn start_tcp_server(
    downloader: Downloader,
    addr_str: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = if addr_str.contains(':') && !addr_str.starts_with('[') {
        // IPv4 address like "0.0.0.0:3000" - convert to IPv6 with dual-stack
        let addr: SocketAddr = addr_str.parse()?;
        let ipv6_addr = std::net::SocketAddrV6::new(
            std::net::Ipv6Addr::UNSPECIFIED,
            addr.port(),
            0,
            0,
        );
        let socket = Socket::new(Domain::IPV6, Type::STREAM, None)?;
        socket.set_only_v6(false)?; // Enable dual-stack
        socket.set_reuse_address(true)?;
        socket.bind(&std::net::SocketAddr::V6(ipv6_addr).into())?;
        socket.listen(128)?;
        let std_listener: std::net::TcpListener = socket.into();
        std_listener.set_nonblocking(true)?;
        TcpListener::from_std(std_listener)?
    } else {
        // IPv6 address or already in correct format
        TcpListener::bind(addr_str).await?
    };

    info!("Server listening on TCP (dual-stack IPv4/IPv6): {}", addr_str);

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let downloader = downloader.clone();

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(|req| handle(&downloader, req)))
                .await
            {
                error!("Error serving connection: {:?}", err);
            }
        });
    }
}

async fn start_unix_socket_server(
    downloader: Downloader,
    socket_path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Safety check: verify socket is not in use
    verify_and_cleanup_unix_socket(socket_path).await?;

    let listener = UnixListener::bind(socket_path)?;
    
    // Write PID to lock file for monitoring
    write_lock_file(socket_path)?;
    
    info!("Server listening on Unix socket: {}", socket_path);

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let downloader = downloader.clone();

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(|req| handle(&downloader, req)))
                .await
            {
                error!("Error serving connection: {:?}", err);
            }
        });
    }
}

async fn verify_and_cleanup_unix_socket(
    socket_path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = Path::new(socket_path);

    if !path.exists() {
        return Ok(());
    }

    // Try to connect to the existing socket to see if it's in use
    match tokio::net::UnixStream::connect(socket_path).await {
        Ok(_) => {
            // Connection succeeded - socket is in use by another process
            return Err(format!(
                "Unix socket '{}' is already in use by another process. \
                 Ensure no other instance is running, or use a different socket path.",
                socket_path
            )
            .into());
        }
        Err(e) => {
            // Connection failed - socket file is stale (process crashed or exited)
            warn!(
                "Found stale Unix socket '{}' from dead process: {}. Cleaning up...",
                socket_path, e
            );

            // Clean up stale socket file
            if let Err(delete_err) = fs::remove_file(socket_path) {
                return Err(format!(
                    "Failed to remove stale socket file '{}': {}",
                    socket_path, delete_err
                )
                .into());
            }

            // Also remove associated lock file
            let lock_path = format!("{}.lock", socket_path);
            if Path::new(&lock_path).exists() {
                let _ = fs::remove_file(&lock_path);
            }

            info!("Stale socket cleaned up successfully");
            Ok(())
        }
    }
}

fn write_lock_file(socket_path: &str) -> io::Result<()> {
    let lock_path = format!("{}.lock", socket_path);
    let pid = std::process::id();
    fs::write(&lock_path, pid.to_string())?;
    Ok(())
}

#[tokio::main]
async fn main() {
    // Prepare logger
    tracing_subscriber::fmt::init();

    // Get address to listen from env or default
    let listen_addr = std::env::var("LISTEN").unwrap_or("127.0.0.1:3000".to_string());

    // Validate address early
    if let Err(e) = validate_listen_addr(&listen_addr) {
        eprintln!("Configuration error: {}", e);
        std::process::exit(1);
    }

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
    start_server(downloader, &listen_addr)
        .await
        .expect("Server start failed");
}

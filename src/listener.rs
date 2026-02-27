use crate::downloader::Downloader;
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

/// Parse and validate listen address
pub fn parse_listen_addr(addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if addr.is_empty() {
        return Err("Listen address cannot be empty".into());
    }

    // Unix socket path validation
    if addr.starts_with('/') || addr.starts_with("./") {
        if addr.len() > 104 {
            return Err(format!(
                "Unix socket path too long (max 104 chars): {} ({})",
                addr,
                addr.len()
            )
            .into());
        }
        return Ok(());
    }

    // TCP address validation
    let _: SocketAddr = addr.parse().map_err(|e| {
        format!(
            "Invalid TCP address '{}': {}. \
             Expected format like '0.0.0.0:3000', '[::1]:3000', or '127.0.0.1:8080'",
            addr, e
        )
    })?;

    Ok(())
}

/// Start appropriate listener based on address type
pub async fn start_listener(
    downloader: Downloader,
    listen_addr: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!(
        "MediaProxyRS@NyaOne #{} starting...",
        env!("CARGO_PKG_VERSION")
    );

    // Determine listener type and start
    if listen_addr.starts_with('/') || listen_addr.starts_with("./") {
        start_unix_socket_listener(downloader, listen_addr).await
    } else {
        start_tcp_listener(downloader, listen_addr).await
    }
}

/// Start TCP listener (with dual-stack for IPv4)
async fn start_tcp_listener(
    downloader: Downloader,
    addr_str: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = addr_str.parse()?;
    
    // Only use dual-stack for 0.0.0.0 (IPv4 wildcard)
    // For specific IPs like 10.42.0.1, bind directly
    let listener = if let std::net::IpAddr::V4(ip) = addr.ip() {
        if ip == std::net::Ipv4Addr::UNSPECIFIED {
            // 0.0.0.0:port - Use dual-stack
            create_dual_stack_listener(addr)?
        } else {
            // Specific IPv4 like 10.42.0.1:port - Bind directly
            TcpListener::bind(addr).await?
        }
    } else {
        // IPv6 address - bind normally
        TcpListener::bind(addr).await?
    };

    // Check if dual-stack is being used
    let mode = if let std::net::IpAddr::V4(ip) = addr.ip() {
        if ip == std::net::Ipv4Addr::UNSPECIFIED {
            "(dual-stack IPv4/IPv6)"
        } else {
            "(IPv4 only)"
        }
    } else {
        "(IPv6)"
    };

    info!("Server listening on TCP {}: {}", mode, addr_str);

    tcp_accept_loop(listener, downloader).await
}

/// Create dual-stack IPv6 listener that accepts IPv4 too
/// Only used when binding to 0.0.0.0:port
fn create_dual_stack_listener(addr: SocketAddr) -> Result<TcpListener, Box<dyn std::error::Error + Send + Sync>> {
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
    Ok(TcpListener::from_std(std_listener)?)
}

/// Accept loop for TCP listener
async fn tcp_accept_loop(
    listener: TcpListener,
    downloader: Downloader,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let downloader = downloader.clone();

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(|req| crate::handler::handle(
                    &downloader,
                    req,
                )))
                .await
            {
                error!("Error serving connection: {:?}", err);
            }
        });
    }
}

/// Start Unix domain socket listener
async fn start_unix_socket_listener(
    downloader: Downloader,
    socket_path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Safety check and cleanup
    verify_and_cleanup_unix_socket(socket_path).await?;

    let listener = UnixListener::bind(socket_path)?;
    write_lock_file(socket_path)?;

    info!("Server listening on Unix socket: {}", socket_path);

    // Run accept loop, allowing graceful shutdown via signals
    unix_accept_loop_with_signals(listener, downloader, socket_path).await
}

/// Accept loop for Unix socket with signal handling
async fn unix_accept_loop_with_signals(
    listener: UnixListener,
    downloader: Downloader,
    socket_path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let socket_path = socket_path.to_string();
    
    loop {
        // Setup SIGTERM handler for this iteration
        let sigterm_future = async {
            #[cfg(unix)]
            {
                if let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    let _ = sig.recv().await;
                }
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await;
            }
        };

        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                let io = TokioIo::new(stream);
                let downloader = downloader.clone();

                tokio::task::spawn(async move {
                    if let Err(err) = http1::Builder::new()
                        .serve_connection(io, service_fn(|req| crate::handler::handle(
                            &downloader,
                            req,
                        )))
                        .await
                    {
                        error!("Error serving connection: {:?}", err);
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Shutdown signal received (SIGINT), cleaning up...");
                cleanup_unix_socket(&socket_path)?;
                return Ok(());
            }
            _ = sigterm_future => {
                info!("Shutdown signal received (SIGTERM), cleaning up...");
                cleanup_unix_socket(&socket_path)?;
                return Ok(());
            }
        }
    }
}

/// Clean up Unix socket and lock file
fn cleanup_unix_socket(socket_path: &str) -> io::Result<()> {
    let path = Path::new(socket_path);
    
    if path.exists() {
        fs::remove_file(path).map_err(|e| {
            error!("Failed to remove socket '{}': {}", socket_path, e);
            e
        })?;
        info!("Socket file cleaned up: {}", socket_path);
    }

    let lock_path = format!("{}.lock", socket_path);
    if Path::new(&lock_path).exists() {
        fs::remove_file(&lock_path).map_err(|e| {
            error!("Failed to remove lock file '{}': {}", lock_path, e);
            e
        })?;
        info!("Lock file cleaned up: {}", lock_path);
    }

    Ok(())
}

/// Check if Unix socket is in use or stale
async fn verify_and_cleanup_unix_socket(
    socket_path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = Path::new(socket_path);

    if !path.exists() {
        return Ok(());
    }

    // Try to connect - if succeeds, socket is in use
    match tokio::net::UnixStream::connect(socket_path).await {
        Ok(_) => Err(format!(
            "Unix socket '{}' is already in use by another process. \
             Ensure no other instance is running, or use a different socket path.",
            socket_path
        )
        .into()),
        Err(e) => {
            // Connection failed - socket is stale
            warn!(
                "Found stale Unix socket '{}' from dead process: {}. Cleaning up...",
                socket_path, e
            );

            fs::remove_file(socket_path).map_err(|e| {
                format!("Failed to remove stale socket '{}': {}", socket_path, e)
            })?;

            // Clean up lock file
            let lock_path = format!("{}.lock", socket_path);
            if Path::new(&lock_path).exists() {
                let _ = fs::remove_file(&lock_path);
            }

            info!("Stale socket cleaned up successfully");
            Ok(())
        }
    }
}

/// Write PID lock file
fn write_lock_file(socket_path: &str) -> io::Result<()> {
    let lock_path = format!("{}.lock", socket_path);
    fs::write(&lock_path, std::process::id().to_string())
}

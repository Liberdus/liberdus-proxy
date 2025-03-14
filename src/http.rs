//! # Client Request Handler Module
//!
//! This module defines the core logic for handling client requests in a TCP-based system.
//! It handles the communication between clients and backend servers (consensors) via a load-balancing system.
//! The module uses asynchronous I/O operations provided by Tokio, ensuring scalability and efficiency.
//!
//! ## Features
//! - Reads and parses HTTP requests from clients with support for timeouts.
//! - Forwards client requests to a backend consensor server.
//! - Relays server responses back to the client.
//! - Implements error handling for timeouts, unavailable servers, and unexpected I/O errors.
//!
//! ## Components
//! - `handle_stream`: The main entry point for processing client requests.
//! - `read_or_collect`: Reads and collects request or response data with header parsing.
//! - `respond_with_internal_error`: Sends a 500 Internal Server Error response to the client.
//! - `respond_with_timeout`: Sends a 504 Gateway Timeout response to the client.
use crate::{config, liberdus, shardus_monitor, Stats};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{timeout, Duration};
use tokio_rustls::TlsAcceptor;

/// Reads from the stream until the end of the headers or the end of the body if the Content-Length
/// header is present. The data is collected into the buffer.
pub async fn collect_http<S>(stream: &mut S, buffer: &mut Vec<u8>) -> Result<(), std::io::Error>
where
    S: AsyncRead + Unpin + Send,
{
    const TEMP_BUFFER_SIZE: usize = 1024;
    let mut temp_buffer = [0; TEMP_BUFFER_SIZE];
    let mut headers_read = false;
    let mut content_length: Option<usize> = None;

    loop {
        // Read into the temporary buffer
        let n = stream.read(&mut temp_buffer).await?;
        if n == 0 {
            if !headers_read {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Stream closed",
                ));
            }
            break; // Stream closed
        }

        buffer.extend_from_slice(&temp_buffer[..n]);

        // Parse headers to determine content length
        if !headers_read {
            if let Some(headers_end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                headers_read = true;
                let headers = &buffer[..headers_end + 4];
                content_length = parse_content_length(headers);

                if content_length.is_none() {
                    // then no body
                    break;
                }
            }
        }

        // Stop reading if content length is known and body is fully read
        if let Some(length) = content_length {
            let body_start = buffer
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .unwrap_or(0)
                + 4;
            if buffer.len() >= body_start + length {
                break;
            }
        }

        // Optional: Limit the buffer size to prevent potential DoS attacks
        const MAX_PAYLOAD_SIZE: usize = 1024 * 1024; // 1 MB
        if buffer.len() > MAX_PAYLOAD_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Payload too large",
            ));
        }
    }

    Ok(())
}

/// Outer loop to handle multiple HTTP requests from the same stream
pub async fn handle_stream<StreamLike>(
    mut client_stream: StreamLike,
    liberdus: Arc<liberdus::Liberdus>,
    config: Arc<config::Config>,
) -> Result<(), Box<dyn std::error::Error>>
where
    StreamLike: AsyncWrite + AsyncRead + Unpin + Send,
{
    loop {
        let mut req_buf = Vec::new();
        match timeout(
            Duration::from_secs(config.tcp_keepalive_time_sec.into()),
            collect_http(&mut client_stream, &mut req_buf),
        )
        .await
        {
            Ok(Ok(())) => {
                let (method, route) = get_route(&req_buf).unwrap();

                if shardus_monitor::proxy::is_monitor_route(&route) {
                    if let Err(e) = shardus_monitor::proxy::handle_request(
                        req_buf,
                        route,
                        &mut client_stream,
                        config.clone(),
                    )
                    .await
                    {
                        eprintln!("Error handling monitor request: {}", e);
                    }
                } else {
                    if let Err(e) = liberdus::handle_request(
                        req_buf,
                        &mut client_stream,
                        liberdus.clone(),
                        config.clone(),
                    )
                    .await
                    {
                        eprintln!("Error handling liberdus request: {}", e);
                    }
                }
            }
            Ok(Err(e)) => {
                eprintln!("Error attempting to read bytes out of client stream: {}", e);
                break;
            }
            Err(_) => {
                eprintln!("Shutting down Stream due to inactivity beyond keepalive time.");
                break;
            }
        }
    }

    match client_stream.shutdown().await {
        Ok(_) => Ok(()),
        Err(_e) => Ok(()),
    }
}

/// Helper function to insert or replace a header in the HTTP response buffer.
pub fn set_http_header(buffer: &mut Vec<u8>, key: &str, value: &str) {
    if let Ok(buffer_str) = std::str::from_utf8(buffer) {
        // Locate the end of the headers
        if let Some(headers_end) = buffer_str.find("\r\n\r\n") {
            // Collect headers as a vector of Strings
            let mut headers: Vec<String> = buffer_str[..headers_end]
                .lines()
                .map(String::from)
                .collect();
            let header_prefix = format!("{}:", key);
            let mut found = false;

            // Update or replace the existing header
            for header in headers.iter_mut() {
                if header.starts_with(&header_prefix) {
                    *header = format!("{} {}", header_prefix, value);
                    found = true;
                    break;
                }
            }

            // If the header is not found, add it
            if !found {
                headers.push(format!("{}: {}", key, value));
            }

            // Rebuild the buffer with updated headers and the original body
            let updated_headers = headers.join("\r\n");
            let body = &buffer[headers_end..]; // Keep the body untouched
            let mut new_buffer = updated_headers.into_bytes();
            new_buffer.extend_from_slice(body);

            *buffer = new_buffer;
        }
    }
}

/// Parses the `Content-Length` header from the given headers.
/// Returns `None` if the header is missing or invalid.
fn parse_content_length(headers: &[u8]) -> Option<usize> {
    if let Ok(headers_str) = std::str::from_utf8(headers) {
        for line in headers_str.lines() {
            if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                return value.trim().parse::<usize>().ok();
            }
        }
    }
    None
}

/// Takes the stream, responds with a 500 Internal Server Error, and shutdown tcp
pub async fn respond_with_internal_error<S>(client_stream: &mut S) -> Result<(), std::io::Error>
where
    S: AsyncWrite + Unpin + Send,
{
    let response =
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    client_stream.write_all(response.as_bytes()).await
}

/// Takes the stream, responds with a timeout error, and shutdown tcp
pub async fn respond_with_timeout<S>(client_stream: &mut S) -> Result<(), std::io::Error>
where
    S: AsyncWrite + Unpin + Send,
{
    let response = "HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    client_stream.write_all(response.as_bytes()).await
}

// without host
pub fn get_route(buffer: &[u8]) -> Option<(String, String)> {
    let mut route = None;
    let mut method = None;

    if let Ok(buffer_str) = std::str::from_utf8(buffer) {
        for line in buffer_str.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let http_method = parts[0]; // GET, POST, DELETE, etc.
                let path = parts[1]; // The requested path

                if matches!(
                    http_method,
                    "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "OPTIONS" | "HEAD"
                ) {
                    method = Some(http_method.to_string());
                    route = Some(path.to_string());
                    break;
                }
            }
        }
    }

    match (method, route) {
        (Some(m), Some(r)) => Some((m, r)),
        _ => None,
    }
}
pub async fn listen(
    liberdus: Arc<liberdus::Liberdus>,
    config: Arc<config::Config>,
    server_stats: Arc<Stats>,
    tls_acceptor: Option<TlsAcceptor>,
) {
    // let semaphore = Arc::new(Semaphore::new(300));

    let listener = match tokio::net::TcpListener::bind(format!(
        "0.0.0.0:{}",
        config.http_port.clone()
    ))
    .await
    {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Error binding to port: {}", e);
            std::process::exit(1);
        }
    };
    println!(
        "HTTP Listening on: {}",
        listener.local_addr().expect("Couldn't bind to a port")
    );

    loop {
        let (raw_stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error: {}", e);
                continue;
            }
        };

        let liberdus = Arc::clone(&liberdus);
        let config = Arc::clone(&config);
        let stats = Arc::clone(&server_stats);
        let tls_acceptor = match tls_acceptor.is_some() && config.tls.enabled {
            true => Some(tls_acceptor.clone().unwrap()),
            false => None,
        };

        tokio::spawn(async move {
            // let permit = throttler.acquire().await.unwrap();
            stats
                .stream_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            match tls_acceptor {
                Some(tls_acceptor) => match tls_acceptor.accept(raw_stream).await {
                    Ok(tls_stream) => {
                        let tls_stream = tokio_rustls::TlsStream::Server(tls_stream);
                        let e = handle_stream(tls_stream, liberdus, config).await;
                        if let Err(e) = e {
                            eprintln!("Handle Stream Error: {}", e);
                        }
                    }
                    Err(e) => {
                        eprintln!("TLS Handshake Error: {}", e);
                        stats
                            .stream_count
                            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                },
                None => {
                    let e = handle_stream(raw_stream, liberdus, config).await;
                    if let Err(e) = e {
                        eprintln!("Handle Stream Error: {}", e);
                    }
                }
            }
            stats
                .stream_count
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });
    }
}

pub fn extract_body(buffer: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    if let Ok(buffer_str) = std::str::from_utf8(buffer) {
        if let Some(body_start) = buffer_str.find("\r\n\r\n") {
            body.extend_from_slice(&buffer[body_start + 4..]);
        }
    }
    body
}

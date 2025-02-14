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
use tokio::net::TcpStream;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncRead, AsyncWrite};
use tokio::time::{timeout, Duration};
use tokio_rustls::TlsAcceptor;
use crate::{Stats, liberdus, config};

/// Handles the client request stream by reading the request, forwarding it to a consensor server,
/// collect the response from validator, and relaying it back to the client.
/// Handles a single HTTP request from the client stream.
/// This function assumes it processes only one http payload request.
/// The proxy will maintain the tcp stream open for a specify amount of time to avoid the handshake
/// overhead when the client do frequent calls.
/// Proxy will not maintain tcp stream connected to upstream server/validator. It is always single
/// use and always shutdown after the response is sent back to the client. If the multiple request
/// from single client tcp stream, it is advisable to pick different validator each time.
///
/// **No chunked encoding is supported.**
/// **No Multiplexing is supported.**
pub async fn handle_request<S>(
    request_buffer: Vec<u8>,
    client_stream: &mut S,
    liberdus: Arc<liberdus::Liberdus>,
    config: Arc<config::Config>,
) -> Result<(), Box<dyn std::error::Error>>
where S: AsyncWrite + AsyncRead + Unpin + Send
{
    // Get the next appropriate consensor from liberdus
    let (_, target_server) = match liberdus.get_next_appropriate_consensor().await {
        Some(consensor) => consensor,
        None => {
            eprintln!("No consensors available.");
            respond_with_internal_error(client_stream).await?;
            return Err("No consensors available".into());
        }
    };

    let ip_port = format!("{}:{}", target_server.ip.clone(), target_server.port.clone());

    let mut server_stream = match timeout(Duration::from_millis(config.max_http_timeout_ms as u64), TcpStream::connect(ip_port)).await {
        Ok(Ok(stream)) => {
            stream
        },
        Ok(Err(e)) => {
            eprintln!("Error connecting to target server: {}", e);
            respond_with_timeout(client_stream).await?;
            return Err(Box::new(e));
        }
        Err(_) => {
            eprintln!("Timeout connecting to target server.");
            respond_with_timeout(client_stream).await?;
            return Err("Timeout connecting to target server".into());
        }
    };

    // Forward the client's request to the server
    let now = std::time::Instant::now();
    let mut response_data = vec![];
    match timeout(Duration::from_millis(config.max_http_timeout_ms as u64), server_stream.write_all(&request_buffer)).await {
        Ok(Ok(())) => {
            match collect_http(&mut server_stream, &mut response_data).await {
                Ok(()) => {
                    let elapsed = now.elapsed();
                    liberdus.set_consensor_trip_ms(target_server.id, elapsed.as_millis());
                    tokio::spawn(async move {
                        server_stream.shutdown().await.unwrap();
                        drop(server_stream);
                    });
                },
                Err(e) => {
                    eprintln!("Error reading response from server: {}", e);
                    respond_with_internal_error(client_stream).await?;
                    liberdus.set_consensor_trip_ms(target_server.id, config.max_http_timeout_ms);
                    tokio::spawn(async move {
                        server_stream.shutdown().await.unwrap();
                        drop(server_stream);
                    });
                    return Err(Box::new(e));
                }
            }


        },
        Ok(Err(e)) => {
            eprintln!("Error forwarding request to server: {}", e);
            respond_with_internal_error(client_stream).await?;
            liberdus.set_consensor_trip_ms(target_server.id, config.max_http_timeout_ms);
            tokio::spawn(async move {
                server_stream.shutdown().await.unwrap();
                drop(server_stream);
            });
            return Err(Box::new(e));
        }
        Err(_) => {
            eprintln!("Timeout forwarding request to server.");
            respond_with_timeout(client_stream).await?;
            liberdus.set_consensor_trip_ms(target_server.id, config.max_http_timeout_ms);
            tokio::spawn(async move {
                server_stream.shutdown().await.unwrap();
                drop(server_stream);
            });
            return Err("Timeout forwarding request to server".into());
        }
    }
    println!("Successfully forwarded request to server.");

    drop(request_buffer);

    if response_data.is_empty() {
        eprintln!("Empty response from server.");
        respond_with_internal_error(client_stream).await?;
        return Err("Empty response from server".into());
    }


    set_http_header(&mut response_data, "Connection", "keep-alive");
    set_http_header(&mut response_data, "Keep-Alive", format!("timeout={}", config.tcp_keepalive_time_sec).as_str());
    set_http_header(&mut response_data, "Access-Control-Allow-Origin", "*");


    // Relay the collected response to the client
    if let Err(e) = client_stream.write_all(&response_data).await {
        eprintln!("Error relaying response to client: {}", e);
        respond_with_internal_error(client_stream).await?;
        return Err(Box::new(e));
    }

    Ok(())
}

/// Reads from the stream until the end of the headers or the end of the body if the Content-Length
/// header is present. The data is collected into the buffer.
pub async fn collect_http<S>(stream: &mut S, buffer: &mut Vec<u8>) -> Result<(), std::io::Error>
where S: AsyncRead + Unpin + Send
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
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Stream closed"));
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
            let body_start = buffer.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(0) + 4;
            if buffer.len() >= body_start + length {
                break;
            }
        }

        // Optional: Limit the buffer size to prevent potential DoS attacks
        const MAX_PAYLOAD_SIZE: usize = 1024 * 1024; // 1 MB
        if buffer.len() > MAX_PAYLOAD_SIZE {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Payload too large"));
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
where StreamLike: AsyncWrite + AsyncRead + Unpin + Send
{
    loop {
        let mut req_buf = Vec::new();
        match timeout(Duration::from_secs(config.tcp_keepalive_time_sec.into()), collect_http(&mut client_stream, &mut req_buf)).await {
            Ok(Ok(())) => {

                if let Err(e) = handle_request(req_buf, &mut client_stream, liberdus.clone(), config.clone()).await {
                    eprintln!("Error handling request: {}", e);
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
        Ok(_) => {Ok(())},
        Err(_e) => {
            Ok(())
        }
    }
}

/// Helper function to insert or replace a header in the HTTP response buffer.
pub fn set_http_header(buffer: &mut Vec<u8>, key: &str, value: &str) {
    if let Ok(buffer_str) = std::str::from_utf8(buffer) {
        // Locate the end of the headers
        if let Some(headers_end) = buffer_str.find("\r\n\r\n") {
            // Collect headers as a vector of Strings
            let mut headers: Vec<String> = buffer_str[..headers_end].lines().map(String::from).collect();
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
async fn respond_with_internal_error<S>(client_stream: &mut S) -> Result<(), std::io::Error>
where S: AsyncWrite + Unpin + Send
{
    let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    client_stream.write_all(response.as_bytes()).await
}

/// Takes the stream, responds with a timeout error, and shutdown tcp
async fn respond_with_timeout<S>(client_stream: &mut S) -> Result<(), std::io::Error>
where S: AsyncWrite + Unpin + Send
{
    let response = "HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    client_stream.write_all(response.as_bytes()).await
}


// without host
pub fn get_route(buffer: &[u8]) -> Option<String> {
    let mut route = None;
    if let Ok(buffer_str) = std::str::from_utf8(buffer) {
        for line in buffer_str.lines() {
            if let Some(value) = line.strip_prefix("GET ") {
                let mut parts = value.split_whitespace();
                if let Some(path) = parts.next() {
                    route = Some(path.to_string());
                    break;
                }
            }
        }
    }
    route
}


pub async fn listen(
    liberdus: Arc<liberdus::Liberdus>, 
    config: Arc<config::Config>, 
    server_stats: Arc<Stats>, 
    tls_acceptor: Option<TlsAcceptor>
) {
    // let semaphore = Arc::new(Semaphore::new(300));

    let listener = match tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.http_port.clone())).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Error binding to port: {}", e);
            std::process::exit(1);
        }
    };
    println!("HTTP Listening on: {}", listener.local_addr().expect("Couldn't bind to a port"));

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
            stats.stream_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            

            match tls_acceptor {
                Some(tls_acceptor) => {
                    match tls_acceptor.accept(raw_stream).await{
                        Ok(tls_stream) => {
                            let tls_stream = tokio_rustls::TlsStream::Server(tls_stream);
                            let e = handle_stream(tls_stream, liberdus, config).await;
                            if let Err(e) = e {
                                eprintln!("Handle Stream Error: {}", e);
                            }
                        },
                        Err(e) => {
                            eprintln!("TLS Handshake Error: {}", e);
                            stats.stream_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                            return
                        }
                    }
                },
                None => {
                    let e = handle_stream(raw_stream, liberdus, config).await;
                    if let Err(e) = e {
                        eprintln!("Handle Stream Error: {}", e);
                    }
                }
            }
            stats.stream_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });

    }
}

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
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};
use tokio_rustls::TlsStream;

use crate::{liberdus, config};


// Create a type alias for a unified stream type
pub enum StreamWrapper {
    Plain(TcpStream),
    Tls(TlsStream<TcpStream>),
}

impl StreamWrapper {
    pub async fn read(&mut self, buf: &mut [u8]) -> tokio::io::Result<usize> {
        match self {
            StreamWrapper::Plain(ref mut stream) => stream.read(buf).await,
            StreamWrapper::Tls(ref mut stream) => Pin::new(stream).read(buf).await,
        }
    }

    pub async fn write_all(&mut self, buf: &[u8]) -> tokio::io::Result<()> {
        match self {
            StreamWrapper::Plain(ref mut stream) => stream.write_all(buf).await,
            StreamWrapper::Tls(ref mut stream) => {
                    let mut pinned = Pin::new(stream);            
                    let _ = pinned.write_all(buf).await;
                    pinned.flush().await
            },
        }
    }

    pub async fn shutdown(&mut self) -> tokio::io::Result<()> {
        match self {
            StreamWrapper::Plain(ref mut stream) => stream.shutdown().await,
            StreamWrapper::Tls(ref mut stream) => {
                let mut st = Pin::new(stream);
                // Ensure all data is written before shutting down
                if let Err(e) = st.flush().await {
                    eprintln!("Error flushing TLS stream: {}", e);
                }
                st.shutdown().await
            }
        }
    }

    pub async fn readable(&mut self) -> tokio::io::Result<()> {
        match self {
            StreamWrapper::Plain(ref mut stream) => stream.readable().await,
            StreamWrapper::Tls(ref mut stream) => {
                let (tcp_stream, tls) = stream.get_mut();
                loop {
                    let _ = tcp_stream.readable().await?;
                    match tls.wants_read() {
                        true => return Ok(()),
                        false => continue,
                    }
                }
            }
        }
    }
}

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
pub async fn handle_request(
    request_buffer: Vec<u8>,
    client_stream: &mut StreamWrapper,
    liberdus: Arc<liberdus::Liberdus>,
    config: Arc<config::Config>,
) -> Result<(), Box<dyn std::error::Error>> {
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
            println!("Successfully connected to target server.");
            StreamWrapper::Plain(stream)
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
    match timeout(Duration::from_millis(config.max_http_timeout_ms as u64), server_stream.write_all(&request_buffer)).await {
        Ok(Ok(())) => {
            server_stream.readable().await?;
            let elapsed = now.elapsed();
            liberdus.set_consensor_trip_ms(target_server.id, elapsed.as_millis());
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

    // Collect the entire response from the server
    let mut response_data = Vec::new();
    if let Err(e) = read_or_collect(&mut server_stream, &mut response_data).await {
        eprintln!("Error collecting response from server: {}", e);
        respond_with_internal_error(client_stream).await?;
        tokio::spawn(async move {
            server_stream.shutdown().await.unwrap();
            drop(server_stream);
        });
        return Err(Box::new(e));
    }

    tokio::spawn(async move {
        server_stream.shutdown().await.unwrap();
        drop(server_stream);
    });

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
pub async fn read_or_collect(stream: &mut StreamWrapper, buffer: &mut Vec<u8>) -> Result<(), std::io::Error> {
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
        const MAX_PAYLOAD_SIZE: usize = (1024 * 1024) * 2; // 2 MB
        if buffer.len() > MAX_PAYLOAD_SIZE {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Payload too large"));
        }
    }

    Ok(())
}

/// Outer loop to handle multiple HTTP requests from the same stream
pub async fn handle_stream(
    mut client_stream: StreamWrapper,
    liberdus: Arc<liberdus::Liberdus>,
    config: Arc<config::Config>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        match timeout(Duration::from_secs(config.tcp_keepalive_time_sec.into()), client_stream.readable()).await {
            Ok(Ok(())) => {
                let mut request_buffer = Vec::new();

                match read_or_collect(&mut client_stream, &mut request_buffer).await {
                    Ok(_) => {
                        if let Err(e) = handle_request(request_buffer, &mut client_stream, liberdus.clone(), config.clone()).await {
                            eprintln!("Error handling request: {}", e);
                        }
                    }
                    Err(e) => {
                        // client probably dropped it
                        // eprintln!("Error reading request: {}", e);
                        break;
                    }
                }
            }
            Ok(Err(e)) => {
                eprintln!("Error waiting for stream to become readable: {}", e);
                break;
            }
            Err(_) => {
                println!("Connection timed out due to inactivity.");
                break;
            }
        }

    }


    match client_stream.shutdown().await {
        Ok(_) => {},
        Err(e) => {
            // eprintln!("Error shutting down client stream: {}", e);
        }
    };

    Ok(())
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
async fn respond_with_internal_error(client_stream: &mut StreamWrapper) -> Result<(), std::io::Error> {
    let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    client_stream.write_all(response.as_bytes()).await
}

/// Takes the stream, responds with a timeout error, and shutdown tcp
async fn respond_with_timeout(client_stream: &mut StreamWrapper) -> Result<(), std::io::Error> {
    let response = "HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    client_stream.write_all(response.as_bytes()).await
}


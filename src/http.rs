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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};

use crate::liberdus;

pub async fn handle_stream(mut client_stream: TcpStream, liberdus: Arc<liberdus::Liberdus>) -> Result<(), Box<dyn std::error::Error>> {
    let mut request_buffer = Vec::new();

    // Set a timeout for reading the request
    match timeout(Duration::from_secs(10), read_or_collect(&mut client_stream, &mut request_buffer)).await {
        Ok(Ok(())) => {
            println!("Successfully read request from client.");
        },
        Ok(Err(e)) => {
            eprintln!("Error reading request: {}", e);
            respond_with_timeout(&mut client_stream).await?;
            client_stream.shutdown().await?;
            return Err(Box::new(e));
        }
        Err(_) => {
            eprintln!("Timeout reading request from client.");
            respond_with_timeout(&mut client_stream).await?;
            client_stream.shutdown().await?;
            return Err("Timeout reading from client".into());
        }
    }


    // Get the next appropriate consensor from liberdus
    let (_, target_server) = match liberdus.get_next_appropriate_consensor().await {
        Some(consensor) => consensor,
        None => {
            eprintln!("No consensors available.");
            respond_with_internal_error(&mut client_stream).await?;
            return Err("No consensors available".into());
        }
    };

    let ip_port = format!("{}:{}", target_server.ip.clone(), target_server.port.clone());

    drop(target_server);

    let mut server_stream = match timeout(Duration::from_secs(5), TcpStream::connect(ip_port)).await {
        Ok(Ok(stream)) => {
            println!("Successfully connected to target server.");
            stream
        },
        Ok(Err(e)) => {
            eprintln!("Error connecting to target server: {}", e);
            respond_with_timeout(&mut client_stream).await?;
            return Err(Box::new(e));
        }
        Err(_) => {
            eprintln!("Timeout connecting to target server.");
            respond_with_timeout(&mut client_stream).await?;
            return Err("Timeout connecting to target server".into());
        }
    };

    // Forward the client's request to the server
    if let Err(e) = server_stream.write_all(&request_buffer).await {
        eprintln!("Error forwarding request to server: {}", e);
        respond_with_internal_error(&mut client_stream).await?;
        return Err(Box::new(e));
    }
    println!("Successfully forwarded request to server.");

    // Collect the entire response from the server
    let mut response_data = Vec::new();
    if let Err(e) = read_or_collect(&mut server_stream, &mut response_data).await {
        eprintln!("Error collecting response from server: {}", e);
        respond_with_internal_error(&mut client_stream).await?;
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


    // Relay the collected response to the client
    // [TODO] should remove Connectin: keep-alive
    if let Err(e) = client_stream.write_all(&response_data).await {
        eprintln!("Error relaying response to client: {}", e);
        respond_with_internal_error(&mut client_stream).await?;
        return Err(Box::new(e));
    }

    client_stream.shutdown().await?;

    Ok(())
}

/// Reads from the stream until the end of the headers or the end of the body if the Content-Length
/// header is present. The data is collected into the buffer.
/// Chunked encoding is not supported. For the purpose of our application this is not necessary, at
/// least at the momment.
async fn read_or_collect(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<(), std::io::Error> {
    let mut temp_buffer = [0; 1024];
    let mut headers_read = false;
    let mut content_length: Option<usize> = None;

    loop {
        let n = stream.read(&mut temp_buffer).await?;
        if n == 0 {
            break; // Stream closed
        }
        buffer.extend_from_slice(&temp_buffer[..n]);

        // Parse headers to determine content length
        if !headers_read {
            if let Some(headers_end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                headers_read = true;
                let headers = &buffer[..headers_end + 4];
                content_length = parse_content_length(headers);
            }
        }

        // Stop reading if content length is known and body is fully read
        if let Some(length) = content_length {
            let body_start = buffer.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(0) + 4;
            if buffer.len() >= body_start + length {
                break;
            }
        }
    }

    Ok(())
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    if let Ok(headers_str) = std::str::from_utf8(headers) {
        for line in headers_str.lines() {
            if let Some(value) = line.strip_prefix("Content-Length:") {
                return value.trim().parse::<usize>().ok();
            }
        }
    }
    None
}

/// Takes the stream, responds with a 500 Internal Server Error, and shutdown tcp
async fn respond_with_internal_error(client_stream: &mut TcpStream) -> Result<(), std::io::Error> {
    let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    client_stream.write_all(response.as_bytes()).await
}

/// Takes the stream, responds with a timeout error, and shutdown tcp
async fn respond_with_timeout(client_stream: &mut TcpStream) -> Result<(), std::io::Error> {
    let response = "HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    client_stream.write_all(response.as_bytes()).await
}


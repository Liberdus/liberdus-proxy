use tokio::net::TcpStream;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};

use crate::liberdus;

pub async fn handle_client(mut client_stream: TcpStream, liberdus: Arc<liberdus::Liberdus>) -> Result<(), Box<dyn std::error::Error>> {
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

    let mut keep_alive = false;
    if request_buffer.windows(17).any(|w| w == b"Connection: keep-alive") {
        keep_alive = true;
        println!("Client requested keep-alive.");
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
        });
        return Err(Box::new(e));
    }

    tokio::spawn(async move {
        server_stream.shutdown().await.unwrap();
    });


    // Relay the collected response to the client
    if let Err(e) = client_stream.write_all(&response_data).await {
        eprintln!("Error relaying response to client: {}", e);
        respond_with_internal_error(&mut client_stream).await?;
        return Err(Box::new(e));
    }

    if !keep_alive {
        println!("Successfully relayed response to client. Closing client connection.");
        client_stream.shutdown().await?;
    }

    Ok(())
}

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

async fn respond_with_internal_error(client_stream: &mut TcpStream) -> Result<(), std::io::Error> {
    let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    client_stream.write_all(response.as_bytes()).await
}

async fn respond_with_timeout(client_stream: &mut TcpStream) -> Result<(), std::io::Error> {
    let response = "HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    client_stream.write_all(response.as_bytes()).await
}


// src/notifier.rs
use crate::http;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::time::Duration;
use tokio::time::timeout;
use tokio::net::TcpStream;
use crate::config::Config;

pub async fn handle_request<S>(
    mut request_buffer: Vec<u8>,
    client_stream: &mut S,
    config: Arc<Config>,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: AsyncWrite + AsyncRead + Unpin + Send,
{
    let (mut header, body) = http::split_head_body(&request_buffer);
    header = http::strip_route_root(&header);
    request_buffer = http::join_head_body(&header, &body);

    let ip_port = format!(
        "{}:{}",
        config.notifier.ip.clone(),
        config.notifier.port.clone()
    );

    let mut server_stream = match timeout(
        Duration::from_millis(config.max_http_timeout_ms as u64),
        TcpStream::connect(ip_port),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            eprintln!("Error connecting to notifier api server: {}", e);
            http::respond_with_timeout(client_stream).await?;
            return Err(Box::new(e));
        }
        Err(_) => {
            eprintln!("Timeout connecting to notifier api server.");
            http::respond_with_timeout(client_stream).await?;
            return Err("Timeout connecting to notifier api server".into());
        }
    };

    // Forward the client's request to the server
    let mut response_data = vec![];
    match timeout(
        Duration::from_millis(config.max_http_timeout_ms as u64),
        server_stream.write_all(&request_buffer),
    )
    .await
    {
        Ok(Ok(())) => match http::collect_http(&mut server_stream, &mut response_data).await {
            Ok(()) => {
                tokio::spawn(async move {
                    server_stream.shutdown().await.unwrap();
                    drop(server_stream);
                });
            }
            Err(e) => {
                eprintln!("Error reading response from notifier api server: {}", e);
                http::respond_with_internal_error(client_stream).await?;
                tokio::spawn(async move {
                    server_stream.shutdown().await.unwrap();
                    drop(server_stream);
                });
                return Err(Box::new(e));
            }
        },
        Ok(Err(e)) => {
            eprintln!("Error forwarding request to notifier api server: {}", e);
            http::respond_with_internal_error(client_stream).await?;
            tokio::spawn(async move {
                server_stream.shutdown().await.unwrap();
                drop(server_stream);
            });
            return Err(Box::new(e));
        }
        Err(_) => {
            eprintln!("Timeout forwarding request to notifier api server.");
            http::respond_with_timeout(client_stream).await?;
            tokio::spawn(async move {
                server_stream.shutdown().await.unwrap();
                drop(server_stream);
            });
            return Err("Timeout forwarding request to notifier api server".into());
        }
    }
    println!("Successfully forwarded request to notifier api server.");

    drop(request_buffer);

    if response_data.is_empty() {
        eprintln!("Empty response from notifier api server.");
        http::respond_with_internal_error(client_stream).await?;
        return Err("Empty response from notifier api server".into());
    }

    http::set_http_header(&mut response_data, "Connection", "keep-alive");
    http::set_http_header(
        &mut response_data,
        "Keep-Alive",
        format!("timeout={}", config.tcp_keepalive_time_sec).as_str(),
    );
    // http::set_http_header(&mut response_data, "Access-Control-Allow-Origin", "*");

    // Relay the collected response to the client
    if let Err(e) = client_stream.write_all(&response_data).await {
        eprintln!("Error relaying response to client: {}", e);
        http::respond_with_internal_error(client_stream).await?;
        return Err(Box::new(e));
    }

    return Ok(());
}

pub fn is_notifier_route(route: &str) -> bool {
    route.starts_with("/notifier")
}

/*
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use std::sync::RwLock;
    use crate::config::Config;

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_notifier_get_request() {
        let config = Config::load().unwrap();
        let mut buffer = vec![];
        buffer.extend_from_slice(b"GET /notifier/api/some_endpoint HTTP/1.1\r\nHost: example.com\r\n\r\n");

        let mut stream = tokio::io::sink();
        let result = handle_request(buffer, &mut stream, Arc::new(config)).await;

        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_notifier_post_request() {
        let config = Config::load().unwrap();
        let mut buffer = vec![];
        buffer.extend_from_slice(b"POST /notifier/api/some_endpoint HTTP/1.1\r\nHost: example.com\r\nContent-Length: 13\r\n\r\nHello, world!");
        buffer.extend_from_slice(b"Hello, world!");

        let mut stream = tokio::io::sink();
        let result = handle_request(buffer, &mut stream, Arc::new(config)).await;

        assert!(result.is_ok());
    }
}
*/
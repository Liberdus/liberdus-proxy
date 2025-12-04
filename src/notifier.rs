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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    use crate::config::{Config, LocalSource, NodeFilteringConfig, NotifierConfig, ShardusMonitorProxyConfig, StandaloneNetworkConfig, TLSConfig};

    fn test_config(port: u16) -> Config {
        Config {
            http_port: 0,
            crypto_seed: String::new(),
            archiver_seed_path: String::new(),
            nodelist_refresh_interval_sec: 1,
            debug: true,
            max_http_timeout_ms: 1_000,
            tcp_keepalive_time_sec: 1,
            standalone_network: StandaloneNetworkConfig {
                replacement_ip: "127.0.0.1".into(),
                enabled: false,
            },
            node_filtering: NodeFilteringConfig {
                enabled: false,
                remove_top_nodes: 0,
                remove_bottom_nodes: 0,
                min_nodes_for_filtering: 0,
            },
            tls: TLSConfig {
                enabled: false,
                cert_path: String::new(),
                key_path: String::new(),
            },
            shardus_monitor: ShardusMonitorProxyConfig {
                enabled: false,
                upstream_ip: String::new(),
                upstream_port: 0,
                https: false,
            },
            local_source: LocalSource {
                collector_api_ip: "127.0.0.1".into(),
                collector_api_port: 0,
                collector_event_server_ip: String::new(),
                collector_event_server_port: 0,
            },
            notifier: NotifierConfig {
                ip: "127.0.0.1".into(),
                port,
            },
        }
    }

    async fn run_request(buffer: Vec<u8>) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Arc::new(test_config(port));

        let server = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 256];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });

        let (mut client, mut peer) = tokio::io::duplex(1024);
        let handle = tokio::spawn(async move {
            handle_request(buffer, &mut client, config)
                .await
                .expect("request should succeed");
        });

        let mut output = vec![0u8; 256];
        let n = peer.read(&mut output).await.unwrap();

        handle.await.unwrap();
        server.await.unwrap();

        output[..n].to_vec()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_notifier_get_request() {
        let buffer = b"GET /notifier/api/some_endpoint HTTP/1.1\r\nHost: example.com\r\n\r\n".to_vec();
        let response = run_request(buffer).await;
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_notifier_post_request() {
        let mut buffer = b"POST /notifier/api/some_endpoint HTTP/1.1\r\nHost: example.com\r\nContent-Length: 13\r\n\r\nHello, world!".to_vec();
        buffer.extend_from_slice(b"Hello, world!");
        let response = run_request(buffer).await;
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"));
    }
}
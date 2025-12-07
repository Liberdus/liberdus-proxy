use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use liberdus_proxy::{collector, config};

fn test_config(port: u16) -> Arc<config::Config> {
    Arc::new(config::Config {
        http_port: 0,
        crypto_seed: "64f152869ca2d473e4ba64ab53f49ccdb2edae22da192c126850970e788af347".into(),
        archiver_seed_path: String::new(),
        nodelist_refresh_interval_sec: 1,
        debug: true,
        max_http_timeout_ms: 1_000,
        tcp_keepalive_time_sec: 1,
        standalone_network: config::StandaloneNetworkConfig {
            replacement_ip: "127.0.0.1".into(),
            enabled: false,
        },
        node_filtering: config::NodeFilteringConfig {
            enabled: false,
            remove_top_nodes: 0,
            remove_bottom_nodes: 0,
            min_nodes_for_filtering: 0,
        },
        tls: config::TLSConfig {
            enabled: false,
            cert_path: String::new(),
            key_path: String::new(),
        },
        shardus_monitor: config::ShardusMonitorProxyConfig {
            enabled: false,
            upstream_ip: String::new(),
            upstream_port: 0,
            https: false,
        },
        local_source: config::LocalSource {
            collector_api_ip: "127.0.0.1".into(),
            collector_api_port: port,
            collector_event_server_ip: String::new(),
            collector_event_server_port: 0,
        },
        notifier: config::NotifierConfig {
            ip: String::new(),
            port: 0,
        },
    })
}

async fn spawn_mock_server(body: &str) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
        body.as_bytes().len(),
        body
    );

    let handle = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer).await;
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });

    (addr, handle)
}

#[tokio::test]
async fn transaction_history_end_to_end() {
    let body = "{\"success\":true,\"transactions\":[{\"originalTxData\":{\"tx\":{\"foo\":1}},\"txId\":\"abc\"}]}";
    let (addr, server) = spawn_mock_server(body).await;

    let config = test_config(addr.port());
    let result = collector::get_transaction_history(
        &config.local_source.collector_api_ip,
        &config.local_source.collector_api_port,
        &"account".to_string(),
    )
    .await
    .unwrap();

    server.await.unwrap();

    assert_eq!(
        result,
        serde_json::json!({"transactions":[{"foo":1, "txId":"abc"}]})
    );
}

#[tokio::test]
async fn get_receipt_smoke_test() {
    let body = "{\"receipt\":true}";
    let (addr, server) = spawn_mock_server(body).await;

    let result = collector::get_receipt("127.0.0.1", &addr.port(), "tx")
        .await
        .unwrap();
    server.await.unwrap();

    assert_eq!(result, serde_json::json!({"receipt": true}));
}

#[tokio::test]
async fn collector_handle_request_round_trip() {
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = test_config(port);

    let server_task = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(response).await;
        }
    });

    let request = b"GET /collector/demo HTTP/1.1\r\nHost: local\r\n\r\n".to_vec();
    let (mut client, mut peer) = tokio::io::duplex(4096);
    let handle = tokio::spawn(async move {
        collector::handle_request(request, &mut client, config)
            .await
            .expect("request should succeed");
    });

    let mut received = vec![0u8; 256];
    let n = peer.read(&mut received).await.unwrap();

    handle.await.unwrap();
    server_task.await.unwrap();

    let text = String::from_utf8_lossy(&received[..n]);
    assert!(text.starts_with("HTTP/1.1 200 OK"));
    assert!(text.contains("Keep-Alive: timeout="));
}

use std::sync::Arc;

use tokio::io::AsyncReadExt;

use liberdus_proxy::config;
use liberdus_proxy::coordinator_gateway;

fn test_config(coordinator_url: Option<String>) -> config::Config {
    config::Config {
        http_port: 0,
        crypto_seed: String::new(),
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
            collector_api_ip: String::new(),
            collector_api_port: 0,
            collector_event_server_ip: String::new(),
            collector_event_server_port: 0,
        },
        notifier: config::NotifierConfig {
            ip: String::new(),
            port: 0,
        },
        coordinator_url,
    }
}

fn post_request_with_body(body: &str) -> Vec<u8> {
    let mut buf = b"POST /notify-bridgeout HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: "
        .to_vec();
    buf.extend_from_slice(body.len().to_string().as_bytes());
    buf.extend_from_slice(b"\r\n\r\n");
    buf.extend_from_slice(body.as_bytes());
    buf
}

async fn run_handle_request(request_buffer: Vec<u8>, config: Arc<config::Config>) -> Vec<u8> {
    let (mut client, mut peer) = tokio::io::duplex(1024);
    let config_clone = Arc::clone(&config);
    let handle = tokio::spawn(async move {
        let _ = coordinator_gateway::handle_request(request_buffer, &mut client, config_clone).await;
    });
    let mut output = vec![0u8; 512];
    let n = peer.read(&mut output).await.unwrap();
    handle.await.unwrap();
    output[..n].to_vec()
}

#[test]
fn test_is_notify_bridgeout_route() {
    assert!(coordinator_gateway::is_notify_bridgeout_route("POST", "/notify-bridgeout"));
    assert!(coordinator_gateway::is_notify_bridgeout_route("post", "/notify-bridgeout"));
    assert!(coordinator_gateway::is_notify_bridgeout_route("POST", "/notify-bridgeout?x=1"));
    assert!(!coordinator_gateway::is_notify_bridgeout_route("GET", "/notify-bridgeout"));
    assert!(!coordinator_gateway::is_notify_bridgeout_route("POST", "/other"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_handle_request_valid_and_errors() {
    let config_ok = Arc::new(test_config(Some("http://127.0.0.1:0".into())));
    let config_no_url = Arc::new(test_config(None));

    // Valid body → 200 accepted
    let res = run_handle_request(post_request_with_body(r#"{"chainId":80002}"#), config_ok.clone()).await;
    let text = String::from_utf8_lossy(&res);
    assert!(text.starts_with("HTTP/1.1 200 OK") && text.contains(r#"{"Ok":"accepted"}"#), "{}", text);

    // Invalid bodies → 400
    for invalid_body in ["", "not json", r#"{"other":123}"#, r#"{"chainId":"80002"}"#] {
        let res = run_handle_request(post_request_with_body(invalid_body), config_ok.clone()).await;
        let text = String::from_utf8_lossy(&res);
        assert!(text.starts_with("HTTP/1.1 400"), "body {:?} should get 400, got: {}", invalid_body, text);
    }

    // No coordinator_url → 503
    let res = run_handle_request(post_request_with_body(r#"{"chainId":80002}"#), config_no_url).await;
    let text = String::from_utf8_lossy(&res);
    assert!(text.starts_with("HTTP/1.1 503") && text.contains("coordinator_url not configured"), "{}", text);
}

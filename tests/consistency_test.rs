use arc_swap::ArcSwap;
use liberdus_proxy::{archivers, config, crypto, liberdus};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_nodelist_consistency_lockstep() {
    let (pk, sk) = sodiumoxide::crypto::sign::gen_keypair();
    let crypto = Arc::new(crypto::ShardusCrypto::new(
        "64f152869ca2d473e4ba64ab53f49ccdb2edae22da192c126850970e788af347",
    ));

    // Shared version counter for the Mock Archiver
    let current_version = Arc::new(AtomicUsize::new(0));

    // 1. Start Deterministic Mock Archiver
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let crypto_clone = crypto.clone();
    let server_version = current_version.clone();

    let server_handle = tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let crypto = crypto_clone.clone();
                let pk = pk;
                let sk = sk.clone();
                let ver = server_version.load(Ordering::SeqCst);

                tokio::spawn(async move {
                    // Read request headers (drain)
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;

                    // Generate deterministic node list based on version
                    let mut nodes = Vec::new();
                    nodes.push(liberdus::Consensor {
                        foundationNode: Some(false),
                        id: format!("version_{}", ver),
                        ip: format!("127.0.0.{}", ver % 255),
                        port: 8000 + (ver as u16),
                        publicKey: format!("pk_{}", ver),
                        rng_bias: None,
                    });

                    // Sign it
                    let resp = signed_node_list_with_keys(&crypto, &pk, &sk, nodes);
                    let json = serde_json::to_string(&resp).unwrap();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        json.len(),
                        json
                    );

                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        }
    });

    // 2. Setup Liberdus
    let archiver = archivers::Archiver {
        ip: "127.0.0.1".into(),
        port,
        publicKey: "00".into(), // Mock PK
    };
    let archivers = Arc::new(ArcSwap::from_pointee(vec![archiver]));

    let mut config = config::Config::load().unwrap_or(config::Config {
        http_port: 0,
        crypto_seed: "64f152869ca2d473e4ba64ab53f49ccdb2edae22da192c126850970e788af347".into(),
        archiver_seed_path: String::new(),
        nodelist_refresh_interval_sec: 1,
        debug: false,
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
    });
    config.max_http_timeout_ms = 500;
    config.nodelist_refresh_interval_sec = 1;

    let liberdus = Arc::new(liberdus::Liberdus::new(crypto.clone(), archivers, config));

    // 3. Lock-step Test Loop
    let num_readers = 10;
    let iterations = 50;

    for i in 1..=iterations {
        // A. Set Archiver Version
        current_version.store(i, Ordering::SeqCst);

        // B. Update Liberdus (this awaits until the list is fetched and stored)
        liberdus.update_active_nodelist().await;

        // C. Spawn 10 Readers to Verify
        let mut handles = Vec::new();
        for _r in 0..num_readers {
            let lib_ref = liberdus.clone();
            let expected_version = i;

            let h = tokio::spawn(async move {
                let nodes = lib_ref.active_nodelist.load_full();

                assert!(!nodes.is_empty(), "Reader got empty list");
                let node = &nodes[0];

                // Assert ID matches version
                assert_eq!(
                    node.id,
                    format!("version_{}", expected_version),
                    "Reader saw stale or incorrect version"
                );

                // Assert IP/Port matches version
                assert_eq!(node.port, 8000 + (expected_version as u16));
            });
            handles.push(h);
        }

        // D. Await all readers to ensure they saw the correct state *before* we proceed to next update
        for h in handles {
            h.await.unwrap();
        }
    }

    println!(
        "Successfully verified {} iterations with {} lock-step readers each.",
        iterations, num_readers
    );
    server_handle.abort();
}

fn signed_node_list_with_keys(
    crypto: &crypto::ShardusCrypto,
    pk: &sodiumoxide::crypto::sign::PublicKey,
    sk: &sodiumoxide::crypto::sign::SecretKey,
    nodes: Vec<liberdus::Consensor>,
) -> liberdus::SignedNodeListResp {
    let unsigned_msg = serde_json::json!({
        "nodeList": nodes.clone(),
    });

    let hash = crypto.hash(&unsigned_msg.to_string().into_bytes(), crypto::Format::Hex);
    let sig_bytes = crypto
        .sign(hash, sk)
        .expect("signature")
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    liberdus::SignedNodeListResp {
        nodeList: nodes,
        sign: liberdus::Signature {
            owner: sodiumoxide::hex::encode(pk.as_ref()),
            sig: sig_bytes,
        },
    }
}

//! # Liberdus proxy server
//! This is a simple proxy server that forwards requests to the Liberdus consensus node (validators). The service does nothing special except picking the appropriate node to forward the request to. Aim to minimize clients having to track the nodes themselves.
//! 
//! For additional features like real time chat room subscription, data distribution protocol integration and muc more consistent API, please use liberdus-rpc.
//! 
//! The underlying algorithem that pick a node based on biased random selection is identical to the one used in liberdus-rpc.
//! 
//! # Have Cargo setup on your system
//! ```bash
//! curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
//! ```
//! # Clone the repository
//! ```bash
//! git clone [link]
//! ```
//! Provide seed archiver in `./src/seed_archiver.json`.
//! 
//! # Standalone network
//! This is for cases where you have the entire network running on a remote machine that's different than the proxy server, such that archiver will a list of validator with loop back ips since they're on same machine. But the list will break the proxy due to loopback ips. In this case, you can use the standalone network mode.
//! 
//! Configure it in `src/config.json`
//! ```json
//! {
//!     "standalone_network": {
//!         "enabled": true,
//!         "replacement_ip": "[ip of the machine that house the network you want to connect]"
//!     }
//! }
//! ```
//! 
//! Configure the seed archiver in `./src/seed_archiver.json` to have the same ip and credentials.
//! 
//! Note that standalone network may never be used in production.
//! 
//! # Run the server
//! ```bash
//! cargo run
//! ```
mod http;
mod liberdus;
mod archivers;
mod crypto;
mod config;
mod tls;
mod ws;
mod subscription;
mod shardus_monitor;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::fs;
use tokio_rustls::TlsAcceptor;

struct Stats {
    pub stream_count: Arc<AtomicUsize>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _configs = config::Config::load().unwrap_or_else(|err| {
        eprintln!("Failed to load config: {}", err);
        std::process::exit(1);
    });

    let tls_config = if _configs.tls.enabled {
            match tls::configure_tls(
                &_configs.tls.cert_path, 
                &_configs.tls.key_path) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("Failed to configure TLS: {}", e);
                    std::process::exit(1);
                }
            }
    } else {
        None
    };

    let tls_acceptor = match tls_config.is_some() {
        true => {
            let tls_config = tls_config.unwrap();
            let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));
            Some(tls_acceptor)
        },
        false => {
            None
        }
    };




    let archiver_seed_data = fs::read_to_string(&_configs.archiver_seed_path)
        .map_err(|err| format!("Failed to read archiver seed file: {}", err))
        .unwrap();

    let archiver_seed: Vec<archivers::Archiver> = serde_json::from_str(&archiver_seed_data).unwrap();

    let crypto = Arc::new(crypto::ShardusCrypto::new(
        &_configs.crypto_seed,
    ));

    let arch_utils = Arc::new(archivers::ArchiverUtil::new(
        crypto.clone(),
        archiver_seed,
        _configs.clone(),
    ));

    let lbd = Arc::new(liberdus::Liberdus::new(
        crypto.clone(),
        arch_utils.get_active_archivers(),
        _configs.clone(),
    ));

    let _archivers = Arc::clone(&arch_utils);
    let _liberdus = Arc::clone(&lbd);

    tokio::spawn(async move {
        Arc::clone(&_archivers).discover().await;
        _liberdus.update_active_nodelist().await;

        let mut ticker = tokio::time::interval(
            tokio::time::Duration::from_secs(_configs.nodelist_refresh_interval_sec),
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            Arc::clone(&_archivers).discover().await;
            _liberdus.update_active_nodelist().await;
        }
    });

    println!("Waiting for active nodelist...");
    loop {
        if lbd.active_nodelist.read().await.len() > 0 {
            break;
        }
    }


    let config = Arc::new(_configs);

    let server_stats = Arc::new(Stats {
        stream_count: Arc::new(AtomicUsize::new(0)),
    });

    let _stats = Arc::clone(&server_stats);

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(tokio::time::Duration::from_millis(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            print!("\rActive Connection Streams: {:<3}",_stats.stream_count.load(std::sync::atomic::Ordering::Relaxed));
        }
    });

    let liberdus_http = Arc::clone(&lbd);
    let config_http = Arc::clone(&config);
    let server_stats_http = Arc::clone(&server_stats);
    let tls_http = tls_acceptor.clone();


    let pid = std::process::id();
    println!("PID: {}", pid);

    let http_listener_task = tokio::spawn(async move {
        http::listen(
            liberdus_http,
            config_http,
            server_stats_http,
            tls_http,
        ).await;
    });

    let ws_liberdus = Arc::clone(&lbd);
    let ws_config = Arc::clone(&config);
    let ws_server_stats = Arc::clone(&server_stats);
    let ws_tls = tls_acceptor.clone();
    let websocket_listener_task = tokio::spawn(async move {
        ws::listen(
            ws_liberdus,
            ws_config,
            ws_server_stats,
            ws_tls,
        ).await;
    });


    tokio::try_join!(http_listener_task, websocket_listener_task)?;

    Ok(())

}

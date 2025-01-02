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

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use std::fs;

struct Stats {
    pub stream_count: Arc<AtomicUsize>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _configs = config::Config::load().unwrap_or_else(|err| {
        eprintln!("Failed to load config: {}", err);
        std::process::exit(1);
    });

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

    let listener = TcpListener::bind(format!("0.0.0.0:{}", _configs.http_port.clone())).await?;
    println!("Listening on: {}", listener.local_addr()?);
    let pid = std::process::id();
    println!("PID: {}", pid);

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


    // let semaphore = Arc::new(Semaphore::new(300));

    loop {
        // let throttler = Arc::clone(&semaphore);
        let (stream, _) = listener.accept().await?;

        // if semaphore.available_permits() == 0 {
        //     tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        //     continue;
        // }

        let liberdus = Arc::clone(&lbd);
        let config = Arc::clone(&config);
        let stats = Arc::clone(&server_stats);

        tokio::spawn(async move {
            // let permit = throttler.acquire().await.unwrap();
            stats.stream_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let e = http::handle_connection(stream, liberdus, config).await;
            // if let Err(e) = e {
            //     eprintln!("Error: {}", e);
            // }
            stats.stream_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            // drop(permit);
        });

    }
}

//! Liberdus proxy server is a payload router between client and liberdus network.
//! It is responsible for handling client requests and forwarding them to the appropriate Validator.
//! The underlying algorithm used to do weighted random pick of validators is identical to the one
//! in liberdus rpc server.
//! Major difference between RPC and proxy server is that proxy merely route the request to the one
//! of the validator. 
//! For more features such as websocket chat room subscription, a more consistent response bodies, 
//! data sourcing from data distribution protocol, and retries, use the RPC server.
//! # Run proxy server for developement
//! ```bash
//! cargo run
//! ```
mod http;
mod liberdus;
mod archivers;
mod crypto;
mod config;

use std::sync::Arc;
use tokio::net::TcpListener;
use std::fs;

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

    let listener = TcpListener::bind(format!("0.0.0.0:{}", _configs.http_port)).await?;
    println!("Listening on: {}", listener.local_addr()?);
    let pid = std::process::id();
    println!("PID: {}", pid);

    loop {
        let (stream, _) = listener.accept().await?;
        let liberdus = Arc::clone(&lbd);

        tokio::spawn(async move {
            let e = http::handle_client(stream, liberdus).await;
            if let Err(e) = e {
                eprintln!("Error: {}", e);
            }
        });
    }
}

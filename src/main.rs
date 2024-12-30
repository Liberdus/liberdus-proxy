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
        "69fa4195670576c0160d660c3be36556ff8d504725be8a59b5a96509e0c994bc",
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

    let listener = TcpListener::bind("127.0.0.1:3030").await?;
    println!("Proxy server running on http://127.0.0.1:3030");

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

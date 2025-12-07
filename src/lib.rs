use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

#[derive(Clone)]
pub struct Stats {
    pub stream_count: Arc<AtomicUsize>,
}

pub mod archivers;
pub mod swap_cell;
pub mod collector;
pub mod config;
pub mod crypto;
pub mod http;
pub mod liberdus;
pub mod notifier;
pub mod rpc;
pub mod shardus_monitor;
pub mod subscription;
pub mod tls;
pub mod ws;

pub use config::Config;

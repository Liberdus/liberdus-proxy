//! Configuration for the rpc server.
use std::fs;

#[derive(Debug, serde::Deserialize, Clone)]
pub struct Config {
    /// The port on which the proxy server will listen
    pub http_port: u16,

    /// cryptographic seed
    pub crypto_seed: String,

    /// This is a system directory path to the archiver seed file
    /// Archiver seed file should contain a list of reputable and reliable node
    pub archiver_seed_path: String,

    /// The interval in seconds at which the node list will be refreshed
    pub nodelist_refresh_interval_sec: u64,

    /// This is currently not used anywhere
    pub debug: bool,

    /// The maximum time in milliseconds that the rpc will cancel a request when communication with
    /// consensus node
    pub max_http_timeout_ms: u128,

    /// TCP keep alive time in seconds
    pub tcp_keepalive_time_sec: u32,

    /// Standalone network configuration
    pub standalone_network: StandaloneNetworkConfig,

    /// Node filtering configuration for join-ordered lists
    pub node_filtering: NodeFilteringConfig,

    pub tls: TLSConfig,

    pub shardus_monitor: ShardusMonitorProxyConfig,

    pub local_source: LocalSource,

    pub notifier: NotifierConfig,
}

#[derive(Debug, serde::Deserialize, Clone)]
/// Standalone network mean that consensus node and archivers reside within same server.
/// This mean archiver will returns node list with loopback ip address 0.0.0.0, 127.0.0.1, localhost.
/// When rpc is on a separate machine, loopback ips of nodes will not work.
/// Usually standalone network config are for testnets. Should not be used in productions
pub struct StandaloneNetworkConfig {
    pub replacement_ip: String,
    pub enabled: bool,
}

#[derive(Debug, serde::Deserialize, Clone)]
/// Node filtering configuration for join-ordered node lists
/// This helps avoid unstable nodes that are joining or leaving the network
pub struct NodeFilteringConfig {
    /// Whether to enable node filtering based on join order
    pub enabled: bool,
    /// Number of nodes to remove from the top of the join-ordered list
    pub remove_top_nodes: usize,
    /// Number of nodes to remove from the bottom of the join-ordered list
    pub remove_bottom_nodes: usize,
    /// Minimum number of nodes required before filtering is applied
    pub min_nodes_for_filtering: usize,
}

#[derive(Debug, serde::Deserialize, Clone)]
/// TLS configuration
pub struct TLSConfig {
    pub enabled: bool,
    pub cert_path: String,
    pub key_path: String,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct ShardusMonitorProxyConfig {
    pub enabled: bool,
    pub upstream_ip: String,
    pub upstream_port: u16,
    pub https: bool,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct LocalSource {
    pub collector_api_ip: String,
    pub collector_api_port: u16,
    pub collector_event_server_ip: String,
    pub collector_event_server_port: u16,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct NotifierConfig {
    pub ip: String,
    pub port: u16,
}

/// Load the configuration from the config json file
/// path is src/config.json
impl Config {
    pub fn load() -> Result<Self, String> {
        let config_file = "src/config.json".to_string();

        let config_data = fs::read_to_string(&config_file)
            .map_err(|err| format!("Failed to read config file: {}", err))?;

        serde_json::from_str(&config_data)
            .map_err(|err| format!("Failed to parse config file: {}", err))
    }
}

impl Default for Config {
    fn default() -> Self {
        Config::load().expect("Default config should load from src/config.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reads_default_config() {
        let cfg = Config::load().expect("config should parse");
        assert_eq!(cfg.http_port, 3030);
        assert!(cfg.debug);
        assert!(!cfg.tls.enabled);
        assert_eq!(cfg.node_filtering.remove_top_nodes, 3);
    }
}

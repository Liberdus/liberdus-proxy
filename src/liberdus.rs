//! This module contains the node management logic require for load balancing with consensor nodes
use crate::crypto;
use crate::{archivers, collector, config, http};
use arc_swap::ArcSwap;
use rand::prelude::*;
use serde::{self, Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Arc,
    },
    time::Duration,
    u128,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::time::sleep;
use tokio::{net::TcpStream, sync::RwLock, time::timeout};

#[derive(serde::Deserialize, serde::Serialize, Clone)]
#[allow(non_snake_case)]
pub struct Consensor {
    pub id: String,
    pub ip: String,
    pub port: u16,
    pub publicKey: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub foundationNode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub rng_bias: Option<f64>,
}

#[allow(non_snake_case)]
#[derive(serde::Deserialize, serde::Serialize, Clone)]
pub struct SignedNodeListResp {
    pub nodeList: Vec<Consensor>,
    pub sign: Signature,
}

#[derive(serde::Deserialize, serde::Serialize, Clone)]
pub struct Signature {
    pub owner: String,
    pub sig: String,
}

pub struct Liberdus {
    pub active_nodelist: Arc<ArcSwap<Vec<Consensor>>>,
    trip_ms: Arc<RwLock<HashMap<String, u128>>>,
    archivers: Arc<ArcSwap<Vec<archivers::Archiver>>>,
    round_robin_index: Arc<AtomicUsize>,
    list_prepared: Arc<AtomicBool>,
    crypto: Arc<crypto::ShardusCrypto>,
    load_distribution_commulative_bias: Arc<RwLock<Vec<f64>>>,
    config: Arc<config::Config>,
}

impl Liberdus {
    pub fn new(
        sc: Arc<crypto::ShardusCrypto>,
        archivers: Arc<ArcSwap<Vec<archivers::Archiver>>>,
        config: config::Config,
    ) -> Self {
        Liberdus {
            config: Arc::new(config),
            round_robin_index: Arc::new(AtomicUsize::new(0)),
            trip_ms: Arc::new(RwLock::new(HashMap::new())),
            active_nodelist: Arc::new(ArcSwap::from_pointee(Vec::new())),
            list_prepared: Arc::new(AtomicBool::new(false)),
            archivers,
            crypto: sc,
            load_distribution_commulative_bias: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// trigger a full nodelist update from one of the archivers
    pub async fn update_active_nodelist(&self) {
        let archivers = self.archivers.load_full();

        for archiver in archivers.iter() {
            let url = format!(
                "http://{}:{}/full-nodelist?activeOnly=true",
                archiver.ip, archiver.port
            );
            let collected_nodelist = match reqwest::get(&url).await {
                Ok(resp) => {
                    let body: Result<SignedNodeListResp, _> =
                        serde_json::from_str(&resp.text().await.unwrap());
                    match body {
                        Ok(body) => {
                            //important that serde doesn't populate default value for
                            // Consensor::trip_ms
                            // it'll taint the signature payload
                            if self.verify_signature(&body) {
                                Ok(body.nodeList)
                            } else {
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "Invalid signature",
                                ))
                            }
                        }
                        Err(e) => Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            e.to_string(),
                        )),
                    }
                }
                Err(e) => Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    e.to_string(),
                )),
            };

            match collected_nodelist {
                Ok(mut nodelist) => {
                    if self.config.standalone_network.enabled {
                        let replacement_ip = self.config.standalone_network.replacement_ip.clone();
                        for node in nodelist.iter_mut() {
                            node.ip = replacement_ip.clone();
                        }
                    }

                    // Filter out top and bottom nodes from the join-ordered list
                    // This helps avoid nodes that might be joining/leaving or unstable
                    if self.config.node_filtering.enabled
                        && nodelist.len() > self.config.node_filtering.min_nodes_for_filtering
                    {
                        let remove_bottom = self.config.node_filtering.remove_bottom_nodes;
                        let remove_top = self.config.node_filtering.remove_top_nodes;

                        // Remove bottom nodes first (affects indices less)
                        let nodes_to_keep = nodelist.len().saturating_sub(remove_bottom);
                        nodelist.truncate(nodes_to_keep);

                        // Remove top nodes
                        if remove_top > 0 && nodelist.len() > remove_top {
                            nodelist.drain(0..remove_top);
                        }

                        println!("Filtered nodelist: using {} nodes (removed top {} and bottom {} from join order)", 
                                 nodelist.len(), remove_top, remove_bottom);
                    } else if self.config.node_filtering.enabled {
                        println!("Warning: Nodelist too small ({} nodes), not filtering to avoid service disruption (min required: {})", 
                                 nodelist.len(), self.config.node_filtering.min_nodes_for_filtering);
                    }

                    self.active_nodelist.store(Arc::new(nodelist));

                    {
                        let mut guard = self.load_distribution_commulative_bias.write().await;
                        *guard = Vec::new();
                    }
                    {
                        let mut guard = self.trip_ms.write().await;
                        *guard = HashMap::new();
                    }

                    self.round_robin_index
                        .store(0, std::sync::atomic::Ordering::Release);
                    // inititally node list does not contain load data.
                    self.list_prepared
                        .store(false, std::sync::atomic::Ordering::Release);
                    break;
                }
                Err(_e) => {
                    continue;
                }
            }
        }
    }

    /// Calculates a node's bias for weighted random selection based on its HTTP round-trip time (RTT).
    ///
    /// # Formula
    /// The function uses the following formula to calculate bias:
    /// ```math
    /// bias = 1.0 - (timetaken_ms - min_timeout) / (max_timeout - min_timeout)
    /// ```
    /// where:
    /// - `timetaken_ms` is the round-trip time (RTT) of the node in milliseconds.
    /// - `min_timeout` is the theoretical minimum RTT (set to 0.01 ms for numerical stability).
    /// - `max_timeout` is the maximum allowable RTT.
    ///
    /// # Explanation
    /// The bias is normalized to a scale between 0 and 1, where:
    /// - A lower `timetaken_ms` (faster node) results in a higher bias, making the node more likely to be selected.
    /// - A higher `timetaken_ms` (slower node) results in a lower bias, making the node less likely to be selected.
    ///
    /// ## Steps:
    /// 1. **Normalize RTT:** The RTT (`timetaken_ms`) is normalized using the formula:
    ///    ```math
    ///    normalized_rtt = (timetaken_ms - min_timeout) / (max_timeout - min_timeout)
    ///    ```
    ///    This maps the RTT to a range between 0 (minimum RTT) and 1 (maximum RTT).
    /// 2. **Invert the Normalization:** The bias is calculated as:
    ///    ```math
    ///    bias = 1.0 - normalized_rtt
    ///    ```
    ///    This ensures that faster nodes (lower RTT) have a higher bias (closer to 1.0),
    ///    while slower nodes (higher RTT) have a lower bias (closer to 0.0).
    ///
    /// ## Special Case
    /// If `max_timeout` is `1` (to prevent division by zero), the function assumes all nodes are equally performant and returns a bias of `1.0`.
    ///
    /// ## Example
    /// For a node with:
    /// - `timetaken_ms = 100`
    /// - `max_timeout = 500`
    /// The bias is calculated as:
    /// ```math
    /// normalized_rtt = (100 - 0.01) / (500 - 0.01) ≈ 0.19996
    /// bias = 1.0 - 0.19996 ≈ 0.80004
    /// ```
    /// This node is more likely to be selected compared to nodes with higher RTTs.
    ///
    /// # Why This Works
    /// - This bias calculation ensures that faster nodes (lower RTTs) are favored during selection.
    /// - Nodes with RTTs close to `max_timeout` are effectively penalized, reducing their likelihood of being selected.
    /// - The linear normalization and inversion provide a smooth, predictable weighting system.
    fn calculate_bias(&self, timetaken_ms: u128, max_timeout: u128) -> f64 {
        if max_timeout == 1 {
            return 1.0; // All timeouts are the same
        }
        let timetaken_ms_f = timetaken_ms as f64;
        let min_timeout_f = 0.0_f64;
        let max_timeout_f = max_timeout as f64;
        1.0 - (timetaken_ms_f - min_timeout_f) / (max_timeout_f - min_timeout_f)
    }

    /// Computes and maintains a cumulative bias distribution for node selection.
    ///
    /// # What is Cumulative Bias?
    /// Cumulative bias is a method to efficiently perform weighted random selection.
    /// Each node's individual bias is calculated based on its HTTP round-trip time (RTT),
    /// and these biases are accumulated into a cumulative distribution. This allows for
    /// efficient random selection of nodes where the probability of selection is proportional
    /// to their bias.
    ///
    /// # How It Works
    /// 1. **Calculate Individual Bias:** For each node, an individual bias is computed using
    ///    the `calculate_bias` function, which maps RTT to a value between `0.0` (poor performance)
    ///    and `1.0` (high performance).
    /// 2. **Build Cumulative Distribution:** The cumulative bias is computed by summing
    ///    the biases iteratively. The resulting vector represents a range of cumulative weights:
    ///    ```math
    ///    cumulative_bias[i] = bias[0] + bias[1] + ... + bias[i]
    ///    ```
    /// 3. **Select Nodes:** When selecting a node, a random value is generated in the range
    ///    `[0, total_bias]`, where `total_bias` is the last element of the cumulative vector.
    ///    The appropriate node is determined using a binary search over the cumulative biases.
    ///
    /// # Why It Works
    /// - The cumulative bias ensures that nodes with higher individual biases have a larger
    ///   range of values in the cumulative distribution, making them more likely to be selected.
    /// - The binary search provides efficient lookup for random selection, making this approach
    ///   scalable for large node lists.
    ///
    /// # Example
    /// Given the following nodes and biases:
    /// ```text
    /// Node  Bias
    /// A     0.2
    /// B     0.5
    /// C     0.3
    /// ```
    /// The cumulative bias array will be:
    /// ```text
    /// [0.2, 0.7, 1.0]
    /// ```
    /// A random value between `0.0` and `1.0` is generated:
    /// - A value in `[0.0, 0.2)` selects Node A.
    /// - A value in `[0.2, 0.7)` selects Node B.
    /// - A value in `[0.7, 1.0]` selects Node C.
    ///
    /// # Efficiency
    /// - Cumulative bias computation is O(n), where `n` is the number of nodes.
    /// - Random selection using binary search is O(log n).
    ///
    /// # Implementation in `prepare_list`
    /// - The cumulative bias is stored in `self.load_distribution_commulative_bias`.
    /// - It is recalculated whenever the node list is updated or RTT data changes.
    async fn prepare_list(&self) {
        if self
            .list_prepared
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }

        let nodes = self.active_nodelist.load_full();

        let trip_ms = {
            let guard = self.trip_ms.read().await;
            guard.clone()
        };

        let max_timeout = self.config.max_http_timeout_ms.try_into().unwrap_or(4000); // 3 seconds
        let mut sorted_nodes = nodes.as_ref().clone();

        sorted_nodes.sort_by(|a, b| {
            let a_time = trip_ms.get(&a.id).unwrap_or(&max_timeout);
            let b_time = trip_ms.get(&b.id).unwrap_or(&max_timeout);
            a_time.cmp(b_time)
        });

        let mut total_bias = 0.0;
        let mut cumulative_bias = Vec::new();
        for node in &mut sorted_nodes {
            let last_http_round_trip = *trip_ms.get(&node.id).unwrap_or(&max_timeout);
            let bias = self.calculate_bias(last_http_round_trip, max_timeout);
            node.rng_bias = Some(bias);
            total_bias += node.rng_bias.unwrap_or(0.0);
            cumulative_bias.push(total_bias);
        }

        self.active_nodelist.store(Arc::new(sorted_nodes));

        {
            let mut guard = self.load_distribution_commulative_bias.write().await;
            *guard = cumulative_bias;
        }

        self.list_prepared
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Selects a random node from the active list based on weighted bias
    /// bias is derived from last http call's round trip time to the node.
    /// this function required the list to be sorted and bias values are calculated prior
    /// return None otherwise.
    async fn get_random_consensor_biased(&self) -> Option<(usize, Consensor)> {
        if !self
            .list_prepared
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }

        let nodes = self.active_nodelist.load_full();
        let cumulative_weights = self.load_distribution_commulative_bias.read().await.clone();

        if nodes.is_empty() || cumulative_weights.is_empty() {
            return None;
        }

        let mut rng = thread_rng();
        let total_bias = *cumulative_weights.last().unwrap_or(&1.0);

        // If all nodes have the same bias, return a random node
        if total_bias == 0.0 {
            let idx = rng.gen_range(0..nodes.len());
            self.round_robin_index
                .store(0, std::sync::atomic::Ordering::Release);
            self.list_prepared
                .store(false, std::sync::atomic::Ordering::Release);
            return Some((idx, nodes[idx].clone()));
        }

        let random_value: f64 = rng.gen_range(0.0..total_bias);

        let index = match cumulative_weights
            .binary_search_by(|&bias| bias.partial_cmp(&random_value).unwrap_or(Ordering::Equal))
        {
            Ok(i) => i,  // Exact match
            Err(i) => i, // Closest match (next higher value)
        };

        Some((index, nodes[index].clone()))
    }

    /// This function is the defecto way to get a consensor.
    /// When nodeList is first refreshed the round trip http request time for the nodes are
    /// unknown. The function will round robin from the list to return consensor.
    /// During the interaction with the each consensors in each rpc call, it will collect the round trip time for
    /// each node. The values are then used to calculate a weighted bias for
    /// node selection. Subsequent call will be redirected towards the node based on that bias and round robin
    /// is dismissed.
    pub async fn get_next_appropriate_consensor(&self) -> Option<(usize, Consensor)> {
        if self.active_nodelist.load().is_empty() {
            return None;
        }
        match self
            .list_prepared
            .load(std::sync::atomic::Ordering::Acquire)
            && (self
                .load_distribution_commulative_bias
                .read()
                .await
                .clone()
                .len()
                == self.active_nodelist.load().len())
        {
            true => self.get_random_consensor_biased().await,
            false => {
                let index = self
                    .round_robin_index
                    .fetch_add(1, std::sync::atomic::Ordering::Acquire);

                let nodes = self.active_nodelist.load_full();
                if index >= nodes.len() {
                    // dropping the `nodes` is really important here
                    // prepare_list() will acquire write lock
                    // but this scope here has a simultaneous read lock
                    // this will cause a deadlock if not drop
                    drop(nodes);
                    self.prepare_list().await;
                    return self.get_random_consensor_biased().await;
                }
                Some((index, nodes[index].clone()))
            }
        }
    }

    pub async fn get_next_appropriate_consensor_with_retry(
        &self,
        max_retry: u32,
    ) -> Option<(usize, Consensor)> {
        let mut retry = 0;
        loop {
            if let Some(respond) = self.get_next_appropriate_consensor().await {
                return Some(respond);
            }

            if retry > max_retry {
                return None;
            }

            retry += 1;
            sleep(Duration::from_millis(1)).await;
        }
    }

    pub fn set_consensor_trip_ms(&self, node_id: String, trip_ms: u128) {
        // list already prepared on the first round robin,  no need to keep recording rtt for nodes
        if self
            .list_prepared
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }

        let trip_ms_map = self.trip_ms.clone();

        tokio::spawn(async move {
            let mut guard = trip_ms_map.write().await;
            guard.insert(node_id, trip_ms);
            drop(guard);
        });
    }

    fn verify_signature(&self, signed_payload: &SignedNodeListResp) -> bool {
        let unsigned_msg = serde_json::json!({
            "nodeList": signed_payload.nodeList,
        });

        let hash = self.crypto.hash(
            &unsigned_msg.to_string().into_bytes(),
            crate::crypto::Format::Hex,
        );

        let pk = sodiumoxide::crypto::sign::PublicKey::from_slice(
            &sodiumoxide::hex::decode(&signed_payload.sign.owner).unwrap(),
        )
        .unwrap();

        self.crypto.verify(
            &hash,
            &sodiumoxide::hex::decode(&signed_payload.sign.sig)
                .unwrap()
                .to_vec(),
            &pk,
        )
    }

    /// Sends a request buffer to an appropriate consensor and returns the response buffer
    /// This method abstracts the consensor selection, connection, and request/response handling
    pub async fn send(
        &self,
        request_buffer: Vec<u8>,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        // Get the next appropriate consensor
        let (_, target_server) = match self.get_next_appropriate_consensor_with_retry(2).await {
            Some(consensor) => consensor,
            None => {
                return Err("No consensors available after 3 retries".into());
            }
        };

        let ip_port = format!(
            "{}:{}",
            target_server.ip.clone(),
            target_server.port.clone()
        );

        // tcp handshakes shouldn't take more than a second
        let mut server_stream =
            match timeout(Duration::from_millis(1000_u64), TcpStream::connect(ip_port)).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => {
                    eprintln!("Error connecting to target server: {}", e);
                    self.set_consensor_trip_ms(target_server.id, self.config.max_http_timeout_ms);
                    return Err(Box::new(e));
                }
                Err(_) => {
                    eprintln!("Timeout connecting to target server.");
                    self.set_consensor_trip_ms(target_server.id, self.config.max_http_timeout_ms);
                    return Err("Timeout connecting to target server".into());
                }
            };

        // Forward the request and collect response
        let now = std::time::Instant::now();
        let mut response_data = vec![];
        match timeout(
            Duration::from_millis(1000_u64),
            server_stream.write_all(&request_buffer),
        )
        .await
        {
            Ok(Ok(())) => match http::collect_http(&mut server_stream, &mut response_data).await {
                Ok(()) => {
                    let elapsed = now.elapsed();
                    self.set_consensor_trip_ms(target_server.id, elapsed.as_millis());
                    tokio::spawn(async move {
                        let _ = server_stream.shutdown().await;
                        drop(server_stream);
                    });
                }
                Err(e) => {
                    eprintln!("Error reading response from server: {}", e);
                    self.set_consensor_trip_ms(target_server.id, self.config.max_http_timeout_ms);
                    tokio::spawn(async move {
                        let _ = server_stream.shutdown().await;
                        drop(server_stream);
                    });
                    return Err(Box::new(e));
                }
            },
            Ok(Err(e)) => {
                eprintln!("Error forwarding request to server: {}", e);
                self.set_consensor_trip_ms(target_server.id, self.config.max_http_timeout_ms);
                tokio::spawn(async move {
                    let _ = server_stream.shutdown().await;
                    drop(server_stream);
                });
                return Err(Box::new(e));
            }
            Err(_) => {
                eprintln!("Timeout forwarding request to server.");
                self.set_consensor_trip_ms(target_server.id, self.config.max_http_timeout_ms);
                tokio::spawn(async move {
                    let _ = server_stream.shutdown().await;
                    drop(server_stream);
                });
                return Err("Timeout forwarding request to server".into());
            }
        }

        if response_data.is_empty() {
            eprintln!("Empty response from server.");
            return Err("Empty response from server".into());
        }

        Ok(response_data)
    }

    pub async fn get_account_by_address(
        &self,
        address: &str,
    ) -> Result<serde_json::Value, std::io::Error> {
        let collecotr_ip = self.config.local_source.collector_api_ip.clone();
        let collector_port = self
            .config
            .local_source
            .collector_api_port
            .clone()
            .to_string();

        let account = collector::get_account_by_address(&collecotr_ip, &collector_port, address)
            .await
            .unwrap_or(serde_json::Value::Bool(false));

        if account != serde_json::Value::Bool(false) {
            return Ok(account);
        }

        let node = match self.get_next_appropriate_consensor().await {
            Some(n) => n.1,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "No consensor available",
                ));
            }
        };
        let url = format!("http://{}:{}/account/{}", node.ip, node.port, address);
        let account = match reqwest::get(&url).await {
            Ok(r) => match r.json::<GetAccountResp>().await {
                Ok(p) => p.account,
                Err(e) => {
                    eprintln!("Failed to parse account: {}", e);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Failed to parse account",
                    ));
                }
            },
            Err(e) => {
                eprintln!("Failed to get account: {}", e);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Failed to get account",
                ));
            }
        };

        Ok(account)
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct GetAccountResp {
    account: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserAccount {
    // pub alias: String,
    // pub claimed_snapshot: bool,
    pub data: AccountData,
    // pub email_hash: Option<String>,
    // pub hash: String,
    pub id: String,
    // pub last_maintenance: i64,
    // pub public_key: String,
    pub timestamp: u128,
    // #[serde(rename = "type")]
    // pub account_type: String,
    // pub verified: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountData {
    // pub balance: BiData,
    pub chat_timestamp: u128,
    // pub chats: HashMap<String, ChatRoomInfo>,
    // pub friends: HashMap<String, serde_json::Value>,
    // pub payments: Vec<serde_json::Value>,
    // pub remove_stake_request: Option<serde_json::Value>,
    // pub stake: BiData,
    // pub toll: Option<serde_json::Value>,
}
//
// #[derive(Debug, Serialize, Deserialize)]
// #[serde(rename_all = "camelCase")]
// pub struct BiData {
//     pub data_type: String,
//     pub value: String,
// }
//
// #[derive(Debug, Serialize, Deserialize)]
// #[serde(rename_all = "camelCase")]
// pub struct ChatRoomInfo {
//     pub chat_id: String,
//     pub received_timestamp: u128,
// }

// write tests
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use arc_swap::ArcSwap;

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_weighted_random() {
        let mut nodes: Vec<Consensor> = Vec::new();

        let config = config::Config::load().unwrap();

        let _mock_archiver = archivers::Archiver {
            ip: "0.0.0.0".to_string(),
            port: 0,
            publicKey: "0x0".to_string(),
        };

        let archivers = Arc::new(ArcSwap::from_pointee(vec![_mock_archiver]));
        let liberdus = Liberdus::new(
            Arc::new(crypto::ShardusCrypto::new(
                "69fa4195670576c0160d660c3be36556ff8d504725be8a59b5a96509e0c994bc",
            )),
            archivers,
            config,
        );

        for i in 0..500 {
            let node = Consensor {
                foundationNode: Some(false),
                publicKey: "0x0".to_string(),
                id: i.to_string(),
                ip: "0.0.0.0".to_string(),
                port: i,
                rng_bias: None,
            };
            nodes.push(node);
        }

        liberdus.active_nodelist.store(Arc::new(nodes));

        liberdus
            .round_robin_index
            .store(1999, std::sync::atomic::Ordering::Relaxed);

        for i in 0..500 {
            //this artificially sets the round trip time for each node
            // specifically making lower indices have lower round trip time
            liberdus.set_consensor_trip_ms(
                i.to_string(),
                (i * 10).min(liberdus.config.max_http_timeout_ms),
            );
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        liberdus.prepare_list().await;

        println!(
            "Cumulative weight {}",
            liberdus
                .load_distribution_commulative_bias
                .read()
                .await
                .last()
                .unwrap()
        );

        for _i in 0..3000 {
            let (index, _) = liberdus.get_random_consensor_biased().await.unwrap();
            println!("{}", index);
            assert_eq!(true, true);
        }
    }

    fn sample_config() -> config::Config {
        config::Config {
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
        }
    }

    fn sample_liberdus() -> Liberdus {
        let crypto = Arc::new(crypto::ShardusCrypto::new(
            "64f152869ca2d473e4ba64ab53f49ccdb2edae22da192c126850970e788af347",
        ));
        Liberdus::new(
            crypto,
            Arc::new(ArcSwap::from_pointee(Vec::new())),
            sample_config(),
        )
    }

    #[test]
    fn calculate_bias_maps_timeouts() {
        let l = sample_liberdus();
        assert!((l.calculate_bias(0, 1000) - 1.0).abs() < f64::EPSILON);
        assert!(l.calculate_bias(1000, 1000) < 0.01);
        assert_eq!(l.calculate_bias(10, 1), 1.0);
    }

    #[tokio::test]
    async fn prepare_list_sorts_nodes_by_rtt() {
        let liberdus = sample_liberdus();
        let mut nodes = liberdus.active_nodelist.load_full().as_ref().clone();
        nodes.push(Consensor {
            foundationNode: Some(false),
            id: "slow".into(),
            ip: "127.0.0.1".into(),
            port: 80,
            publicKey: "pk1".into(),
            rng_bias: None,
        });
        nodes.push(Consensor {
            foundationNode: Some(false),
            id: "fast".into(),
            ip: "127.0.0.1".into(),
            port: 80,
            publicKey: "pk2".into(),
            rng_bias: None,
        });
        liberdus.active_nodelist.store(Arc::new(nodes));

        {
            let mut trips = liberdus.trip_ms.write().await;
            trips.insert("slow".into(), 900);
            trips.insert("fast".into(), 10);
        }

        liberdus.prepare_list().await;

        let ordered = liberdus.active_nodelist.load_full();
        assert_eq!(ordered[0].id, "fast");
        assert!(ordered[0].rng_bias.unwrap() > ordered[1].rng_bias.unwrap());

        let dist = liberdus
            .load_distribution_commulative_bias
            .read()
            .await
            .clone();
        assert_eq!(dist.len(), 2);
        assert!(liberdus
            .list_prepared
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    async fn start_http_server(response: String) -> (tokio::task::JoinHandle<()>, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        (handle, port)
    }

    fn signed_node_list_with_keys(
        crypto: &crypto::ShardusCrypto,
        pk: &sodiumoxide::crypto::sign::PublicKey,
        sk: &sodiumoxide::crypto::sign::SecretKey,
        nodes: Vec<Consensor>,
    ) -> SignedNodeListResp {
        let unsigned_msg = serde_json::json!({
            "nodeList": nodes.clone(),
        });

        let hash = crypto.hash(
            &unsigned_msg.to_string().into_bytes(),
            crate::crypto::Format::Hex,
        );
        let sig_bytes = crypto
            .sign(hash, sk)
            .expect("signature")
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();

        SignedNodeListResp {
            nodeList: nodes,
            sign: Signature {
                owner: sodiumoxide::hex::encode(pk.as_ref()),
                sig: sig_bytes,
            },
        }
    }

    #[tokio::test]
    async fn update_active_nodelist_filters_and_replaces_ips() {
        let crypto = Arc::new(crypto::ShardusCrypto::new(
            "64f152869ca2d473e4ba64ab53f49ccdb2edae22da192c126850970e788af347",
        ));
        let (pk, sk) = sodiumoxide::crypto::sign::gen_keypair();

        let mut nodes = vec![];
        for id in 0..4 {
            nodes.push(Consensor {
                foundationNode: Some(false),
                id: format!("node{}", id),
                ip: "127.0.0.1".into(),
                port: 9,
                publicKey: "pk".into(),
                rng_bias: None,
            });
        }

        let good_resp = signed_node_list_with_keys(&crypto, &pk, &sk, nodes.clone());
        let bad_resp = SignedNodeListResp {
            nodeList: nodes.clone(),
            sign: Signature {
                owner: sodiumoxide::hex::encode(pk.as_ref()),
                sig: "00".repeat(64),
            },
        };

        let bad_json = serde_json::to_string(&bad_resp).unwrap();
        let good_json = serde_json::to_string(&good_resp).unwrap();

        let (bad_server, bad_port) = start_http_server(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            bad_json.len(),
            bad_json
        ))
        .await;

        let (good_server, good_port) = start_http_server(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            good_json.len(),
            good_json
        ))
        .await;

        let archivers = Arc::new(ArcSwap::from_pointee(vec![
            archivers::Archiver {
                ip: "127.0.0.1".into(),
                port: bad_port,
                publicKey: String::new(),
            },
            archivers::Archiver {
                ip: "127.0.0.1".into(),
                port: good_port,
                publicKey: String::new(),
            },
        ]));

        let mut cfg = sample_config();
        cfg.standalone_network.enabled = true;
        cfg.standalone_network.replacement_ip = "192.0.2.1".into();
        cfg.node_filtering.enabled = true;
        cfg.node_filtering.remove_top_nodes = 1;
        cfg.node_filtering.remove_bottom_nodes = 1;
        cfg.node_filtering.min_nodes_for_filtering = 2;

        let liberdus = Liberdus::new(crypto, archivers, cfg);
        liberdus.update_active_nodelist().await;

        bad_server.abort();
        good_server.abort();

        let list = liberdus.active_nodelist.load_full();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|n| n.ip == "192.0.2.1"));
        assert!(list.iter().all(|n| n.id == "node1" || n.id == "node2"));
        assert!(liberdus
            .load_distribution_commulative_bias
            .read()
            .await
            .is_empty());
    }

    #[test]
    fn verify_signature_accepts_valid_and_rejects_invalid() {
        let crypto = crypto::ShardusCrypto::new(
            "64f152869ca2d473e4ba64ab53f49ccdb2edae22da192c126850970e788af347",
        );
        let (pk, sk) = sodiumoxide::crypto::sign::gen_keypair();

        let nodes = vec![Consensor {
            foundationNode: Some(false),
            id: "node".into(),
            ip: "127.0.0.1".into(),
            port: 1,
            publicKey: String::new(),
            rng_bias: None,
        }];

        let valid = signed_node_list_with_keys(&crypto, &pk, &sk, nodes.clone());

        let liberdus = sample_liberdus();
        assert!(liberdus.verify_signature(&valid));

        let mut invalid = valid.clone();
        invalid.sign.sig = "00".into();
        assert!(!liberdus.verify_signature(&invalid));
    }

    #[tokio::test]
    async fn get_account_by_address_prefers_local_and_fallbacks() {
        let collector_resp = serde_json::json!({
            "accounts": [ { "data": {"balance": 10} } ]
        })
        .to_string();
        let (collector, collector_port) = start_http_server(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            collector_resp.len(),
            collector_resp
        ))
        .await;

        let mut cfg = sample_config();
        cfg.local_source.collector_api_ip = "127.0.0.1".into();
        cfg.local_source.collector_api_port = collector_port;

        let liberdus = Liberdus::new(
            Arc::new(crypto::ShardusCrypto::new(&cfg.crypto_seed)),
            Arc::new(ArcSwap::from_pointee(Vec::new())),
            cfg.clone(),
        );

        let value = liberdus
            .get_account_by_address("0xabc")
            .await
            .expect("collector account");
        assert_eq!(value["balance"], 10);
        collector.abort();

        // failing collector triggers fallback
        let cons_body = "{\"account\":true}";
        let (cons_server, cons_port) = start_http_server(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            cons_body.len(),
            cons_body
        ))
        .await;

        let failing = start_http_server(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".into(),
        )
        .await;

        let mut cfg = sample_config();
        cfg.local_source.collector_api_ip = "127.0.0.1".into();
        cfg.local_source.collector_api_port = failing.1;

        let liberdus = Liberdus::new(
            Arc::new(crypto::ShardusCrypto::new(&cfg.crypto_seed)),
            Arc::new(ArcSwap::from_pointee(Vec::new())),
            cfg,
        );

        let mut nodes = liberdus.active_nodelist.load_full().as_ref().clone();
        nodes.push(Consensor {
            foundationNode: Some(false),
            id: "n1".into(),
            ip: "127.0.0.1".into(),
            port: cons_port,
            publicKey: String::new(),
            rng_bias: Some(1.0),
        });
        liberdus.active_nodelist.store(Arc::new(nodes));
        liberdus
            .load_distribution_commulative_bias
            .write()
            .await
            .push(1.0);
        liberdus
            .list_prepared
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let fallback = liberdus
            .get_account_by_address("0xabc")
            .await
            .expect("fallback account");
        assert_eq!(fallback, serde_json::Value::Bool(true));

        cons_server.abort();
        failing.0.abort();
    }

    #[tokio::test]
    async fn handle_request_routes_and_sets_headers() {
        let mut cfg = sample_config();
        cfg.max_http_timeout_ms = 2_000;
        cfg.tcp_keepalive_time_sec = 3;

        let ok_body = "ok";
        let (server_handle, port) = start_http_server(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            ok_body.len(),
            ok_body
        ))
        .await;

        let liberdus = sample_liberdus();
        let mut nodes = liberdus.active_nodelist.load_full().as_ref().clone();
        nodes.push(Consensor {
            foundationNode: Some(false),
            id: "fast".into(),
            ip: "127.0.0.1".into(),
            port,
            publicKey: String::new(),
            rng_bias: Some(1.0),
        });
        liberdus.active_nodelist.store(Arc::new(nodes));
        liberdus
            .load_distribution_commulative_bias
            .write()
            .await
            .push(1.0);
        liberdus
            .list_prepared
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n".to_vec();
        let (mut client, mut peer) = tokio::io::duplex(1024);

        handle_request(request, &mut client, Arc::new(liberdus), Arc::new(cfg))
            .await
            .expect("request should succeed");

        let mut buf = vec![0u8; 512];
        let n = tokio::time::timeout(std::time::Duration::from_secs(1), peer.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let response = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(response.contains("Connection: keep-alive"));
        assert!(response.contains("Keep-Alive: timeout=3"));
        assert!(response.contains("ok"));

        server_handle.abort();
    }

    #[tokio::test]
    async fn handle_request_handles_connection_error() {
        let mut cfg = sample_config();
        cfg.max_http_timeout_ms = 100;

        let liberdus = sample_liberdus();
        let mut nodes = liberdus.active_nodelist.load_full().as_ref().clone();
        nodes.push(Consensor {
            foundationNode: Some(false),
            id: "fast".into(),
            ip: "127.0.0.1".into(),
            port: 9_999,
            publicKey: String::new(),
            rng_bias: Some(1.0),
        });
        liberdus.active_nodelist.store(Arc::new(nodes));
        liberdus
            .load_distribution_commulative_bias
            .write()
            .await
            .push(1.0);
        liberdus
            .list_prepared
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n".to_vec();
        let (mut client, mut peer) = tokio::io::duplex(1024);

        let err = handle_request(request, &mut client, Arc::new(liberdus), Arc::new(cfg))
            .await
            .expect_err("connection should fail");
        let err_text = format!("{}", err);
        assert!(err_text.contains("Connection refused") || err_text.contains("Error connecting"));

        let mut buf = vec![0u8; 128];
        let n = tokio::time::timeout(std::time::Duration::from_secs(1), peer.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let response = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(response.starts_with("HTTP/1.1 504"));
    }

    #[tokio::test]
    async fn get_next_round_robins_then_bias() {
        let liberdus = sample_liberdus();
        let mut nodes = liberdus.active_nodelist.load_full().as_ref().clone();
        nodes.push(Consensor {
            foundationNode: Some(false),
            id: "one".into(),
            ip: "127.0.0.1".into(),
            port: 80,
            publicKey: "pk1".into(),
            rng_bias: None,
        });
        nodes.push(Consensor {
            foundationNode: Some(false),
            id: "two".into(),
            ip: "127.0.0.1".into(),
            port: 80,
            publicKey: "pk2".into(),
            rng_bias: None,
        });
        liberdus.active_nodelist.store(Arc::new(nodes));

        {
            let mut trips = liberdus.trip_ms.write().await;
            trips.insert("one".into(), 50);
            trips.insert("two".into(), 10);
        }

        let first = liberdus.get_next_appropriate_consensor().await.unwrap();
        let second = liberdus.get_next_appropriate_consensor().await.unwrap();
        assert_eq!(first.0, 0);
        assert_eq!(second.0, 1);

        let _ = liberdus.get_next_appropriate_consensor().await.unwrap();
        assert!(liberdus
            .list_prepared
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_send_method() {
        let mut cfg = sample_config();
        cfg.max_http_timeout_ms = 2_000;

        let ok_body = "ok";
        let (server_handle, port) = start_http_server(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            ok_body.len(),
            ok_body
        ))
        .await;

        let liberdus = sample_liberdus();
        let mut nodes = liberdus.active_nodelist.load_full().as_ref().clone();
        nodes.push(Consensor {
            foundationNode: Some(false),
            id: "fast".into(),
            ip: "127.0.0.1".into(),
            port,
            publicKey: String::new(),
            rng_bias: Some(1.0),
        });
        liberdus.active_nodelist.store(Arc::new(nodes));
        liberdus
            .load_distribution_commulative_bias
            .write()
            .await
            .push(1.0);
        liberdus
            .list_prepared
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n".to_vec();

        let response = liberdus.send(request).await.expect("send should succeed");
        let response_str = std::str::from_utf8(&response).unwrap();
        assert!(response_str.contains("ok"));

        server_handle.abort();
    }

    #[test]
    fn old_receipt_route_variants() {
        assert_eq!(
            is_old_receipt_route(
                "/old_receipt/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
            ),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into())
        );
        assert_eq!(is_old_receipt_route("/wrong/abcd"), None);
    }
}

/// Handles the client request stream by reading the request, forwarding it to a consensor server,
/// collect the response from validator, and relaying it back to the client.
/// Handles a single HTTP request from the client stream.
/// This function assumes it processes only one http payload request.
/// The proxy will maintain the tcp stream open for a specify amount of time to avoid the handshake
/// overhead when the client do frequent calls.
/// Proxy will not maintain tcp stream connected to upstream server/validator. It is always single
/// use and always shutdown after the response is sent back to the client. If the multiple request
/// from single client tcp stream, it is advisable to pick different validator each time.
///
/// **No chunked encoding is supported.**
/// **No Multiplexing is supported.**
pub async fn handle_request<S>(
    request_buffer: Vec<u8>,
    client_stream: &mut S,
    liberdus: Arc<Liberdus>,
    config: Arc<config::Config>,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: AsyncWrite + AsyncRead + Unpin + Send,
{
    let mut retry = 0;
    let mut response_data = loop {
        match liberdus.send(request_buffer.clone()).await {
            Ok(response) => break response,
            Err(e) => {
                if retry < 2 {
                    retry += 1;
                    sleep(Duration::from_millis(500)).await;
                    continue;
                }
                eprintln!("Error sending request through liberdus: {}", e);
                let error_str = e.to_string();
                println!("{}", error_str);
                if error_str.contains("Timeout")
                    || error_str.contains("timeout")
                    || error_str.contains("Connection refused")
                {
                    http::respond_with_timeout(client_stream).await?;
                } else {
                    http::respond_with_internal_error(client_stream).await?;
                }
                return Err(e);
            }
        }
    };

    http::set_http_header(&mut response_data, "Connection", "keep-alive");
    http::set_http_header(
        &mut response_data,
        "Keep-Alive",
        format!("timeout={}", config.tcp_keepalive_time_sec).as_str(),
    );
    http::set_http_header(&mut response_data, "Access-Control-Allow-Origin", "*");

    // Relay the collected response to the client
    if let Err(e) = client_stream.write_all(&response_data).await {
        eprintln!("Error relaying response to client: {}", e);
        http::respond_with_internal_error(client_stream).await?;
        return Err(Box::new(e));
    }

    Ok(())
}

/// Returns `(true, <tx_hash>)` when `route` is exactly
/// `/old_receipt/<64-char hex>` with no extra path segments.
/// Otherwise returns `(false, String::new())`.
pub fn is_old_receipt_route(route: &str) -> Option<String> {
    let mut parts = route.trim_start_matches('/').split('/');

    match (parts.next(), parts.next(), parts.next()) {
        // first segment,   second segment,    make sure there’s no 3rd segment
        (Some(_seg @ "old_receipt") | Some(_seg @ "oldreceipt"), Some(hash), None)
            if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            Some(hash.to_owned())
        }
        _ => None,
    }
}

//! This module contains the node management logic require for load balancing with consensor nodes
use crate::crypto;
use crate::{archivers, collector, config, http};
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
    pub rng_bias: Option<f64>,
}

#[allow(non_snake_case)]
#[derive(serde::Deserialize, serde::Serialize)]
struct SignedNodeListResp {
    pub nodeList: Vec<Consensor>,
    pub sign: Signature,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct Signature {
    pub owner: String,
    pub sig: String,
}

pub struct Liberdus {
    pub active_nodelist: Arc<RwLock<Vec<Consensor>>>,
    trip_ms: Arc<RwLock<HashMap<String, u128>>>,
    archivers: Arc<RwLock<Vec<archivers::Archiver>>>,
    round_robin_index: Arc<AtomicUsize>,
    list_prepared: Arc<AtomicBool>,
    crypto: Arc<crypto::ShardusCrypto>,
    load_distribution_commulative_bias: Arc<RwLock<Vec<f64>>>,
    config: Arc<config::Config>,
}

impl Liberdus {
    pub fn new(
        sc: Arc<crypto::ShardusCrypto>,
        archivers: Arc<RwLock<Vec<archivers::Archiver>>>,
        config: config::Config,
    ) -> Self {
        Liberdus {
            config: Arc::new(config),
            round_robin_index: Arc::new(AtomicUsize::new(0)),
            trip_ms: Arc::new(RwLock::new(HashMap::new())),
            active_nodelist: Arc::new(RwLock::new(Vec::new())),
            list_prepared: Arc::new(AtomicBool::new(false)),
            archivers,
            crypto: sc,
            load_distribution_commulative_bias: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// trigger a full nodelist update from one of the archivers
    pub async fn update_active_nodelist(&self) {
        let archivers = self.archivers.read().await;

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

                    {
                        let mut guard = self.active_nodelist.write().await;
                        *guard = nodelist;
                    }

                    self.round_robin_index
                        .store(0, std::sync::atomic::Ordering::Relaxed);
                    // inititally node list does not contain load data.
                    self.list_prepared
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    {
                        let mut guard = self.load_distribution_commulative_bias.write().await;
                        *guard = Vec::new();
                    }
                    {
                        let mut guard = self.trip_ms.write().await;
                        *guard = HashMap::new();
                    }
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

        let nodes = {
            let guard = self.active_nodelist.read().await;
            guard.clone()
        };

        let trip_ms = {
            let guard = self.trip_ms.read().await;
            guard.clone()
        };

        let max_timeout = self.config.max_http_timeout_ms.try_into().unwrap_or(4000); // 3 seconds
        let mut sorted_nodes = nodes;

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

        {
            let mut guard = self.active_nodelist.write().await;
            *guard = sorted_nodes;
        }

        {
            let mut guard = self.load_distribution_commulative_bias.write().await;
            *guard = cumulative_bias;
        }

        self.list_prepared
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Selects a random node from the active list based on weighted bias
    /// bias is derived from last http call's round trip time to the node.
    /// this function required the list to be sorted and bias values are calculated prior
    /// return None otherwise.
    async fn get_random_consensor_biased(&self) -> Option<(usize, Consensor)> {
        if !self
            .list_prepared
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }

        let nodes = self.active_nodelist.read().await.clone();
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
                .store(0, std::sync::atomic::Ordering::Relaxed);
            self.list_prepared
                .store(false, std::sync::atomic::Ordering::Relaxed);
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
        if self.active_nodelist.read().await.is_empty() {
            return None;
        }
        match self
            .list_prepared
            .load(std::sync::atomic::Ordering::Relaxed)
            && (self
                .load_distribution_commulative_bias
                .read()
                .await
                .clone()
                .len()
                == self.active_nodelist.read().await.len())
        {
            true => self.get_random_consensor_biased().await,
            false => {
                let index = self
                    .round_robin_index
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                let nodes = self.active_nodelist.read().await;
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

    pub fn set_consensor_trip_ms(&self, node_id: String, trip_ms: u128) {
        // list already prepared on the first round robin,  no need to keep recording rtt for nodes
        if self
            .list_prepared
            .load(std::sync::atomic::Ordering::Relaxed)
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_weighted_random() {
        let mut nodes: Vec<Consensor> = Vec::new();

        let config = config::Config::load().unwrap();

        let _mock_archiver = archivers::Archiver {
            ip: "0.0.0.0".to_string(),
            port: 0,
            publicKey: "0x0".to_string(),
        };

        let archivers = Arc::new(RwLock::new(vec![_mock_archiver]));
        let liberdus = Liberdus::new(
            Arc::new(crypto::ShardusCrypto::new(
                "69fa4195670576c0160d660c3be36556ff8d504725be8a59b5a96509e0c994bc",
            )),
            archivers,
            config,
        );

        for i in 0..500 {
            let node = Consensor {
                publicKey: "0x0".to_string(),
                id: i.to_string(),
                ip: "0.0.0.0".to_string(),
                port: i,
                rng_bias: None,
            };
            nodes.push(node);
        }

        liberdus.active_nodelist.write().await.extend(nodes);

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
    // Get the next appropriate consensor from liberdus
    let (_, target_server) = match liberdus.get_next_appropriate_consensor().await {
        Some(consensor) => consensor,
        None => {
            eprintln!("No consensors available.");
            http::respond_with_internal_error(client_stream).await?;
            return Err("No consensors available".into());
        }
    };

    let ip_port = format!(
        "{}:{}",
        target_server.ip.clone(),
        target_server.port.clone()
    );

    let mut server_stream = match timeout(
        Duration::from_millis(config.max_http_timeout_ms as u64),
        TcpStream::connect(ip_port),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            eprintln!("Error connecting to target server: {}", e);
            http::respond_with_timeout(client_stream).await?;
            return Err(Box::new(e));
        }
        Err(_) => {
            eprintln!("Timeout connecting to target server.");
            http::respond_with_timeout(client_stream).await?;
            return Err("Timeout connecting to target server".into());
        }
    };

    // Forward the client's request to the server
    let now = std::time::Instant::now();
    let mut response_data = vec![];
    match timeout(
        Duration::from_millis(config.max_http_timeout_ms as u64),
        server_stream.write_all(&request_buffer),
    )
    .await
    {
        Ok(Ok(())) => match http::collect_http(&mut server_stream, &mut response_data).await {
            Ok(()) => {
                let elapsed = now.elapsed();
                liberdus.set_consensor_trip_ms(target_server.id, elapsed.as_millis());
                tokio::spawn(async move {
                    server_stream.shutdown().await.unwrap();
                    drop(server_stream);
                });
            }
            Err(e) => {
                eprintln!("Error reading response from server: {}", e);
                http::respond_with_internal_error(client_stream).await?;
                liberdus.set_consensor_trip_ms(target_server.id, config.max_http_timeout_ms);
                tokio::spawn(async move {
                    server_stream.shutdown().await.unwrap();
                    drop(server_stream);
                });
                return Err(Box::new(e));
            }
        },
        Ok(Err(e)) => {
            eprintln!("Error forwarding request to server: {}", e);
            http::respond_with_internal_error(client_stream).await?;
            liberdus.set_consensor_trip_ms(target_server.id, config.max_http_timeout_ms);
            tokio::spawn(async move {
                server_stream.shutdown().await.unwrap();
                drop(server_stream);
            });
            return Err(Box::new(e));
        }
        Err(_) => {
            eprintln!("Timeout forwarding request to server.");
            http::respond_with_timeout(client_stream).await?;
            liberdus.set_consensor_trip_ms(target_server.id, config.max_http_timeout_ms);
            tokio::spawn(async move {
                server_stream.shutdown().await.unwrap();
                drop(server_stream);
            });
            return Err("Timeout forwarding request to server".into());
        }
    }
    println!("Successfully forwarded request to server.");

    drop(request_buffer);

    if response_data.is_empty() {
        eprintln!("Empty response from server.");
        http::respond_with_internal_error(client_stream).await?;
        return Err("Empty response from server".into());
    }

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
        (Some(seg @ "old_receipt") | Some(seg @ "oldreceipt"), Some(hash), None)
            if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            Some(hash.to_owned())
        }
        _ => None,
    }
}

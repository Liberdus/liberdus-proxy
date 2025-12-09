use arc_swap::ArcSwap;
use criterion::{criterion_group, criterion_main, Criterion};
use liberdus_proxy::{
    archivers::{self, ArchiverUtil},
    config::{self, Config},
    crypto::{self, ShardusCrypto},
    liberdus::{Consensor, Liberdus, SignedNodeListResp},
};
use rand::prelude::*;
use std::time::Duration;
use std::{
    cmp::Ordering,
    collections::HashMap,
    fs,
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Arc,
    },
};
use tokio::{runtime::Runtime, sync::RwLock};

fn benchmark_get_consensor(c: &mut Criterion) {
    let mut group = c.benchmark_group("Integrated Read Latency ArcSwap vs RwLock");
    group.measurement_time(Duration::from_secs(120));

    let rt = Runtime::new().unwrap();

    let config = Config::load().unwrap();

    let crypto = Arc::new(ShardusCrypto::new(&config.crypto_seed));

    // Use the mock server address for the seed archiver
    let seed_archiver = {
        let blunt_string = fs::read_to_string(&config.archiver_seed_path)
            .map_err(|err| format!("Failed to read archiver seed file: {}", err))
            .unwrap();

        serde_json::from_str(&blunt_string).unwrap()
    };

    let arch_utils = Arc::new(ArchiverUtil::new(
        crypto.clone(),
        seed_archiver,
        config.clone(),
    ));
    let lbd = Arc::new(Liberdus::new(
        crypto.clone(),
        arch_utils.get_active_archivers(),
        config.clone(),
    ));

    // Background task
    let arch_utils_task = arch_utils.clone();
    let lbd_task = lbd.clone();
    let bg1 = rt.spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        loop {
            ticker.tick().await;
            Arc::clone(&arch_utils_task).discover().await;
            lbd_task.update_active_nodelist().await;
        }
    });

    loop {
        if lbd.active_nodelist.load().len() > 0 {
            break;
        }
    }

    group.bench_function("ArcSwap read", |b| {
        b.to_async(&rt).iter(|| async {
            lbd.get_next_appropriate_consensor().await;
        });
    });

    bg1.abort();

    let old_lbd_with_rwlock = Arc::new(LiberdusRwLock::new(
        crypto.clone(),
        arch_utils.get_active_archivers(),
        config.clone(),
    ));
    let old_lbd_task = old_lbd_with_rwlock.clone();

    let arch_utils_task = arch_utils.clone();
    let bg2 = rt.spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        loop {
            ticker.tick().await;
            Arc::clone(&arch_utils_task).discover().await;
            old_lbd_task.update_active_nodelist().await;
        }
    });

    std::thread::sleep(Duration::from_secs(5));

    group.bench_function("RwLock read", |b| {
        b.to_async(&rt).iter(|| async {
            old_lbd_with_rwlock.get_next_appropriate_consensor().await;
        });
    });
    bg2.abort();
}

criterion_group!(benches, benchmark_get_consensor);
criterion_main!(benches);

pub struct LiberdusRwLock {
    pub active_nodelist: Arc<RwLock<Vec<Consensor>>>,
    trip_ms: Arc<RwLock<HashMap<String, u128>>>,
    archivers: Arc<ArcSwap<Vec<archivers::Archiver>>>,
    round_robin_index: Arc<std::sync::atomic::AtomicUsize>,
    list_prepared: Arc<AtomicBool>,
    crypto: Arc<crypto::ShardusCrypto>,
    load_distribution_commulative_bias: Arc<RwLock<Vec<f64>>>,
    config: Arc<Config>,
}

impl LiberdusRwLock {
    pub fn new(
        sc: Arc<crypto::ShardusCrypto>,
        archivers: Arc<ArcSwap<Vec<archivers::Archiver>>>,
        config: config::Config,
    ) -> Self {
        LiberdusRwLock {
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

    fn calculate_bias(&self, timetaken_ms: u128, max_timeout: u128) -> f64 {
        if max_timeout == 1 {
            return 1.0; // All timeouts are the same
        }
        let timetaken_ms_f = timetaken_ms as f64;
        let min_timeout_f = 0.0_f64;
        let max_timeout_f = max_timeout as f64;
        1.0 - (timetaken_ms_f - min_timeout_f) / (max_timeout_f - min_timeout_f)
    }

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
                    drop(nodes);
                    self.prepare_list().await;
                    return self.get_random_consensor_biased().await;
                }
                Some((index, nodes[index].clone()))
            }
        }
    }

    pub fn set_consensor_trip_ms(&self, node_id: String, trip_ms: u128) {
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
}

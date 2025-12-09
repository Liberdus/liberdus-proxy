use crate::liberdus::Consensor;
use crate::load_balancer::LoadBalancer;
use crate::swap_cell::SwapCell;
use async_trait::async_trait;
use rand::prelude::*;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug)]
pub struct BiasedRandomStrategy {
    trip_ms: Arc<RwLock<HashMap<String, u128>>>,
    load_distribution_commulative_bias: Arc<RwLock<Vec<f64>>>,
    round_robin_index: Arc<AtomicUsize>,
    list_prepared: Arc<AtomicBool>,
    max_timeout_ms: u128,
}

impl BiasedRandomStrategy {
    pub fn new(max_timeout_ms: u128) -> Self {
        BiasedRandomStrategy {
            trip_ms: Arc::new(RwLock::new(HashMap::new())),
            load_distribution_commulative_bias: Arc::new(RwLock::new(Vec::new())),
            round_robin_index: Arc::new(AtomicUsize::new(0)),
            list_prepared: Arc::new(AtomicBool::new(false)),
            max_timeout_ms,
        }
    }

    fn calculate_bias(&self, timetaken_ms: u128, max_timeout: u128) -> f64 {
        if max_timeout == 1 {
            return 1.0;
        }
        let timetaken_ms_f = timetaken_ms as f64;
        let min_timeout_f = 0.0_f64;
        let max_timeout_f = max_timeout as f64;
        1.0 - (timetaken_ms_f - min_timeout_f) / (max_timeout_f - min_timeout_f)
    }

    /// Prepares the list by sorting it based on RTT and calculating biases.
    /// This mimics the original `prepare_list` logic.
    async fn prepare_list(&self, nodelist: &SwapCell<Vec<Consensor>>) {
        if self.list_prepared.load(AtomicOrdering::Relaxed) {
            return;
        }

        // Acquire write lock implicitly by being the only one updating `nodelist` via `publish`?
        // No, `nodelist` is shared.

        // We need to fetch, sort, and re-publish.
        let nodes_arc = nodelist.get_latest();
        let mut sorted_nodes = nodes_arc.as_ref().clone();

        let trip_ms = {
            let guard = self.trip_ms.read().await;
            guard.clone()
        };

        let max_timeout = self.max_timeout_ms;

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

        // Publish the sorted list back to the SwapCell
        nodelist.publish(sorted_nodes);

        {
            let mut guard = self.load_distribution_commulative_bias.write().await;
            *guard = cumulative_bias;
        }

        self.list_prepared
            .store(true, AtomicOrdering::Relaxed);
    }

    async fn get_random_consensor_biased(&self, nodelist: &SwapCell<Vec<Consensor>>) -> Option<(usize, Consensor)> {
         if !self.list_prepared.load(AtomicOrdering::Relaxed) {
            return None;
        }

        let nodes = nodelist.get_latest();
        let cumulative_weights = self.load_distribution_commulative_bias.read().await.clone();

        if nodes.is_empty() || cumulative_weights.is_empty() {
            return None;
        }

        // Safety check: if lengths mismatch, it means list was updated but we haven't re-prepared fully?
        // Or something else updated the list.
        if nodes.len() != cumulative_weights.len() {
             // Invalidate and force re-prep next time?
             // For now, return None to trigger fallback or just fail.
             // But wait, prepare_list updates both.
             // If `update_active_nodelist` happened externally, `on_node_list_update` should have been called
             // clearing `list_prepared`.
             // If `list_prepared` is true, they SHOULD match.
             return None;
        }

        let mut rng = thread_rng();
        let total_bias = *cumulative_weights.last().unwrap_or(&1.0);

        if total_bias == 0.0 {
            let idx = rng.gen_range(0..nodes.len());
            self.round_robin_index.store(0, AtomicOrdering::Relaxed);
            self.list_prepared.store(false, AtomicOrdering::Relaxed);
            return Some((idx, nodes[idx].clone()));
        }

        let random_value: f64 = rng.gen_range(0.0..total_bias);

        let index = match cumulative_weights
            .binary_search_by(|&bias| bias.partial_cmp(&random_value).unwrap_or(Ordering::Equal))
        {
            Ok(i) => i,
            Err(i) => i,
        };

        if index < nodes.len() {
            Some((index, nodes[index].clone()))
        } else {
            None
        }
    }
}

#[async_trait]
impl LoadBalancer for BiasedRandomStrategy {
    async fn select_node(&self, nodelist: &SwapCell<Vec<Consensor>>) -> Option<(usize, Consensor)> {
        let nodes = nodelist.get_latest();
        if nodes.is_empty() {
            return None;
        }

        let prepared = self.list_prepared.load(AtomicOrdering::Relaxed);
        let weights_len = self.load_distribution_commulative_bias.read().await.len();
        let nodes_len = nodes.len();

        // Check if we are ready for biased selection
        if prepared && weights_len == nodes_len {
            if let Some(res) = self.get_random_consensor_biased(nodelist).await {
                return Some(res);
            }
        }

        // Fallback or Initial Round Robin
        let index = self.round_robin_index.fetch_add(1, AtomicOrdering::Relaxed);

        if index >= nodes_len {
            // Need to prepare the list (sort and calc bias)
            drop(nodes); // Release read lock/reference
            self.prepare_list(nodelist).await;

            // Try biased selection now
            self.get_random_consensor_biased(nodelist).await
        } else {
             Some((index, nodes[index].clone()))
        }
    }

    async fn update_rtt(&self, node_id: String, rtt_ms: u128) {
        // If list is already prepared, we don't update RTT anymore (as per original logic logic:
        // "list already prepared on the first round robin, no need to keep recording rtt for nodes")
        if self.list_prepared.load(AtomicOrdering::Relaxed) {
            return;
        }

        let mut guard = self.trip_ms.write().await;
        guard.insert(node_id, rtt_ms);
    }

    async fn on_node_list_update(&self, _nodes: &[Consensor]) {
         self.round_robin_index.store(0, AtomicOrdering::Relaxed);
         self.list_prepared.store(false, AtomicOrdering::Relaxed);
         {
             let mut guard = self.load_distribution_commulative_bias.write().await;
             *guard = Vec::new();
         }
         {
             let mut guard = self.trip_ms.write().await;
             *guard = HashMap::new();
         }
    }
}

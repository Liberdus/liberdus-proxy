use crate::liberdus::Consensor;
use crate::swap_cell::SwapCell;
use async_trait::async_trait;
use std::fmt::Debug;

#[async_trait]
pub trait LoadBalancer: Send + Sync + Debug {
    /// Selects a node from the provided node list according to the strategy.
    /// Returns the index and the node.
    async fn select_node(&self, nodelist: &SwapCell<Vec<Consensor>>) -> Option<(usize, Consensor)>;

    /// Updates the Round Trip Time statistics for a node.
    async fn update_rtt(&self, node_id: String, rtt_ms: u128);

    /// Callback when the node list is updated from an external source (e.g. archivers).
    async fn on_node_list_update(&self, nodes: &[Consensor]);
}

//! Robust query mechanism for validator read routes.
//!
//! Queries multiple validators for the same request, tallies responses
//! for consensus, and returns the majority-agreed response.

use crate::config::Config;
use crate::http;
use crate::liberdus::Liberdus;
use std::collections::HashSet;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

/// One "bucket" in the tally. Tracks a unique response body, how many
/// nodes returned it, and which nodes those were.
#[derive(Debug)]
struct TallyItem {
    /// The HTTP response body bytes (headers stripped).
    body: Vec<u8>,
    /// The full raw HTTP response (headers + body) from the first node
    /// that produced this body. This is what we send back to the client
    /// so that headers like Content-Type are preserved.
    full_response: Vec<u8>,
    /// How many nodes returned a response whose body matches `self.body`.
    count: usize,
    /// IDs of the nodes that returned this response.
    node_ids: Vec<String>,
}

/// Collects responses and counts how many nodes agree on each unique body.
struct Tally {
    /// The number of matching responses needed to declare a winner.
    win_count: usize,
    /// All distinct response buckets seen so far.
    items: Vec<TallyItem>,
}

impl Tally {
    fn new(win_count: usize) -> Self {
        Tally {
            win_count,
            items: Vec::new(),
        }
    }

    /// Add a response. Returns `Some(&TallyItem)` if `win_count` is reached.
    /// Comparison is done on body bytes only (headers stripped).
    fn add(
        &mut self,
        body: Vec<u8>,
        full_response: Vec<u8>,
        node_id: String,
    ) -> Option<&TallyItem> {
        // Check for existing item
        if let Some(index) = self.items.iter().position(|item| item.body == body) {
            let item = &mut self.items[index];
            item.count += 1;
            item.node_ids.push(node_id);
            if item.count >= self.win_count {
                return Some(item);
            }
            return None;
        }

        // No match found, add new
        let new_item = TallyItem {
            body,
            full_response,
            count: 1,
            node_ids: vec![node_id],
        };
        self.items.push(new_item);

        // If win_count is 1, return the item we just pushed
        if self.win_count <= 1 {
            return self.items.last();
        }

        None
    }

    /// Returns the current highest count across all items.
    fn highest_count(&self) -> usize {
        self.items.iter().map(|item| item.count).max().unwrap_or(0)
    }

    /// Returns the TallyItem with the highest count (best-effort fallback).
    fn highest_count_item(&self) -> Option<&TallyItem> {
        self.items.iter().max_by_key(|item| item.count)
    }
}

/// The result of a robust query across multiple validators.
pub struct RobustQueryResult {
    /// The full HTTP response bytes (headers + body) of the winning response.
    /// This is what gets written back to the client stream.
    pub response_data: Vec<u8>,
    /// True if the result met the redundancy threshold.
    /// False means we're returning best-effort (highest count).
    pub is_robust: bool,
}

/// Queries multiple validators for the same request and returns the
/// response that at least `redundancy` nodes agree on.
pub async fn robust_query(
    liberdus: &Liberdus,
    request_buffer: Vec<u8>,
    config: &Config,
) -> Result<RobustQueryResult, Box<dyn std::error::Error + Send + Sync>> {
    let active_nodes = liberdus.active_nodelist.load();
    let available_count = active_nodes.len();
    if available_count == 0 {
        return Err("No nodes available".into());
    }

    // Clamp redundancy to min(config.redundancy, available_nodes).
    let redundancy = std::cmp::min(config.robust_query.redundancy, available_count);
    if redundancy == 0 {
        return Err("Redundancy is 0".into());
    }

    let verbose = config.robust_query.verbose_logs;

    let mut tally = Tally::new(redundancy);
    let mut used_ids = HashSet::new();
    let mut tries = 0;

    if verbose {
        eprintln!(
            "[robust_query] starting: redundancy={}, available_nodes={}, max_retries={}",
            redundancy, available_count, config.robust_query.max_retries
        );
    }

    // Loop until max retries or we run out of nodes to query
    while tries < config.robust_query.max_retries {
        tries += 1;

        let to_query = redundancy - tally.highest_count();

        // Pick `to_query` distinct nodes not already used
        let batch = liberdus
            .get_n_distinct_consensors(to_query, &used_ids)
            .await;

        if batch.is_empty() {
            if verbose {
                eprintln!(
                    "[robust_query] iteration {}: no new unique nodes available, breaking",
                    tries
                );
            }
            break; // ran out of nodes
        }

        if verbose {
            let batch_ids: Vec<&str> = batch.iter().map(|n| n.id.as_str()).collect();
            eprintln!(
                "[robust_query] iteration {}: querying {} nodes: {:?}",
                tries,
                batch.len(),
                batch_ids
            );
        }

        for node in &batch {
            used_ids.insert(node.id.clone());
        }

        // Query all nodes in this batch concurrently
        let mut handles = Vec::new();
        for node in batch {
            let buf = request_buffer.clone();
            let ip_port = format!("{}:{}", node.ip, node.port);
            let node_id = node.id.clone();
            // RTT penalty value for connect/write errors (matches send() behavior)
            let max_timeout = config.max_http_timeout_ms as u64;

            handles.push(tokio::spawn(async move {
                type BoxErr = Box<dyn std::error::Error + Send + Sync>;
                let now = std::time::Instant::now();

                // TCP connect - 1s timeout (matches send())
                let mut server_stream = match timeout(
                    Duration::from_millis(1000),
                    TcpStream::connect(&ip_port),
                )
                .await
                {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        let err: BoxErr = format!("Error connecting: {}", e).into();
                        return (node_id, Err(err), max_timeout as u128);
                    }
                    Err(_) => {
                        let err: BoxErr = "Timeout connecting".into();
                        return (node_id, Err(err), max_timeout as u128);
                    }
                };

                // Write request - 1s timeout (matches send())
                match timeout(Duration::from_millis(1000), server_stream.write_all(&buf)).await {
                    Ok(Ok(())) => {} // write succeeded
                    Ok(Err(e)) => {
                        let _ = server_stream.shutdown().await;
                        let err: BoxErr = format!("Error forwarding request: {}", e).into();
                        return (node_id, Err(err), max_timeout as u128);
                    }
                    Err(_) => {
                        let _ = server_stream.shutdown().await;
                        let err: BoxErr = "Timeout forwarding request".into();
                        return (node_id, Err(err), max_timeout as u128);
                    }
                }

                // Collect response - 60s safety cap so we don't hang forever
                let mut resp_data = Vec::new();
                match timeout(
                    Duration::from_secs(60),
                    http::collect_http(&mut server_stream, &mut resp_data),
                )
                .await
                {
                    Ok(Ok(())) => {
                        let elapsed_ms = now.elapsed().as_millis();
                        let _ = server_stream.shutdown().await;
                        (node_id, Ok(resp_data), elapsed_ms)
                    }
                    Ok(Err(e)) => {
                        let _ = server_stream.shutdown().await;
                        let err: BoxErr = format!("Error reading response: {}", e).into();
                        (node_id, Err(err), max_timeout as u128)
                    }
                    Err(_) => {
                        let _ = server_stream.shutdown().await;
                        let err: BoxErr = "Timeout reading response (60s)".into();
                        (node_id, Err(err), max_timeout as u128)
                    }
                }
            }));
        }

        // Wait for all results in this batch
        let results = futures::future::join_all(handles).await;

        for (node_id, result, elapsed_ms) in results.into_iter().flatten() {
            // Update RTT bias data for future node selection
            liberdus.set_consensor_trip_ms(node_id.clone(), elapsed_ms);

            if let Ok(response_data) = result {
                // Skip empty responses (matches send() behavior)
                if response_data.is_empty() {
                    if verbose {
                        eprintln!(
                            "[robust_query] node {} returned empty response, skipping",
                            node_id
                        );
                    }
                    continue;
                }
                let body = http::extract_body(&response_data);
                if let Some(winner) = tally.add(body, response_data, node_id) {
                    if verbose {
                        eprintln!(
                            "[robust_query] consensus reached: count={}, node_ids={:?}",
                            winner.count, winner.node_ids
                        );
                    }
                    return Ok(RobustQueryResult {
                        response_data: winner.full_response.clone(),
                        is_robust: true,
                    });
                }
            } else if let Err(e) = result {
                if verbose {
                    eprintln!("[robust_query] node {} failed: {}", node_id, e);
                }
            }
        }
    }

    // No consensus reached -- return best-effort
    match tally.highest_count_item() {
        Some(item) => {
            if verbose {
                eprintln!(
                    "[robust_query] no consensus, best-effort: count={}, node_ids={:?}, total_tries={}",
                    item.count, item.node_ids, tries
                );
            }
            Ok(RobustQueryResult {
                response_data: item.full_response.clone(),
                is_robust: false,
            })
        }
        None => {
            if verbose {
                eprintln!(
                    "[robust_query] no responses from any validator after {} tries",
                    tries
                );
            }
            Err("No responses from any validator".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tally_consensus_reached() {
        let mut tally = Tally::new(3);

        // 1. Add first response
        let res1 = tally.add(vec![1, 2, 3], vec![10, 1, 2, 3], "node1".into());
        assert!(res1.is_none());
        assert_eq!(tally.highest_count(), 1);

        // 2. Add second response (match)
        let res2 = tally.add(vec![1, 2, 3], vec![10, 1, 2, 3], "node2".into());
        assert!(res2.is_none());
        assert_eq!(tally.highest_count(), 2);

        // 3. Add third response (match) -> Winner
        let res3 = tally.add(vec![1, 2, 3], vec![10, 1, 2, 3], "node3".into());
        assert!(res3.is_some());
        assert_eq!(res3.unwrap().count, 3);
        assert_eq!(tally.highest_count(), 3);
    }

    #[test]
    fn test_tally_no_consensus() {
        let mut tally = Tally::new(3);

        tally.add(vec![1], vec![], "node1".into());
        tally.add(vec![2], vec![], "node2".into());
        tally.add(vec![1], vec![], "node3".into());

        assert_eq!(tally.highest_count(), 2); // Body [1] has 2

        let best = tally.highest_count_item();
        assert!(best.is_some());
        assert_eq!(best.unwrap().body, vec![1]);
    }

    #[test]
    fn test_tally_single_winner() {
        let mut tally = Tally::new(1);
        let res = tally.add(vec![1], vec![], "node1".into());
        assert!(res.is_some());
    }
}

//! This module provides utilities to interact with a transaction collector API.
//!
//! The module includes:
//! - Fetching a specific transaction by its ID.
//! - Fetching transaction history for a specific account.
//! - (Planned) Fetching messages.
//!
//! # Structures
//! - [`TxResp`]: Represents the API response for transaction queries.
//! - [`Transaction`]: Represents a single transaction.
//! - [`OriginalTxData`]: Represents the original data of a transaction.
//!
//! # Functions
//! - [`get_transaction`]: Fetches a specific transaction by its ID.
//! - [`get_transaction_history`]: Fetches the transaction history for a given account.
//! - [`get_message`]: Placeholder for message-related functionality.
//! - [`insert_field`]: Inserts a key-value pair into a JSON object.

/// Represents the API response for transaction queries.
#[derive(serde::Deserialize)]
struct TxResp {
    /// Indicates if the operation was successful.
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<bool>,

    /// Contains error details if the operation was not successful.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,

    /// A list of transactions returned by the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    transactions: Vec<Transaction>,
}

/// Represents a single transaction.
#[derive(serde::Deserialize)]
struct Transaction {
    /// The original transaction data.
    #[allow(non_snake_case)]
    originalTxData: OriginalTxData,

    /// The unique identifier for the transaction.
    txId: String,
}

/// Represents the original data of a transaction.
#[derive(serde::Deserialize, Clone)]
struct OriginalTxData {
    /// The transaction data as a JSON value.
    tx: serde_json::Value,
}

/// Represents the original data of a transaction.
#[derive(serde::Deserialize, Clone)]
struct CollectorAccountResp {
    /// The transaction data as a JSON value.
    accounts: Vec<serde_json::Value>,
}

/// Fetches a specific transaction by its ID.
///
/// # Parameters
/// - `collector_ip`: The IP address of the transaction collector.
/// - `collector_port`: The port number of the transaction collector.
/// - `id`: The transaction ID to fetch.
///
/// # Returns
/// - `Some(serde_json::Value)` if the transaction is found.
/// - `None` if the transaction is not found or an error occurs.
pub async fn get_transaction(
    collector_ip: &String,
    collector_port: &u16,
    id: &String,
) -> Option<serde_json::Value> {
    let built_url = format!(
        "http://{}:{}/api/transaction?txId={}",
        collector_ip, collector_port, id
    );
    let resp = match reqwest::get(built_url).await {
        Ok(resp) => resp,
        Err(_) => {
            return None;
        }
    };

    let result: Option<TxResp> = match resp.status() {
        reqwest::StatusCode::OK => match resp.json().await {
            Ok(json) => json,
            Err(_) => {
                return None;
            }
        },
        _ => None,
    };

    match result {
        Some(result) => {
            if result.success? && !result.transactions.is_empty() {
                Some(result.transactions[0].originalTxData.tx.clone())
            } else {
                None
            }
        }
        None => None,
    }
}

/// Fetches the transaction history for a specific account.
///
/// # Parameters
/// - `collector_ip`: The IP address of the transaction collector.
/// - `collector_port`: The port number of the transaction collector.
/// - `account_id`: The account ID to fetch transaction history for.
///
/// # Returns
/// - `Ok(serde_json::Value)` containing the transaction history.
/// - `Err(String)` if an error occurs or the operation fails.
pub async fn get_transaction_history(
    collector_ip: &String,
    collector_port: &u16,
    account_id: &String,
) -> Result<serde_json::Value, String> {
    let built_url = format!(
        "http://{}:{}/api/transaction?accountId={}",
        collector_ip, collector_port, account_id
    );
    let resp = match reqwest::get(built_url).await {
        Ok(resp) => resp,
        Err(e) => return Err(e.to_string()),
    };

    let result: TxResp = match resp.status() {
        reqwest::StatusCode::OK => match resp.json().await {
            Ok(json) => json,
            Err(e) => return Err(e.to_string()),
        },
        status => return Err(format!("HTTP error: {}", status)),
    };

    if result.success.unwrap_or(false) {
        let transactions = result
            .transactions
            .iter()
            .map(|tx| {
                let original_tx_data = tx.originalTxData.tx.clone();
                let tx_id = tx.txId.clone();

                insert_field(original_tx_data, "txId", serde_json::json!(tx_id))
            })
            .collect::<Vec<serde_json::Value>>();
        Ok(serde_json::json!({ "transactions": transactions }))
    } else {
        Err(result.error.unwrap_or_else(|| "Unknown error".to_string()))
    }
}

pub async fn get_account_by_address(
    collector_ip: &str,
    collector_port: &str,
    account_id: &str,
) -> Result<serde_json::Value, String> {
    let built_url = format!(
        "http://{}:{}/api/account?accountId={}",
        collector_ip, collector_port, account_id
    );
    let resp = match reqwest::get(built_url).await {
        Ok(resp) => resp,
        Err(e) => return Err(e.to_string()),
    };

    let result: CollectorAccountResp = match resp.status() {
        reqwest::StatusCode::OK => match resp.json().await {
            Ok(json) => json,
            Err(e) => return Err(e.to_string()),
        },
        status => return Err(format!("HTTP error: {}", status)),
    };

    if !result.accounts.is_empty() {
        // grab `data` from the first account
        let account = result.accounts[0].get("data").unwrap();
        Ok(account.clone())
    } else {
        Err("Account not found".to_string())
    }
}

/// Placeholder function for fetching messages.
///
/// # Notes
/// This function is not yet implemented.
pub async fn get_message() {
    todo!()
}

/// Inserts a key-value pair into a JSON object.
///
/// # Parameters
/// - `obj`: The JSON object to modify.
/// - `key`: The key to insert.
/// - `value`: The value to associate with the key.
///
/// # Returns
/// A modified JSON object with the new key-value pair.
fn insert_field(
    mut obj: serde_json::Value,
    key: &str,
    value: serde_json::Value,
) -> serde_json::Value {
    if let Some(map) = obj.as_object_mut() {
        map.insert(key.to_string(), value);
    }
    obj
}

use std::sync::Arc;

use futures::StreamExt;
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};

use crate::subscription;

pub async fn listen_account_update<Fut>(
    subscription_state_manager: Arc<subscription::Manager>,
    callback: fn(serde_json::Value, Arc<subscription::Manager>) -> Fut,
) where
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let server_url = "ws://0.0.0.0:4444".into_client_request().unwrap();

    // keep retrying if fails
    let ws_stream = {
        loop {
            match connect_async(server_url.clone()).await {
                Ok((ws_stream, _)) => {
                    println!("Ready to listen account update from collector");
                    break ws_stream;
                }
                Err(e) => {
                    eprintln!("Failed to connect: {:?}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                }
            }
        }
    };

    let (_, mut read_half) = ws_stream.split();

    // Just listen and print incoming messages
    let subscription_state_manager_long_live = Arc::clone(&subscription_state_manager);
    while let Some(msg) = read_half.next().await {
        let sm = Arc::clone(&subscription_state_manager_long_live);
        match msg {
            Ok(message) => {
                if message.is_text() || message.is_binary() {
                    let data = message.into_data();
                    let json: serde_json::Value = serde_json::from_slice(&data).unwrap();

                    // Call the callback function with the received JSON
                    tokio::spawn(async move {
                        callback(json, sm.clone()).await;
                    });
                } else {
                    println!("Received non-text message");
                }
            }
            Err(e) => {
                eprintln!("WebSocket error: {:?}", e);
                break;
            }
        }
    }
}

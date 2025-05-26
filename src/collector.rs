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

use crate::config::Config;
use crate::http;

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

pub async fn get_receipt(
    collector_ip: &str,
    collector_port: &u16,
    tx_hash: &str,
) -> Result<serde_json::Value, String> {
    let url = format!(
        "http://{}:{}/api/transaction?appReceiptId={}",
        collector_ip, collector_port, tx_hash
    );

    let resp = reqwest::get(&url).await.map_err(|e| e.to_string())?;

    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!("HTTP error: {}", resp.status()));
    }

    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| e.to_string())
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

use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};

use crate::subscription;

pub async fn listen_account_update<Fut>(
    ip: &String,
    port: &u16,
    subscription_state_manager: Arc<subscription::Manager>,
    callback: fn(serde_json::Value, Arc<subscription::Manager>) -> Fut,
) where
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let server_url = format!("ws://{}:{}", ip, port)
        .into_client_request()
        .unwrap();
    loop {
        // keep retrying if fails
        let ws_stream = match connect_async(server_url.clone()).await {
            Ok((ws_stream, _)) => {
                println!("Ready to listen account update from collector");
                ws_stream
            }
            Err(e) => {
                continue;
            }
        };

        let (_, mut read_half) = ws_stream.split();

        // Just listen and print incoming messages
        let subscription_state_manager_long_live = Arc::clone(&subscription_state_manager);
        while let Some(msg) = read_half.next().await {
            let sm = Arc::clone(&subscription_state_manager_long_live);

            match msg {
                Ok(message) => {
                    tokio::spawn(async move {
                        if message.is_text() || message.is_binary() {
                            let data = message.into_data();
                            let json: serde_json::Value = serde_json::from_slice(&data).unwrap();

                            // Call the callback function with the received JSON
                            callback(json, sm.clone()).await;
                        } else {
                            println!("Received non-text message");
                        }
                    });
                }
                Err(e) => {
                    eprintln!("WebSocket error: {:?}", e);
                    break;
                }
            }
        }
    }
}

pub async fn handle_request<S>(
    mut request_buffer: Vec<u8>,
    client_stream: &mut S,
    config: Arc<Config>,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: AsyncWrite + AsyncRead + Unpin + Send,
{
    let (mut header, body) = http::split_head_body(&request_buffer);
    header = http::strip_route_root(&header);
    request_buffer = http::join_head_body(&header, &body);

    let ip_port = format!(
        "{}:{}",
        config.local_source.collector_api_ip.clone(),
        config.local_source.collector_api_port.clone()
    );

    let mut server_stream = match timeout(
        Duration::from_millis(config.max_http_timeout_ms as u64),
        TcpStream::connect(ip_port),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            eprintln!("Error connecting to collector api server: {}", e);
            http::respond_with_timeout(client_stream).await?;
            return Err(Box::new(e));
        }
        Err(_) => {
            eprintln!("Timeout connecting to collector api server.");
            http::respond_with_timeout(client_stream).await?;
            return Err("Timeout connecting to collector api server".into());
        }
    };

    // Forward the client's request to the server
    let mut response_data = vec![];
    match timeout(
        Duration::from_millis(config.max_http_timeout_ms as u64),
        server_stream.write_all(&request_buffer),
    )
    .await
    {
        Ok(Ok(())) => match http::collect_http(&mut server_stream, &mut response_data).await {
            Ok(()) => {
                tokio::spawn(async move {
                    server_stream.shutdown().await.unwrap();
                    drop(server_stream);
                });
            }
            Err(e) => {
                eprintln!("Error reading response from collector api server: {}", e);
                http::respond_with_internal_error(client_stream).await?;
                tokio::spawn(async move {
                    server_stream.shutdown().await.unwrap();
                    drop(server_stream);
                });
                return Err(Box::new(e));
            }
        },
        Ok(Err(e)) => {
            eprintln!("Error forwarding request to collector api server: {}", e);
            http::respond_with_internal_error(client_stream).await?;
            tokio::spawn(async move {
                server_stream.shutdown().await.unwrap();
                drop(server_stream);
            });
            return Err(Box::new(e));
        }
        Err(_) => {
            eprintln!("Timeout forwarding request to collector api server.");
            http::respond_with_timeout(client_stream).await?;
            tokio::spawn(async move {
                server_stream.shutdown().await.unwrap();
                drop(server_stream);
            });
            return Err("Timeout forwarding request to collector api server".into());
        }
    }
    println!("Successfully forwarded request to collector api server.");

    drop(request_buffer);

    if response_data.is_empty() {
        eprintln!("Empty response from collector api server.");
        http::respond_with_internal_error(client_stream).await?;
        return Err("Empty response from collector api server".into());
    }

    http::set_http_header(&mut response_data, "Connection", "keep-alive");
    http::set_http_header(
        &mut response_data,
        "Keep-Alive",
        format!("timeout={}", config.tcp_keepalive_time_sec).as_str(),
    );
    // http::set_http_header(&mut response_data, "Access-Control-Allow-Origin", "*");

    // Relay the collected response to the client
    if let Err(e) = client_stream.write_all(&response_data).await {
        eprintln!("Error relaying response to client: {}", e);
        http::respond_with_internal_error(client_stream).await?;
        return Err(Box::new(e));
    }

    Ok(())
}

pub fn is_collector_route(route: &str) -> bool {
    route.starts_with("/collector")
}

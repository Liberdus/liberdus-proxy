//! Forwards bridge UI notify-bridgeout requests to the coordinator.
//!
//! Flow: Bridge UI → Liberdus Proxy (this) → immediate 200 to UI; in background, proxy
//! forwards to coordinator, awaits response, and logs the result.
//!
//! Coordinator API (see tss-signer/coordinator):
//! - POST /notify-bridgeout
//! - Request body: `{ "chainId": number }`
//! - Response: `{ "Ok": "triggered" | "queued" | "cooldown" }` or 400 `{ "Err": "..." }`

use crate::config::Config;
use crate::http;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// True if this is POST /notify-bridgeout (exact path, no query).
pub fn is_notify_bridgeout_route(method: &str, route: &str) -> bool {
    method.eq_ignore_ascii_case("POST")
        && route
            .split('?')
            .next()
            .map(|p| p == "/notify-bridgeout")
            .unwrap_or(false)
}

pub async fn handle_request<S>(
    request_buffer: Vec<u8>,
    client_stream: &mut S,
    config: Arc<Config>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: AsyncWrite + AsyncRead + Unpin + Send,
{
    let coordinator_url = match config.coordinator_url.as_deref() {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => {
            eprintln!("[notify-bridgeout] coordinator_url not configured");
            respond_503(client_stream, "coordinator_url not configured").await?;
            return Err("coordinator_url not configured".into());
        }
    };

    let body = http::extract_body(&request_buffer);

    // Validate request: Bridge UI sends JSON.stringify({ chainId }) — body must be {"chainId": number}
    if body.is_empty() {
        eprintln!("[notify-bridgeout] rejected: empty body");
        respond_json(client_stream, 400, r#"{"Err":"Invalid or missing chainId"}"#).await?;
        return Ok(());
    }
    let chain_id_valid = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("chainId").and_then(|n| n.as_u64().or_else(|| n.as_i64().map(|i| i as u64))))
        .is_some();
    if !chain_id_valid {
        eprintln!("[notify-bridgeout] rejected: invalid or missing chainId in body");
        respond_json(client_stream, 400, r#"{"Err":"Invalid or missing chainId"}"#).await?;
        return Ok(());
    }

    // Respond to Bridge UI immediately so it is not blocked
    respond_json(client_stream, 200, r#"{"Ok":"accepted"}"#).await?;

    // Forward to coordinator in background; await coordinator response and log result
    tokio::spawn(async move {
        let url = format!("{}/notify-bridgeout", coordinator_url.trim_end_matches('/'));
        let client = reqwest::Client::new();
        match client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(res) => {
                let status = res.status();
                let body = res.bytes().await.unwrap_or_default();
                let body_str = String::from_utf8_lossy(&body);
                if status.is_success() {
                    println!("[notify-bridgeout] coordinator ok status={} body={}", status, body_str);
                } else {
                    eprintln!(
                        "[notify-bridgeout] coordinator error status={} body={}",
                        status, body_str
                    );
                }
            }
            Err(e) => {
                eprintln!("[notify-bridgeout] coordinator request failed: {} (url={})", e, url);
            }
        }
    });

    Ok(())
}

async fn respond_503<S>(client_stream: &mut S, message: &str) -> Result<(), std::io::Error>
where
    S: AsyncWrite + Unpin + Send,
{
    let body = format!(r#"{{"Err":"{}"}}"#, escape_json(message));
    respond_json(client_stream, 503, &body).await
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn respond_json<S>(
    client_stream: &mut S,
    status_code: u16,
    body: &str,
) -> Result<(), std::io::Error>
where
    S: AsyncWrite + Unpin + Send,
{
    let status_line = match status_code {
        200 => "200 OK",
        400 => "400 Bad Request",
        503 => "503 Service Unavailable",
        500 => "500 Internal Server Error",
        _ => "500 Internal Server Error",
    };
    let body_bytes = body.as_bytes();
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{}",
        status_line,
        body_bytes.len(),
        body
    );
    client_stream.write_all(response.as_bytes()).await?;
    client_stream.flush().await
}

//! Forwards bridge UI notify-bridgeout requests to all configured observers.
//!
//! Flow: Bridge UI → Liberdus Proxy (this) → immediate 200 to UI; in background, proxy
//! forwards to every observer, awaits response(s), and logs the results.
//!
//! Observer API (see tss-signer/observer):
//! - POST /notify-bridgeout
//! - Request body: `{ "chainId": number }`
//! - Response: `{ "Ok": "triggered" | "queued" | "cooldown" }` or 400 `{ "Err": "..." }`

use crate::config::Config;
use crate::http;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Rotates which configured observer is tried first for transaction GETs (failover order follows).
static OBSERVER_RR_INDEX: AtomicUsize = AtomicUsize::new(0);

/// Route observer traffic by prefix, similar to collector/notifier modules.
pub fn is_observer_route(route: &str) -> bool {
    route.starts_with("/observer")
}

pub async fn handle_request<S>(
    request_buffer: Vec<u8>,
    client_stream: &mut S,
    config: Arc<Config>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: AsyncWrite + AsyncRead + Unpin + Send,
{
    let (method, route) = match http::get_route(&request_buffer) {
        Some(v) => v,
        None => {
            respond_json(client_stream, 400, r#"{"Err":"Invalid request line"}"#).await?;
            return Ok(());
        }
    };

    let endpoint = match validate_observer_endpoint(&method, &route) {
        ObserverEndpointValidation::AllowedNotifyBridgeout => {
            ObserverEndpoint::NotifyBridgeout
        }
        ObserverEndpointValidation::AllowedPreflight => ObserverEndpoint::Preflight,
        ObserverEndpointValidation::AllowedTransactionGet(forward_route) => {
            ObserverEndpoint::TransactionGet(forward_route)
        }
        ObserverEndpointValidation::BadRequest(message) => {
            let body = format!(r#"{{"Err":"{}"}}"#, escape_json(&message));
            respond_json(client_stream, 400, &body).await?;
            return Ok(());
        }
        ObserverEndpointValidation::MethodNotAllowed => {
            respond_json(
                client_stream,
                405,
                r#"{"Err":"Method not allowed for observer route"}"#,
            )
            .await?;
            return Ok(());
        }
        ObserverEndpointValidation::UnsupportedRoute => {
            respond_json(
                client_stream,
                404,
                r#"{"Err":"Unsupported observer route"}"#,
            )
            .await?;
            return Ok(());
        }
    };

    match endpoint {
        ObserverEndpoint::Preflight => {
            respond_preflight(client_stream).await?;
            Ok(())
        }
        ObserverEndpoint::TransactionGet(route_to_forward) => {
            let observer_urls = configured_observer_urls(config.as_ref());
            forward_transaction_get_to_observers(route_to_forward, client_stream, observer_urls)
                .await
        }
        ObserverEndpoint::NotifyBridgeout => {
            let observer_urls = configured_observer_urls(config.as_ref());
            handle_notify_bridgeout(request_buffer, client_stream, observer_urls).await
        }
    }
}

async fn handle_notify_bridgeout<S>(
    request_buffer: Vec<u8>,
    client_stream: &mut S,
    observer_urls: Vec<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: AsyncWrite + AsyncRead + Unpin + Send,
{
    if observer_urls.is_empty() {
        respond_503(client_stream, "observer_urls not configured").await?;
        return Ok(());
    }

    let body = http::extract_body(&request_buffer);

    // Validate request: Bridge UI sends JSON.stringify({ chainId }) — body must be {"chainId": number}
    if body.is_empty() {
        eprintln!("[notify-bridgeout] rejected: empty body");
        respond_json(client_stream, 400, r#"{"Err":"Invalid or missing chainId"}"#).await?;
        return Ok(());
    }
    let chain_id_valid = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("chainId")
                .and_then(|n| n.as_u64().or_else(|| n.as_i64().map(|i| i as u64)))
        })
        .is_some();
    if !chain_id_valid {
        eprintln!("[notify-bridgeout] rejected: invalid or missing chainId in body");
        respond_json(client_stream, 400, r#"{"Err":"Invalid or missing chainId"}"#).await?;
        return Ok(());
    }

    // Respond to Bridge UI immediately so it is not blocked
    respond_json(client_stream, 200, r#"{"Ok":"accepted"}"#).await?;

    // Forward to observers in background; await responses and log results
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let timeout = std::time::Duration::from_secs(10);

        for base in observer_urls {
            let url = format!("{}/notify-bridgeout", base);
            match client
                .post(&url)
                .header("Content-Type", "application/json")
                .body(body.clone())
                .timeout(timeout)
                .send()
                .await
            {
                Ok(res) => {
                    let status = res.status();
                    let resp_body = res.bytes().await.unwrap_or_default();
                    let body_str = String::from_utf8_lossy(&resp_body);
                    if status.is_success() {
                        println!(
                            "[notify-bridgeout] observer ok url={} status={} body={}",
                            url, status, body_str
                        );
                    } else {
                        eprintln!(
                            "[notify-bridgeout] observer error url={} status={} body={}",
                            url, status, body_str
                        );
                    }
                }
                Err(e) => {
                    eprintln!("[notify-bridgeout] observer request failed: {} (url={})", e, url);
                }
            }
        }
    });

    Ok(())
}

fn is_allowed_observer_resource(route: &str) -> bool {
    let path_only = route.split('?').next().unwrap_or(route);
    matches!(
        path_only,
        "/observer/notify-bridgeout" | "/observer/transaction" | "/observer/transactions"
    )
}

enum ObserverEndpoint {
    NotifyBridgeout,
    Preflight,
    TransactionGet(String),
}

enum ObserverEndpointValidation {
    AllowedNotifyBridgeout,
    AllowedPreflight,
    AllowedTransactionGet(String),
    BadRequest(String),
    MethodNotAllowed,
    UnsupportedRoute,
}

fn validate_observer_endpoint(method: &str, route: &str) -> ObserverEndpointValidation {
    if !is_allowed_observer_resource(route) {
        return ObserverEndpointValidation::UnsupportedRoute;
    }

    let normalized_route = normalize_observer_route(route);
    let normalized_path = normalized_route
        .split('?')
        .next()
        .unwrap_or(normalized_route.as_str());

    match normalized_path {
        "/notify-bridgeout" if method.eq_ignore_ascii_case("POST") => {
            ObserverEndpointValidation::AllowedNotifyBridgeout
        }
        "/notify-bridgeout" if method.eq_ignore_ascii_case("OPTIONS") => {
            ObserverEndpointValidation::AllowedPreflight
        }
        "/transaction" | "/transactions" if method.eq_ignore_ascii_case("GET") => {
            if let Err(msg) = validate_transaction_query(&normalized_route) {
                ObserverEndpointValidation::BadRequest(msg)
            } else {
                ObserverEndpointValidation::AllowedTransactionGet(normalized_route)
            }
        }
        "/notify-bridgeout" | "/transaction" | "/transactions" => {
            ObserverEndpointValidation::MethodNotAllowed
        }
        _ => ObserverEndpointValidation::UnsupportedRoute,
    }
}

fn validate_transaction_query(route_with_query: &str) -> Result<(), String> {
    // Mirrors observer GET /transaction validation (query-level):
    // - page: integer >= 1
    // - txId: 64 hex chars OR 0x + 64 hex chars
    // - sender: 0x + 40 hex chars
    // - type: integer in [0..=2]
    // - status: integer in [0..=5]
    // - unprocessed: "true" | "false"
    let mut parts = route_with_query.splitn(2, '?');
    let _path = parts.next().unwrap_or(route_with_query);
    let query = parts.next().unwrap_or("");
    if query.is_empty() {
        return Ok(());
    }

    let params = parse_query_params(query);

    if let Some(page) = params.get("page") {
        let page_num = page
            .parse::<i64>()
            .map_err(|_| "Invalid page number".to_string())?;
        if page_num < 1 {
            return Err("Invalid page number".to_string());
        }
    }

    if let Some(tx_id) = params.get("txId") {
        let len = tx_id.len();
        let valid = (len == 64 && is_hex(tx_id))
            || (len == 66 && tx_id.starts_with("0x") && is_hex(&tx_id[2..]));
        if !valid {
            return Err("Invalid txId".to_string());
        }
    }

    if let Some(sender) = params.get("sender") {
        if !is_eth_address(sender) {
            return Err("Invalid ethereum address format".to_string());
        }
    }

    if let Some(t) = params.get("type") {
        let v = t.parse::<i64>().map_err(|_| "Invalid type".to_string())?;
        if !(0..=2).contains(&v) {
            return Err("Invalid type".to_string());
        }
    }

    if let Some(s) = params.get("status") {
        let v = s.parse::<i64>().map_err(|_| "Invalid status".to_string())?;
        if !(0..=5).contains(&v) {
            return Err("Invalid status".to_string());
        }
    }

    if let Some(u) = params.get("unprocessed") {
        if u != "true" && u != "false" {
            return Err("Invalid unprocessed flag".to_string());
        }
    }

    if let Some(since) = params.get("sinceTxTimestamp") {
        let v = since
            .parse::<i64>()
            .map_err(|_| "Invalid sinceTxTimestamp".to_string())?;
        if v < 0 {
            return Err("Invalid sinceTxTimestamp".to_string());
        }
    }

    Ok(())
}

fn parse_query_params(query: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut kv = pair.splitn(2, '=');
        let k = kv.next().unwrap_or("").trim();
        if k.is_empty() {
            continue;
        }
        let v = kv.next().unwrap_or("").trim();
        out.insert(k.to_string(), url_decode(v));
    }
    out
}

fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.as_bytes().iter().copied().peekable();
    while let Some(b) = it.next() {
        match b {
            b'+' => out.push(' '),
            b'%' => {
                let hi = it.next();
                let lo = it.next();
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    if let (Some(h), Some(l)) = (from_hex_byte(hi), from_hex_byte(lo)) {
                        out.push((h * 16 + l) as char);
                    }
                }
            }
            _ => out.push(b as char),
        }
    }
    out
}

fn from_hex_byte(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn is_hex(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
}

fn is_eth_address(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && is_hex(&s[2..])
}

fn parse_since_tx_timestamp(route_with_query: &str) -> Option<i64> {
    let mut parts = route_with_query.splitn(2, '?');
    let _path = parts.next()?;
    let query = parts.next().unwrap_or("");
    if query.is_empty() {
        return None;
    }
    let params = parse_query_params(query);
    let since = params.get("sinceTxTimestamp")?.parse::<i64>().ok()?;
    Some(since)
}

fn normalize_observer_route(route: &str) -> String {
    let mut route_parts = route.splitn(2, '?');
    let path_only = route_parts.next().unwrap_or(route);
    let query = route_parts.next();

    let normalized_path = if let Some(stripped) = path_only.strip_prefix("/observer") {
        if stripped.is_empty() {
            "/".to_string()
        } else if stripped.starts_with('/') {
            stripped.to_string()
        } else {
            format!("/{}", stripped)
        }
    } else {
        path_only.to_string()
    };

    match query {
        Some(q) if !q.is_empty() => format!("{}?{}", normalized_path, q),
        _ => normalized_path,
    }
}

async fn forward_transaction_get_to_observers<S>(
    route: String,
    client_stream: &mut S,
    observer_urls: Vec<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: AsyncWrite + Unpin + Send,
{
    if observer_urls.is_empty() {
        respond_503(client_stream, "observer_urls not configured").await?;
        return Ok(());
    }

    let observer_urls = observer_urls_round_robin(observer_urls);
    let route = normalize_transaction_route_params(&route);
    let since_filter = parse_since_tx_timestamp(&route);
    let client = reqwest::Client::new();
    for base in observer_urls {
        let url = format!("{}{}", base, route);
        match client
            .get(&url)
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await
        {
            Ok(resp) => {
                if !resp.status().is_success() {
                    eprintln!("[observer] observer returned {} for {}", resp.status(), url);
                    continue;
                }
                let body = resp.bytes().await.map_err(|e| e.to_string())?;
                if let Some(since_ms) = since_filter {
                    let filtered = filter_transaction_response_by_timestamp(&body, since_ms)?;
                    http::respond_with_json(client_stream, filtered).await?;
                } else {
                    http::respond_with_json(client_stream, body.to_vec()).await?;
                }
                return Ok(());
            }
            Err(e) => {
                eprintln!("[observer] observer request failed for {}: {}", url, e);
                continue;
            }
        }
    }

    http::respond_with_timeout(client_stream).await?;
    Err("all observer transaction requests failed".into())
}

fn normalize_transaction_route_params(route_with_query: &str) -> String {
    let mut parts = route_with_query.splitn(2, '?');
    let path = parts.next().unwrap_or(route_with_query);
    let query = parts.next();
    let Some(query) = query else {
        return path.to_string();
    };
    if query.is_empty() {
        return path.to_string();
    }

    let mut params = parse_query_params(query);

    if let Some(sender) = params.get("sender").cloned() {
        params.insert("sender".to_string(), sender.trim().to_lowercase());
    }

    if let Some(tx_id) = params.get("txId").cloned() {
        let lower = tx_id.trim().to_lowercase();
        let stripped = lower.strip_prefix("0x").unwrap_or(&lower);
        params.insert("txId".to_string(), stripped.to_string());
    }

    // Rebuild query string (order not guaranteed, but semantics identical)
    let mut out = String::with_capacity(route_with_query.len());
    out.push_str(path);
    out.push('?');
    let mut first = true;
    for (k, v) in params.into_iter() {
        if !first {
            out.push('&');
        }
        first = false;
        out.push_str(&k);
        out.push('=');
        out.push_str(&url_encode(&v));
    }
    out
}

fn url_encode(s: &str) -> String {
    // minimal percent-encoding good enough for these params
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn filter_transaction_response_by_timestamp(
    raw_body: &[u8],
    since_ms: i64,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut v: serde_json::Value = serde_json::from_slice(raw_body)?;

    let cutoff = since_ms;

    let txs = v
        .get_mut("Ok")
        .and_then(|ok| ok.get_mut("transactions"))
        .and_then(|t| t.as_array_mut());

    let Some(txs) = txs else {
        return Ok(raw_body.to_vec());
    };

    let mut filtered: Vec<serde_json::Value> = Vec::with_capacity(txs.len());
    for tx in txs.drain(..) {
        let ts = tx
            .get("txTimestamp")
            .and_then(|n| n.as_i64().or_else(|| n.as_u64().map(|u| u as i64)));
        if let Some(ts) = ts {
            if ts >= cutoff {
                filtered.push(tx);
            }
        } else {
            filtered.push(tx);
        }
    }

    if let Some(ok) = v.get_mut("Ok").and_then(|ok| ok.as_object_mut()) {
        let len = filtered.len() as u64;
        ok.insert("transactions".to_string(), serde_json::Value::Array(filtered));
        ok.insert(
            "totalTranactions".to_string(),
            serde_json::Value::Number(serde_json::Number::from(len)),
        );
        ok.insert(
            "totalPages".to_string(),
            serde_json::Value::Number(serde_json::Number::from(1u64)),
        );
    }

    Ok(serde_json::to_vec(&v)?)
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

fn configured_observer_urls(config: &Config) -> Vec<String> {
    config
        .observer_urls
        .iter()
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
        .collect()
}

/// Rotates `urls` so index `start % n` is first; remaining entries preserve failover order.
fn observer_urls_round_robin_at_start(mut urls: Vec<String>, start: usize) -> Vec<String> {
    let n = urls.len();
    if n <= 1 {
        return urls;
    }
    urls.rotate_left(start % n);
    urls
}

/// Puts the next observer first; remaining entries preserve failover order.
fn observer_urls_round_robin(urls: Vec<String>) -> Vec<String> {
    let n = urls.len();
    if n <= 1 {
        return urls;
    }
    let start = OBSERVER_RR_INDEX.fetch_add(1, Ordering::Relaxed);
    observer_urls_round_robin_at_start(urls, start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_urls_round_robin_cycles_start_index() {
        let urls = vec![
            "http://127.0.0.1:8101".to_string(),
            "http://127.0.0.1:8102".to_string(),
            "http://127.0.0.1:8103".to_string(),
        ];
        assert_eq!(
            observer_urls_round_robin_at_start(urls.clone(), 0),
            vec![
                "http://127.0.0.1:8101",
                "http://127.0.0.1:8102",
                "http://127.0.0.1:8103"
            ]
        );
        assert_eq!(
            observer_urls_round_robin_at_start(urls.clone(), 1),
            vec![
                "http://127.0.0.1:8102",
                "http://127.0.0.1:8103",
                "http://127.0.0.1:8101"
            ]
        );
        assert_eq!(
            observer_urls_round_robin_at_start(urls.clone(), 2),
            vec![
                "http://127.0.0.1:8103",
                "http://127.0.0.1:8101",
                "http://127.0.0.1:8102"
            ]
        );
        assert_eq!(
            observer_urls_round_robin_at_start(urls, 3),
            vec![
                "http://127.0.0.1:8101",
                "http://127.0.0.1:8102",
                "http://127.0.0.1:8103"
            ]
        );
    }
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
        404 => "404 Not Found",
        405 => "405 Method Not Allowed",
        503 => "503 Service Unavailable",
        500 => "500 Internal Server Error",
        _ => "500 Internal Server Error",
    };
    let body_bytes = body.as_bytes();
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
        status_line,
        body_bytes.len(),
        http::CORS_ALLOW_ALL,
        body
    );
    client_stream.write_all(response.as_bytes()).await?;
    client_stream.flush().await
}

async fn respond_preflight<S>(client_stream: &mut S) -> Result<(), std::io::Error>
where
    S: AsyncWrite + Unpin + Send,
{
    let response = format!(
        "HTTP/1.1 204 No Content\r\n{}Access-Control-Max-Age: 86400\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        http::CORS_ALLOW_ALL
    );
    client_stream.write_all(response.as_bytes()).await?;
    client_stream.flush().await
}

use tokio::{io::{AsyncRead, AsyncWrite, AsyncWriteExt}, sync::RwLock};
use crate::{config, http, liberdus::Liberdus};
use std::{collections::HashMap, sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH}};
use tokio::{net::TcpStream, time::timeout};
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
    _request_buffer: Vec<u8>,
    route: String,
    client_stream: &mut S,
    config: Arc<config::Config>,
) -> Result<(), Box<dyn std::error::Error>>
where S: AsyncWrite + AsyncRead + Unpin + Send
{

    let cache_map = CACHE_MAP.get_or_init(init_caches);

    let cache = cache_map.get(&route).unwrap().read().await;

    let payload = cache.get();

    drop(cache);


    // Forward the client's request to the server
    let mut response_data = {
        let date_now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
        if payload.buffer.is_none() || (date_now - payload.timestamp) > payload.lifespan as u128 {
            println!("Cold fetch for route: {}", route);
            let mut server_resp_buffer = Vec::new();
            cold_fetch(client_stream, &mut server_resp_buffer, config.clone(), route.as_str()).await?;

            if route == "/api/report" {
                let body = http::extract_body(&server_resp_buffer);

                let mut report: super::Report = match serde_json::from_slice(&body) {
                    Ok(report) => report,
                    Err(e) => {
                        http::respond_with_internal_error(client_stream).await?;
                        return Err(Box::new(e));
                    }
                };
        
                for (_, node) in report.nodes.active.iter_mut() {
                    node.timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
                    node.reportInterval = payload.lifespan;
                }
                   
                for (_, node) in report.nodes.syncing.iter_mut() {
                    node.timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
                }
                   
                report.timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
        
                server_resp_buffer = serde_json::to_vec(&report).unwrap();
            };

            cache_map.get(&route).unwrap().write().await.set(server_resp_buffer.clone());
            server_resp_buffer

        } else {
            payload.buffer.unwrap()
        }
    };

    // route = if route == "/monitor" {
    //     "/".to_string()
    // } else {
    //     route
    // };


    if response_data.is_empty() {
        eprintln!("Empty response from server.");
        http::respond_with_internal_error(client_stream).await?;
        return Err("Empty response from server".into());
    }


    http::set_http_header(&mut response_data, "Connection", "keep-alive");
    http::set_http_header(&mut response_data, "Keep-Alive", format!("timeout={}", config.tcp_keepalive_time_sec).as_str());
    http::set_http_header(&mut response_data, "Access-Control-Allow-Origin", "*");


    // Relay the collected response to the client
    if let Err(e) = client_stream.write_all(&response_data).await {
        eprintln!("Error relaying response to client: {}", e);
        http::respond_with_internal_error(client_stream).await?;
        return Err(Box::new(e));
    }

    Ok(())
}

pub fn is_monitor_route(route: &str) -> bool {
    match route {
        // "/monitor" => true,
        "/style.css" => true,
        "/app.js" => true,
        "/version.js" => true,
        "/fabric.js" => true,
        "/auth.js" => true,
        "/favicon.ico" => true,
        "/axios.min.js" => true,
        "/popmotion.min.js" => true,
        "/api/report" => true,
        "/api/status" => true,
        "/api/version" => true,
        "/milky-way2.png" => true,
        "/logo.png" => true,
        "/" => true,
        _ => false,
    }
}

async fn cold_fetch<S>(
    client_stream: &mut S, 
    server_resp_buffer: &mut Vec<u8>,
    config: Arc<config::Config>, 
    route: &str, 
) -> Result<(), Box<dyn std::error::Error>>
where S: AsyncRead + AsyncWrite + Unpin + Send
{
    let upstream_ip = config.shardus_monitor.upstream_ip.clone();
    let upstream_port = config.shardus_monitor.upstream_port.clone();
    let ip_port = format!("{}:{}", upstream_ip, upstream_port);
    let mut server_stream = match timeout(Duration::from_millis(config.max_http_timeout_ms as u64), TcpStream::connect(ip_port)).await {
        Ok(Ok(stream)) => {
            stream
        },
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


    let emulated_request = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\n\r\n",
        route,
        upstream_ip,
        upstream_port
    ).into_bytes().to_vec();

    match timeout(Duration::from_millis((config.max_http_timeout_ms * 2) as u64), server_stream.write_all(&emulated_request)).await {
        Ok(Ok(())) => {
            match http::collect_http(&mut server_stream, server_resp_buffer).await {
                Ok(()) => {
                    tokio::spawn(async move {
                        server_stream.shutdown().await.unwrap();
                        drop(server_stream);
                    });
                },
                Err(e) => {
                    eprintln!("Error reading response from server: {}", e);
                    http::respond_with_internal_error(client_stream).await?;
                    tokio::spawn(async move {
                        server_stream.shutdown().await.unwrap();
                        drop(server_stream);
                    });
                    return Err(Box::new(e));
                }
            }


        },
        Ok(Err(e)) => {
            eprintln!("Error forwarding request to server: {}", e);
            http::respond_with_internal_error(client_stream).await?;
            tokio::spawn(async move {
                server_stream.shutdown().await.unwrap();
                drop(server_stream);
            });
            return Err(Box::new(e));
        }
        Err(_) => {
            eprintln!("Timeout forwarding request to server.");
            http::respond_with_timeout(client_stream).await?;
            tokio::spawn(async move {
                server_stream.shutdown().await.unwrap();
                drop(server_stream);
            });
            return Err("Timeout forwarding request to server".into());
        }
    }
    println!("Successfully forwarded request to server.");
    Ok(())
}

pub struct PayloadCache {
    pub buffer: Option<Vec<u8>>,
    pub timestamp: u128,
    pub lifespan: u64,
}

impl PayloadCache {
    pub fn new(lifespan: u64) -> Self {
        Self {
            buffer: None,
            timestamp: 0,
            lifespan,
        }
    }
    pub fn set(&mut self, payload: Vec<u8>) {
        self.buffer = Some(payload);
        self.timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    }

    pub fn get(&self) -> PayloadCache {
        PayloadCache {
            buffer: self.buffer.clone(),
            timestamp: self.timestamp.clone(),
            lifespan: self.lifespan.clone(),
        }
    }
}

type PayloadCacheMap = Arc<
    std::collections::HashMap<String, tokio::sync::RwLock<PayloadCache>>
>;

    
fn init_caches() -> PayloadCacheMap {
        let mut map: HashMap<String, RwLock<PayloadCache>> = HashMap::new();
        let one_minute = 60000;

        map.insert("/style.css".to_string(), RwLock::new(PayloadCache::new(one_minute * 30)));
        map.insert("/app.js".to_string(), RwLock::new(PayloadCache::new(one_minute * 30)));
        map.insert("/version.js".to_string(), RwLock::new(PayloadCache::new(one_minute * 30)));
        map.insert("/fabric.js".to_string(), RwLock::new(PayloadCache::new(one_minute * 30)));
        map.insert("/auth.js".to_string(), RwLock::new(PayloadCache::new(one_minute * 30)));
        map.insert("/favicon.ico".to_string(), RwLock::new(PayloadCache::new(one_minute * 30)));
        map.insert("/axios.min.js".to_string(), RwLock::new(PayloadCache::new(one_minute * 30)));
        map.insert("/popmotion.min.js".to_string(), RwLock::new(PayloadCache::new(one_minute * 30)));
        map.insert("/api/report".to_string(), RwLock::new(PayloadCache::new(one_minute * 2)));
        map.insert("/api/status".to_string(), RwLock::new(PayloadCache::new(one_minute * 2)));
        map.insert("/api/version".to_string(), RwLock::new(PayloadCache::new(one_minute * 2)));
        map.insert("/milky-way2.png".to_string(), RwLock::new(PayloadCache::new(one_minute * 30)));
        map.insert("/logo.png".to_string(), RwLock::new(PayloadCache::new(one_minute * 30)));
        map.insert("/".to_string(), RwLock::new(PayloadCache::new(one_minute * 30)));

        Arc::new(map)

}

pub static CACHE_MAP: std::sync::OnceLock<PayloadCacheMap> = std::sync::OnceLock::new();


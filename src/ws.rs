use crate::{collector, rpc, subscription};
use crate::{config, liberdus, Stats};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Mutex, RwLock};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::protocol::Message;

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub enum Methods {
    ChatEvent,
    GetSubscriptions,
}

pub type SocketId = String;
pub type SocketIdents = Arc<RwLock<HashMap<SocketId, UnboundedSender<rpc::RpcResponse>>>>;
pub type WebsocketIncoming = crate::rpc::RpcRequest<Methods>;

fn generate_uuid() -> String {
    let mut random_bytes = [0u8; 16];

    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_nanos();

    let mut seed = seed as u64;

    for i in 0..16 {
        random_bytes[i] = (seed % 256) as u8;
        seed = (seed >> 8) ^ (seed.wrapping_mul(6364136223846793005));
    }

    random_bytes[6] = (random_bytes[6] & 0x0F) | 0x40;

    random_bytes[8] = (random_bytes[8] & 0x3F) | 0x80;

    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([
            random_bytes[0],
            random_bytes[1],
            random_bytes[2],
            random_bytes[3]
        ]),
        u16::from_be_bytes([random_bytes[4], random_bytes[5]]),
        u16::from_be_bytes([random_bytes[6], random_bytes[7]]),
        u16::from_be_bytes([random_bytes[8], random_bytes[9]]),
        u64::from_be_bytes([
            random_bytes[10],
            random_bytes[11],
            random_bytes[12],
            random_bytes[13],
            random_bytes[14],
            random_bytes[15],
            0,
            0
        ]) >> 16 // Ensure 12-digit hex format
    )
}

pub async fn listen(
    liberdus: Arc<liberdus::Liberdus>,
    subscription_manager: Arc<subscription::Manager>,
    sock_map: Arc<RwLock<HashMap<SocketId, UnboundedSender<rpc::RpcResponse>>>>,
    config: Arc<config::Config>,
    server_stats: Arc<Stats>,
    tls_acceptor: Option<TlsAcceptor>,
) {
    // let semaphore = Arc::new(Semaphore::new(300));

    let listener =
        match tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.http_port + 1)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Error binding to port: {}", e);
                std::process::exit(1);
            }
        };

    // Account update listening disabled
    // let subscription_manager_for_listener = Arc::clone(&subscription_manager);
    // let config_moved = Arc::clone(&config);
    // tokio::spawn(async move {
    //     collector::listen_account_update(
    //         &config_moved.local_source.collector_event_server_ip,
    //         &config_moved.local_source.collector_event_server_port,
    //         subscription_manager_for_listener.clone(),
    //         subscription::listen_account_update_callback,
    //     )
    //     .await;
    // });
    println!(
        "Websocket Listening on: {}",
        listener.local_addr().expect("Couldn't bind to a port")
    );

    loop {
        // let throttler = Arc::clone(&semaphore);
        let (raw_stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error: {}", e);
                continue;
            }
        };

        // if semaphore.available_permits() == 0 {
        //     tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        //     continue;
        // }

        let liberdus = Arc::clone(&liberdus);
        let config = Arc::clone(&config);
        let stats = Arc::clone(&server_stats);
        let tls_acceptor = match tls_acceptor.is_some() && config.tls.enabled {
            true => Some(tls_acceptor.clone().unwrap()),
            false => None,
        };
        let sock_map = Arc::clone(&sock_map);
        let subscription_manager = Arc::clone(&subscription_manager);

        tokio::spawn(async move {
            // let permit = throttler.acquire().await.unwrap();
            stats
                .stream_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            match tls_acceptor {
                Some(tls_acceptor) => match tls_acceptor.accept(raw_stream).await {
                    Ok(tls_stream) => {
                        let tls_stream = tokio_rustls::TlsStream::Server(tls_stream);
                        let e = handle_stream(tls_stream, sock_map, subscription_manager).await;
                        if let Err(e) = e {
                            eprintln!("Handle Stream Error: {:?}", e);
                        }
                    }
                    Err(e) => {
                        eprintln!("TLS Handshake Error: {:?}", e);
                        stats
                            .stream_count
                            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                },
                None => {
                    let e = handle_stream(raw_stream, sock_map, subscription_manager).await;
                    if let Err(e) = e {
                        eprintln!("Handle Stream Error: {}", e);
                    }
                }
            }
            stats
                .stream_count
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            // drop(permit);
        });
    }
}

async fn handle_stream<StreamLike>(
    stream: StreamLike,
    sock_map: SocketIdents,
    subscription_manager: Arc<subscription::Manager>,
) -> Result<(), std::io::Error>
where
    StreamLike: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let socket_id = generate_uuid();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<rpc::RpcResponse>();
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws_stream) => {
            {
                let mut guard = sock_map.write().await;
                guard.insert(socket_id.clone(), tx.clone());
            }
            ws_stream
        }
        Err(e) => {
            eprintln!("Error accepting websocket: {}", e);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Error accepting websocket",
            ));
        }
    };

    let (write, mut read) = ws_stream.split();

    let socket_write_half = Arc::new(Mutex::new(write));

    let id = socket_id.clone();
    let subscription_manager_long_lived = Arc::clone(&subscription_manager);

    let last_pong = Arc::new(AtomicU64::new(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    ));

    let last_pong_1 = Arc::clone(&last_pong);
    let socket_id_1 = socket_id.clone();
    tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(msg) => match msg {
                    Message::Close(_) => {
                        let mut guard = sock_map.write().await;
                        guard.remove(&socket_id_1);
                        subscription_manager_long_lived
                            .unsubscribe_all(&socket_id_1)
                            .await;
                        drop(guard);
                        break;
                    }
                    Message::Text(msg) => {
                        let parsed: WebsocketIncoming = match serde_json::from_str(&msg) {
                            Ok(p) => p,
                            Err(e) => {
                                let resp = rpc::generate_error_response(
                                    None,
                                    format!("Invalid JSON: {}", e),
                                    -32600,
                                );
                                tx.send(resp).unwrap();
                                continue;
                            }
                        };

                        let rpc_id = parsed.id.clone();
                        let e = rpc::handle(
                            parsed,
                            subscription_manager_long_lived.clone(),
                            tx.clone(),
                            id.clone(),
                        )
                        .await;
                        if let Err(e) = e {
                            eprintln!("Error handling request: {}", e);
                            let resp = rpc::generate_error_response(
                                Some(rpc_id),
                                format!("Error handling request: {}", e),
                                rpc::RpcErrorCode::InternalError as i32,
                            );
                            tx.send(resp).unwrap();
                        }
                    }
                    Message::Pong(_) => {
                        println!("Pong received, socket id {:?}", socket_id_1);
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        last_pong_1.store(now, Ordering::Relaxed);
                    }
                    _ => {
                        continue;
                    }
                },
                Err(e) => {
                    if e.to_string().contains("Connection reset by peer") {
                        break;
                    }
                }
            }
        }
    });

    //heartbeat ping
    let write_half_for_ping_pong = Arc::clone(&socket_write_half);
    let ping_task = tokio::spawn(async move {
        let heartbeat_interval_sec = 60;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(heartbeat_interval_sec)).await;

            let last = last_pong.load(Ordering::Relaxed);
            let elapsed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .saturating_sub(last);
            if elapsed > heartbeat_interval_sec {
                subscription_manager.unsubscribe_all(&socket_id).await;
                let mut guard = write_half_for_ping_pong.lock().await;
                let _ = guard.close().await;
                drop(guard);
                break;
            }

            let mut guard = write_half_for_ping_pong.lock().await;
            if let Err(_e) = guard.send(Message::Ping("Ping".into())).await {
                let _ = guard.close().await;
                drop(guard);
                break;
            }
            drop(guard);
        }
    });

    while let Some(msg) = rx.recv().await {
        let json = match serde_json::to_value(&msg) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("Error serializing message: {}", e);
                break;
            }
        };

        match socket_write_half
            .lock()
            .await
            .send(Message::Text(json.to_string().into()))
            .await
        {
            Ok(_) => {}
            Err(e) => {
                eprintln!("Error sending message: {}", e);
                break;
            }
        }
    }

    ping_task.abort();
    let _ = socket_write_half.lock().await.close().await;

    Ok(())
}

use crate::subscription;
use crate::{config, liberdus, subscription::WebsocketIncoming, Stats};
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::RwLock;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite;
use tokio_tungstenite::tungstenite::protocol::Message;

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub enum Methods {
    ChatEvent,
}

pub type SocketId = String;
pub type SocketIdents = Arc<RwLock<HashMap<SocketId, UnboundedSender<Message>>>>;

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
    config: Arc<config::Config>,
    server_stats: Arc<Stats>,
    tls_acceptor: Option<TlsAcceptor>,
) {
    // let semaphore = Arc::new(Semaphore::new(300));

    let listener =
        match tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.http_port.clone() + 1))
            .await
        {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Error binding to port: {}", e);
                std::process::exit(1);
            }
        };
    let sock_map = Arc::new(RwLock::new(HashMap::new()));
    let subscription_manager = Arc::new(subscription::Manager::new(
        sock_map.clone(),
        liberdus.clone(),
    ));

    let sm = Arc::clone(&subscription_manager);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(10));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);

        loop {
            ticker.tick().await;
            sm.discover().await;
        }
    });
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
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
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

    let (mut write, mut read) = ws_stream.split();

    let id = socket_id.clone();
    let subscription_manager_long_lived = Arc::clone(&subscription_manager);
    tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(msg) => match msg {
                    Message::Close(_) => {
                        let mut guard = sock_map.write().await;
                        guard.remove(&socket_id);
                        subscription_manager_long_lived
                            .unsubscribe_all(&socket_id)
                            .await;
                        drop(guard);
                        break;
                    }
                    Message::Text(msg) => {
                        let parsed: WebsocketIncoming = match serde_json::from_str(&msg) {
                            Ok(p) => p,
                            Err(e) => {
                                let resp = serde_json::json!({
                                    "id": 0,
                                    "error": "Invalid JSON or Request",
                                });
                                tx.send(Message::Text(resp.to_string().into())).unwrap();
                                continue;
                            }
                        };

                        match parsed.method {
                            Methods::ChatEvent => {
                                let e = handle_subscriptions(
                                    parsed,
                                    tx.clone(),
                                    Arc::clone(&subscription_manager_long_lived),
                                    id.clone(),
                                )
                                .await;
                                if let Err(e) = e {
                                    eprintln!("Error handling subscription: {}", e);
                                }
                            }
                        }
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

    while let Some(msg) = rx.recv().await {
        match write.send(msg).await {
            Ok(_) => {}
            Err(e) => {
                eprintln!("Error sending message: {}", e);
                break;
            }
        }
    }

    let _ = write.close().await;

    Ok(())
}

async fn handle_subscriptions(
    msg: WebsocketIncoming,
    transmitter: tokio::sync::mpsc::UnboundedSender<Message>,
    subscription_manager: Arc<subscription::Manager>,
    socket_id: SocketId,
) -> Result<(), Box<dyn std::error::Error>> {
    match msg.params.0 {
        subscription::SubscriptionActions::Subscribe => {
            let status = subscription_manager
                .subscribe(&socket_id, msg.params.1.as_str())
                .await;

            let resp = subscription::SubscriptionResponse {
                result: status,
                account_id: msg.params.1.clone(),
                error: None,
            };

            let text = serde_json::to_string(&resp).unwrap();

            let msg = Message::Text(text.into());

            transmitter.send(msg).expect("I think channel is closed");
        }
        subscription::SubscriptionActions::Unsubscribe => {
            let status = subscription_manager
                .unsubscribe(&socket_id, msg.params.1.as_str())
                .await;

            let resp = subscription::SubscriptionResponse {
                result: status,
                account_id: msg.params.1.clone(),
                error: None,
            };

            let text = serde_json::to_string(&resp).unwrap();

            let msg = Message::Text(text.into());

            transmitter.send(msg).expect("I think channel is closed");
        }
    }

    Ok(())
}

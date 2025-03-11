use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;

use crate::liberdus;
use crate::ws;

#[derive(serde::Deserialize, serde::Serialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum SubscriptionActions {
    Subscribe,
    Unsubscribe,
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct WebsocketIncoming {
    pub method: ws::Methods,
    pub params: (SubscriptionActions, String),
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct SubscriptionEvent {
    pub account_id: UserAccountAddress,
    pub timestamp: u128,
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct SubscriptionResponse {
    pub result: bool,
    pub account_id: UserAccountAddress,
    pub error: Option<serde_json::Value>,
}

type UserAccountAddress = String;
type Timestamp = u128;

pub struct Manager {
    states: Arc<RwLock<Inner>>,
    liberdus: Arc<liberdus::Liberdus>,
    socket_map: ws::SocketIdents,
}

#[derive(Debug)]
struct Inner {
    accounts_by_sock: HashMap<ws::SocketId, HashSet<UserAccountAddress>>,
    socks_by_account: HashMap<UserAccountAddress, HashSet<ws::SocketId>>,
    last_received: HashMap<UserAccountAddress, Timestamp>,
}

impl Manager {
    pub fn new(sock_map: ws::SocketIdents, liberdus: Arc<liberdus::Liberdus>) -> Self {
        Manager {
            states: Arc::new(RwLock::new(Inner {
                accounts_by_sock: HashMap::new(),
                socks_by_account: HashMap::new(),
                last_received: HashMap::new(),
            })),
            socket_map: sock_map,
            liberdus,
        }
    }

    async fn set_states(&self, addr: UserAccountAddress, socket_id: &ws::SocketId) {
        let mut guard = self.states.write().await;

        let subs = guard
            .accounts_by_sock
            .entry(socket_id.clone())
            .or_insert(HashSet::new());
        subs.insert(addr.clone());

        let sockets = guard
            .socks_by_account
            .entry(addr.clone())
            .or_insert(HashSet::new());
        sockets.insert(socket_id.clone());

        guard.last_received.entry(addr.clone()).or_insert(0);
    }

    async fn remove_states(&self, addr: UserAccountAddress, socket_id: &ws::SocketId) {
        let mut guard = self.states.write().await;

        match guard.accounts_by_sock.get_mut(socket_id) {
            Some(a) => {
                a.remove(&addr);
                if a.is_empty() {
                    guard.accounts_by_sock.remove(socket_id);
                }
            }
            _ => {}
        }

        match guard.socks_by_account.get_mut(&addr) {
            Some(s) => {
                s.remove(socket_id);
                if s.is_empty() {
                    guard.socks_by_account.remove(&addr);
                    guard.last_received.remove(&addr);
                }
            }
            _ => {}
        }
    }

    async fn is_exist(&self, addr: &UserAccountAddress, sock_id: &String) -> bool {
        let guard = self.states.read().await;

        let accounts = match guard.accounts_by_sock.get(sock_id) {
            Some(a) => a.contains(addr),
            None => false,
        };

        drop(guard);

        accounts
    }

    pub async fn unsubscribe_all(&self, socket_id: &ws::SocketId) {
        // grab all account subscribed by this socket
        // remove self from that account
        // remove self from associated data structs

        let guard = self.states.read().await;

        let accounts = match guard.accounts_by_sock.get(socket_id) {
            Some(a) => a.clone(),
            None => {
                return;
            }
        };

        drop(guard);

        for account in accounts {
            self.remove_states(account, socket_id).await;
        }
        // println!("{:?}", self.states.read().await);
    }

    pub async fn discover(&self) {
        let read_guard = self.states.read().await;

        let accounts = read_guard
            .socks_by_account
            .keys()
            .cloned()
            .collect::<Vec<UserAccountAddress>>();

        drop(read_guard);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Message)>();

        if accounts.is_empty() {
            return;
        }

        for address in accounts {
            let liberdus = Arc::clone(&self.liberdus);
            let states = Arc::clone(&self.states);
            let tx = tx.clone();
            tokio::spawn(async move {
                let resp = match liberdus.get_account_by_address(&address).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("Error getting account: {}", e);
                        return;
                    }
                };

                let account = match serde_json::from_value::<liberdus::UserAccount>(resp) {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("Error parsing account: {}", e);
                        return;
                    }
                };

                let new_timestamp = account.timestamp;

                let guard = states.read().await;

                let old_timestamp = match guard.last_received.get(&address) {
                    Some(t) => t.clone(),
                    None => {
                        // todo purge this address from all entries
                        drop(guard);
                        return;
                    }
                };

                drop(guard);

                if old_timestamp == 0 && new_timestamp > old_timestamp {
                    let mut guard = states.write().await;
                    let timestmap = guard.last_received.get_mut(&address);
                    if let Some(t) = timestmap {
                        *t = new_timestamp;
                    }
                    drop(guard);
                    return;
                }

                if new_timestamp > old_timestamp {
                    let sockets = states
                        .read()
                        .await
                        .socks_by_account
                        .get(&address)
                        .unwrap_or(&HashSet::new())
                        .clone();

                    let event = SubscriptionEvent {
                        account_id: address.clone(),
                        timestamp: new_timestamp,
                    };

                    let tx = tx.clone();
                    let msg = Message::Text(serde_json::to_string(&event).unwrap().into());

                    let mut guard = states.write().await;
                    let timestmap = guard.last_received.get_mut(&address);
                    if let Some(t) = timestmap {
                        *t = new_timestamp;
                    }

                    drop(guard);

                    for socket in sockets {
                        tx.clone()
                            .send((socket.clone(), msg.clone()))
                            .expect("Failed to send message");
                    }
                }
            });
        }

        drop(tx);
        while let Some((sock_id, msg)) = rx.recv().await {
            let sock_transmitter = match self.socket_map.read().await.get(&sock_id) {
                Some(t) => t.clone(),
                None => {
                    self.unsubscribe_all(&sock_id).await;
                    continue;
                }
            };

            if let Err(e) = sock_transmitter.send(msg) {
                eprintln!("Error sending message: {}", e);
                self.unsubscribe_all(&sock_id).await;
            }
        }
    }

    pub async fn subscribe(&self, socket_id: &ws::SocketId, address: &str) -> bool {
        match self.is_exist(&address.to_string(), &socket_id).await {
            true => {
                return false;
            }
            false => {
                self.set_states(address.to_string(), socket_id).await;
                return true;
            }
        }
    }

    pub async fn unsubscribe(&self, socket_id: &ws::SocketId, address: &str) -> bool {
        let guard = self.states.read().await;

        let accounts = match guard.accounts_by_sock.get(socket_id) {
            Some(a) => a.contains(&address.to_string()),
            None => false,
        };

        drop(guard);

        if accounts {
            self.remove_states(address.to_string(), socket_id).await;
            return true;
        }

        false
    }
}

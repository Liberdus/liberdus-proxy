use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;

use crate::{liberdus, rpc};
use crate::ws;

#[derive(serde::Deserialize, serde::Serialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum SubscriptionActions {
    Subscribe,
    Unsubscribe,
}

impl From<&str> for SubscriptionActions {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "subscribe" => SubscriptionActions::Subscribe,
            "unsubscribe" => SubscriptionActions::Unsubscribe,
            _ => panic!("Invalid action"),
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct Notification {
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
        let account_current_timestamp = match self.liberdus.get_account_by_address(&addr).await {
            Ok(r) => match serde_json::from_value::<liberdus::UserAccount>(r) {
                Ok(a) => a.data.chat_timestamp,
                _ => 0,
            },
            _ => 0,
        };

        let mut write_guard = self.states.write().await;
        let subs = write_guard
            .accounts_by_sock
            .entry(socket_id.clone())
            .or_insert(HashSet::new());
        subs.insert(addr.clone());

        let sockets = write_guard
            .socks_by_account
            .entry(addr.clone())
            .or_insert(HashSet::new());
        sockets.insert(socket_id.clone());

        write_guard
            .last_received
            .entry(addr.clone())
            .or_insert(account_current_timestamp);
    }

    async fn remove_states(&self, addr: UserAccountAddress, socket_id: &ws::SocketId) {
        let mut guard = self.states.write().await;

        if let Some(a) = guard.accounts_by_sock.get_mut(socket_id) {
            a.remove(&addr);
            if a.is_empty() {
                guard.accounts_by_sock.remove(socket_id);
            }
        }

        if let Some(s) = guard.socks_by_account.get_mut(&addr) {
            s.remove(socket_id);
            if s.is_empty() {
                guard.socks_by_account.remove(&addr);
                guard.last_received.remove(&addr);
            }
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

    pub async fn subscribe(&self, socket_id: &ws::SocketId, address: &str) -> bool {
        match self.is_exist(&address.to_string(), socket_id).await {
            true => false,
            false => {
                self.set_states(address.to_string(), socket_id).await;
                true
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

    pub async fn get_all_subscriptions(&self) -> Vec<String> {
        let read_guard = self.states.read().await;
        let mut all_subscriptions = Vec::new();

        for (account, _) in &read_guard.socks_by_account {
            all_subscriptions.push(account.clone());
        }
        drop(read_guard);
        all_subscriptions
    }
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct AccountUpdatePayload {
    event: String,

    #[serde(deserialize_with = "deserialize_stringified_account_update")]
    data: AccountUpdate,
}
fn deserialize_stringified_account_update<'de, D>(
    deserializer: D,
) -> Result<AccountUpdate, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = <String as serde::Deserialize>::deserialize(deserializer)?;
    serde_json::from_str(&s).map_err(serde::de::Error::custom)
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct AccountUpdate {
    accountId: String,
    timestamp: u128,
    data: InnerData,
}
#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct InnerData {
    data: serde_json::Value,
}

pub async fn listen_account_update_callback(
    value: serde_json::Value,
    subscription_manager: Arc<Manager>,
) {
    let payload: AccountUpdatePayload = match serde_json::from_value(value) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error parsing account update payload: {}", e);
            return;
        }
    };

    let account_id = payload.data.accountId;
    let timestamp = payload.data.data.data["chatTimestamp"]
        .as_u64()
        .unwrap_or(0) as u128;

    let read_guard = subscription_manager.states.read().await;
    // if no subscription for this account, return
    if !read_guard.socks_by_account.contains_key(&account_id) {
        drop(read_guard);
        return;
    }
    drop(read_guard);

    let mut write_guard = subscription_manager.states.write().await;

    let old_timestamp = match write_guard.last_received.get(&account_id) {
        Some(t) => *t,
        None => 0,
    };

    if timestamp <= old_timestamp {
        drop(write_guard);
        return;
    }

    let timestmap = write_guard.last_received.get_mut(&account_id);
    if let Some(t) = timestmap {
        *t = timestamp;
    }
    drop(write_guard);

    let read_guard = subscription_manager.states.read().await;
    let sockets = read_guard
        .socks_by_account
        .get(&account_id)
        .unwrap_or(&HashSet::new())
        .clone();
    drop(read_guard);

    if sockets.is_empty() {
        return;
    }

    let noti = Notification {
        account_id: account_id.clone(),
        timestamp,
    };

    let resp = rpc::generate_success_response(
        None,
        serde_json::to_value(noti).unwrap_or(serde_json::Value::Null),
    );

    for socket in sockets {
        let socket_map = subscription_manager.socket_map.read().await;
        let tx = match socket_map.get(&socket) {
            Some(t) => t.clone(),
            None => {
                subscription_manager.unsubscribe_all(&socket).await;
                continue;
            }
        };
        if let Err(_e) = tx.send(resp.clone()) {
            subscription_manager.unsubscribe_all(&socket).await;
        }
    }
}

pub mod rpc_handler {
    use std::sync::Arc;

    use crate::{rpc::{self, RpcResponse}, ws::{SocketId, WebsocketIncoming}};

    use super::Manager;

    pub async fn handle_subscriptions(
        req: WebsocketIncoming,
        subscription_manager: Arc<Manager>,
        socket_id: SocketId,
    ) -> RpcResponse {
        match req.params[0].as_str().unwrap_or("").into() {
            super::SubscriptionActions::Subscribe => {
                let _ = subscription_manager
                    .subscribe(&socket_id, req.params[1].as_str().unwrap_or("").into())
                    .await;

                rpc::generate_success_response(
                    Some(req.id),
                    serde_json::json!({
                        "subscription_status": true,
                        "account_id": req.params[1].as_str().unwrap_or(""),
                    }),
                )

            }
            super::SubscriptionActions::Unsubscribe => {
                let status = subscription_manager
                    .unsubscribe(&socket_id, req.params[1].as_str().unwrap_or("").into())
                    .await;

                rpc::generate_success_response(
                    Some(req.id),
                    serde_json::json!({
                        "unsubscribe_status": status,
                        "account_id": req.params[1].as_str().unwrap_or(""),
                    }),
                )

            }
        }

    }

    pub async fn get_all_subscriptions(
        request: WebsocketIncoming,
        subscription_manager: Arc<Manager>,
    ) -> RpcResponse {

        let subscriptions = subscription_manager
            .get_all_subscriptions()
            .await;
        rpc::generate_success_response(
            Some(request.id),
            serde_json::json!({
                "subscribed_accounts": subscriptions
            }),
        )
    }
}

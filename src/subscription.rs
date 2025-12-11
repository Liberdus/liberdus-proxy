use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::ws;
use crate::{liberdus, rpc};

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
    pub action: String,
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

        for account in read_guard.socks_by_account.keys() {
            all_subscriptions.push(account.clone());
        }
        drop(read_guard);
        all_subscriptions
    }

    #[cfg(test)]
    pub async fn insert_subscription_for_test(
        &self,
        socket_id: &ws::SocketId,
        account: &str,
        timestamp: u128,
    ) {
        let mut guard = self.states.write().await;
        guard
            .accounts_by_sock
            .entry(socket_id.clone())
            .or_default()
            .insert(account.to_string());
        guard
            .socks_by_account
            .entry(account.to_string())
            .or_default()
            .insert(socket_id.clone());
        guard.last_received.insert(account.to_string(), timestamp);
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
        action: "Notification".to_string(),
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

    use crate::{
        rpc::{self, RpcResponse},
        ws::{SocketId, WebsocketIncoming},
    };

    use super::Manager;

    pub async fn handle_subscriptions(
        req: WebsocketIncoming,
        subscription_manager: Arc<Manager>,
        socket_id: SocketId,
    ) -> RpcResponse {
        match req.params[0].as_str().unwrap_or("").into() {
            super::SubscriptionActions::Subscribe => {
                let _ = subscription_manager
                    .subscribe(&socket_id, req.params[1].as_str().unwrap_or(""))
                    .await;

                rpc::generate_success_response(
                    Some(req.id),
                    serde_json::json!({
                        "action": "subscribe",
                        "subscription_status": true,
                        "account_id": req.params[1].as_str().unwrap_or(""),
                    }),
                )
            }
            super::SubscriptionActions::Unsubscribe => {
                let status = subscription_manager
                    .unsubscribe(&socket_id, req.params[1].as_str().unwrap_or(""))
                    .await;

                rpc::generate_success_response(
                    Some(req.id),
                    serde_json::json!({
                        "action": "unsubscribe",
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
        let subscriptions = subscription_manager.get_all_subscriptions().await;
        rpc::generate_success_response(
            Some(request.id),
            serde_json::json!({
                "action": "GetSubscriptions",
                "subscribed_accounts": subscriptions
            }),
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config;
    use crate::crypto::ShardusCrypto;
    use crate::liberdus;
    use crate::ws;
    use arc_swap::ArcSwap;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn sample_config() -> crate::config::Config {
        crate::config::Config {
            http_port: 0,
            crypto_seed: "64f152869ca2d473e4ba64ab53f49ccdb2edae22da192c126850970e788af347".into(),
            archiver_seed_path: String::new(),
            nodelist_refresh_interval_sec: 1,
            debug: false,
            max_http_timeout_ms: 1_000,
            tcp_keepalive_time_sec: 1,
            standalone_network: crate::config::StandaloneNetworkConfig {
                replacement_ip: "127.0.0.1".into(),
                enabled: false,
            },
            node_filtering: crate::config::NodeFilteringConfig {
                enabled: false,
                remove_top_nodes: 0,
                remove_bottom_nodes: 0,
                min_nodes_for_filtering: 0,
            },
            tls: crate::config::TLSConfig {
                enabled: false,
                cert_path: String::new(),
                key_path: String::new(),
            },
            shardus_monitor: crate::config::ShardusMonitorProxyConfig {
                enabled: false,
                upstream_ip: String::new(),
                upstream_port: 0,
                https: false,
            },
            local_source: crate::config::LocalSource {
                collector_api_ip: String::new(),
                collector_api_port: 0,
                collector_event_server_ip: String::new(),
                collector_event_server_port: 0,
            },
            notifier: crate::config::NotifierConfig {
                ip: String::new(),
                port: 0,
            },
        }
    }

    pub(crate) fn sample_manager() -> Manager {
        let crypto = Arc::new(ShardusCrypto::new(
            "64f152869ca2d473e4ba64ab53f49ccdb2edae22da192c126850970e788af347",
        ));
        let liberdus = Arc::new(crate::liberdus::Liberdus::new(
            crypto,
            Arc::new(ArcSwap::from_pointee(Vec::new())),
            sample_config(),
        ));
        Manager::new(Arc::new(tokio::sync::RwLock::new(HashMap::new())), liberdus)
    }

    #[tokio::test]
    async fn unsubscribe_all_clears_state() {
        let manager = sample_manager();
        manager
            .insert_subscription_for_test(&"socket1".into(), "acct", 1)
            .await;

        manager.unsubscribe_all(&"socket1".into()).await;

        assert!(manager.states.read().await.accounts_by_sock.is_empty());
        assert!(manager.states.read().await.socks_by_account.is_empty());
    }

    #[tokio::test]
    async fn listen_account_update_sends_notification() {
        let manager = sample_manager();
        manager
            .insert_subscription_for_test(&"socket1".into(), "acct", 0)
            .await;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        manager
            .socket_map
            .write()
            .await
            .insert("socket1".into(), tx);

        let payload = serde_json::json!({
            "event": "accountUpdate",
            "data": serde_json::to_string(&serde_json::json!({
                "accountId": "acct",
                "timestamp": 2,
                "data": {"data": {"chatTimestamp": 2}}
            }))
            .unwrap()
        });

        listen_account_update_callback(payload, Arc::new(manager)).await;

        let msg = rx.try_recv().expect("notification should be emitted");
        assert_eq!(msg.result.unwrap()["account_id"], "acct");
    }

    fn create_test_manager() -> Manager {
        let config = config::Config::default();
        let crypto = Arc::new(ShardusCrypto::new(&config.crypto_seed));
        let archivers = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let liberdus = Arc::new(liberdus::Liberdus::new(crypto, archivers, config));

        Manager::new(Arc::new(tokio::sync::RwLock::new(HashMap::new())), liberdus)
    }

    #[tokio::test]
    async fn test_subscribe_success() {
        let manager = create_test_manager();
        let sock_id = "sock1".to_string();
        let account = "acc1".to_string();

        let result = manager.subscribe(&sock_id, &account).await;
        assert!(result);

        let guard = manager.states.read().await;
        assert!(guard.accounts_by_sock.contains_key(&sock_id));
        assert!(guard.socks_by_account.contains_key(&account));
    }

    #[tokio::test]
    async fn test_subscribe_duplicate() {
        let manager = create_test_manager();
        let sock_id = "sock1".to_string();
        let account = "acc1".to_string();

        manager.subscribe(&sock_id, &account).await;
        let result = manager.subscribe(&sock_id, &account).await;
        assert!(!result); // Should return false if already exists
    }

    #[tokio::test]
    async fn test_unsubscribe_success() {
        let manager = create_test_manager();
        let sock_id = "sock1".to_string();
        let account = "acc1".to_string();

        manager.subscribe(&sock_id, &account).await;
        let result = manager.unsubscribe(&sock_id, &account).await;
        assert!(result);

        let guard = manager.states.read().await;
        // Should be empty as it was the only subscription
        assert!(!guard.accounts_by_sock.contains_key(&sock_id));
        assert!(!guard.socks_by_account.contains_key(&account));
    }

    #[tokio::test]
    async fn test_unsubscribe_non_existent() {
        let manager = create_test_manager();
        let result = manager.unsubscribe(&"sock1".to_string(), "acc1").await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_unsubscribe_all() {
        let manager = create_test_manager();
        let sock_id = "sock1".to_string();
        manager.subscribe(&sock_id, "acc1").await;
        manager.subscribe(&sock_id, "acc2").await;

        manager.unsubscribe_all(&sock_id).await;

        let guard = manager.states.read().await;
        assert!(!guard.accounts_by_sock.contains_key(&sock_id));
        assert!(!guard.socks_by_account.contains_key("acc1"));
        assert!(!guard.socks_by_account.contains_key("acc2"));
    }

    #[tokio::test]
    async fn test_get_all_subscriptions() {
        let manager = create_test_manager();
        manager.subscribe(&"s1".to_string(), "a1").await;
        manager.subscribe(&"s2".to_string(), "a2").await;

        let subs = manager.get_all_subscriptions().await;
        assert_eq!(subs.len(), 2);
        assert!(subs.contains(&"a1".to_string()));
        assert!(subs.contains(&"a2".to_string()));
    }

    #[tokio::test]
    async fn test_listen_account_update_callback_ignore_old_timestamp() {
        let manager = Arc::new(create_test_manager());
        let sock_id = "sock1".to_string();
        let account = "acc1".to_string();

        // Insert manually to set timestamp
        manager
            .insert_subscription_for_test(&sock_id, &account, 100)
            .await;

        // Setup channel to receive notification
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager.socket_map.write().await.insert(sock_id.clone(), tx);

        // Send update with older timestamp (50 < 100)
        let payload = serde_json::json!({
            "event": "accountUpdate",
            "data": serde_json::to_string(&serde_json::json!({
                "accountId": account,
                "timestamp": 50,
                "data": {"data": {"chatTimestamp": 50}}
            })).unwrap()
        });

        listen_account_update_callback(payload, manager.clone()).await;

        // Should NOT receive anything
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_rpc_handler_subscribe() {
        let manager = Arc::new(create_test_manager());
        let sock_id = "sock1".to_string();

        // Note: rpc_handler::handle_subscriptions assumes the method was already checked/dispatched
        // but it reads params to determine action. Wait, looking at src/subscription.rs:
        // match req.params[0].as_str()...
        // It does not use req.method.
        // However, we need to construct a valid WebsocketIncoming struct.
        // WebsocketIncoming = RpcRequest<Methods>. Methods is enum.

        // But in src/ws.rs:
        // pub type WebsocketIncoming = crate::rpc::RpcRequest<Methods>;
        // enum Methods { ChatEvent, GetSubscriptions }

        // src/subscription.rs uses `req.params[0]` to distinguish Subscribe/Unsubscribe.
        // The method field in request usually routes to `handle_subscriptions`.
        // src/rpc.rs maps `Methods::ChatEvent` to `handle_subscriptions`.

        let req = ws::WebsocketIncoming {
            id: 1,
            jsonrpc: "2.0".to_string(),
            method: ws::Methods::ChatEvent,
            params: vec![serde_json::json!("subscribe"), serde_json::json!("acc1")],
        };

        let resp = rpc_handler::handle_subscriptions(req, manager.clone(), sock_id.clone()).await;

        assert_eq!(resp.error, None);
        // assert!(resp.result.unwrap().get("subscription_status").unwrap().as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_rpc_handler_unsubscribe() {
        let manager = Arc::new(create_test_manager());
        let sock_id = "sock1".to_string();
        manager.subscribe(&sock_id, "acc1").await;

        let req = ws::WebsocketIncoming {
            id: 1,
            jsonrpc: "2.0".to_string(),
            method: ws::Methods::ChatEvent,
            params: vec![serde_json::json!("unsubscribe"), serde_json::json!("acc1")],
        };

        let resp = rpc_handler::handle_subscriptions(req, manager.clone(), sock_id.clone()).await;

        assert_eq!(resp.error, None);
    }

    #[test]
    fn test_subscription_actions_from_str() {
        let sub = SubscriptionActions::from("subscribe");
        assert!(matches!(sub, SubscriptionActions::Subscribe));

        let unsub = SubscriptionActions::from("unsubscribe");
        assert!(matches!(unsub, SubscriptionActions::Unsubscribe));
    }

    #[test]
    #[should_panic]
    fn test_subscription_actions_invalid() {
        let _ = SubscriptionActions::from("invalid");
    }
}

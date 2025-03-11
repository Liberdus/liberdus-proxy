pub mod proxy;

// allow snake case
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[allow(non_snake_case)]
#[derive(Serialize, Deserialize, Debug)]
pub struct Report {
    pub nodes: NodeList,

    pub timestamp: u128,
    // flatten
    #[serde(flatten)]
    pub unkown: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct NodeList {
    // pub joining: HashMap<String, JoinReport>,
    pub syncing: HashMap<String, SyncReport>,
    pub active: HashMap<String, ActiveReport>,
    // pub standby: HashMap<String, StandbyReport>,
    #[serde(flatten)]
    pub unknown: serde_json::Value,
}

#[allow(non_snake_case)]
#[derive(Serialize, Deserialize, Debug)]
pub struct SyncReport {
    // pub publicKey: String,
    // pub nodeId: String,
    // pub nodeIpInfo: NodeIpInfo,
    pub timestamp: u128,
    #[serde(flatten)]
    extra: serde_json::Value,
}
//
#[allow(non_snake_case)]
#[derive(Serialize, Deserialize, Debug)]
pub struct ActiveReport {
    pub reportInterval: u64,
    pub timestamp: u128,
    #[serde(flatten)]
    extra: serde_json::Value,
}

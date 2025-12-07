use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    subscription,
    ws::{Methods, SocketId},
};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RpcRequest<M> {
    pub id: u32,
    pub jsonrpc: String,
    pub method: M,
    pub params: Vec<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RpcResponse {
    pub id: Option<u32>,
    pub jsonrpc: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<RpcError>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum RpcErrorCode {
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
}

/// Creates a JSON-RPC success response.
///
/// # Parameters
/// - `id`: The ID of the request.
/// - `result`: The result of the method call.
///
/// # Returns
/// A [`RpcResponse`] object representing success.
pub fn generate_success_response(id: Option<u32>, result: serde_json::Value) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0".to_string(),
        result: Some(result),
        error: None,
        id,
    }
}

/// Creates a JSON-RPC error response.
///
/// # Parameters
/// - `id`: The ID of the request.
/// - `error_msg`: The error message to include in the response.
/// - `code`: The error code.
///
/// # Returns
/// A [`RpcResponse`] object representing an error.
pub fn generate_error_response(id: Option<u32>, error_msg: String, code: i32) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0".to_string(),
        result: None,
        error: Some(RpcError {
            code,
            message: error_msg,
        }),
        id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription;
    use crate::ws::{Methods, WebsocketIncoming};

    #[tokio::test]
    async fn handle_get_subscriptions_round_trip() {
        let manager = subscription::tests::sample_manager();
        manager
            .insert_subscription_for_test(&"sock".into(), "acct", 1)
            .await;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let req = WebsocketIncoming {
            id: 1,
            jsonrpc: "2.0".into(),
            method: Methods::GetSubscriptions,
            params: vec![],
        };

        handle(req, Arc::new(manager), tx, "sock".into())
            .await
            .unwrap();

        let response = rx.recv().await.unwrap();
        assert!(response.error.is_none());
        let result = response.result.unwrap();
        let accounts = result["subscribed_accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0], "acct");
    }

    #[test]
    fn build_success_and_error_responses() {
        let ok = generate_success_response(Some(1), serde_json::json!({"ok":true}));
        assert_eq!(ok.id, Some(1));
        assert!(ok.error.is_none());
        assert_eq!(ok.result.unwrap()["ok"], true);

        let err =
            generate_error_response(Some(2), "bad".into(), RpcErrorCode::InternalError as i32);
        assert_eq!(err.id, Some(2));
        assert!(err.result.is_none());
        assert_eq!(err.error.unwrap().code, RpcErrorCode::InternalError as i32);
    }
}

pub async fn handle(
    request: crate::ws::WebsocketIncoming,
    subscription_manager: Arc<subscription::Manager>,
    transmitter: tokio::sync::mpsc::UnboundedSender<RpcResponse>,
    socket_id: SocketId,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = match request.method {
        Methods::ChatEvent => {
            subscription::rpc_handler::handle_subscriptions(
                request,
                subscription_manager,
                socket_id,
            )
            .await
        }
        Methods::GetSubscriptions => {
            subscription::rpc_handler::get_all_subscriptions(request, subscription_manager).await
        }
    };

    transmitter.send(resp).map_err(|e| {
        eprintln!("Failed to send response: {}", e);
        std::io::Error::new(std::io::ErrorKind::Other, "Failed to send response")
    })?;

    Ok(())
}

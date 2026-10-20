//! Bridges the existing `RpcRegistry` to the transport-agnostic `httprpc` crate.
//!
//! All the protocol behaviour — notifications, batches, error codes, the derived capability
//! document — lives in `crates/httprpc`, which compiles and is unit-tested on a plain x86 host
//! with no cross toolchain and no Rockchip sysroot. This file is the only part that must be
//! built for the device, and it is deliberately tiny for exactly that reason: everything here
//! is wiring, and everything testable is next door.

use std::sync::Arc;

use futures::future::BoxFuture;
use httprpc::RpcDispatch;
use serde_json::Value;

use crate::jsonrpc::{RpcRegistry, PROCESSOR};

/// Dispatches into the SAME registry the WebRTC data channel uses.
pub struct RegistryDispatch {
    registry: Arc<RpcRegistry>,
}

impl RegistryDispatch {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self { registry: PROCESSOR.registry().clone() })
    }
}

impl RpcDispatch for RegistryDispatch {
    fn call(&self, method: &str, params: Option<Value>) -> BoxFuture<'_, Result<Value, String>> {
        let handler = self.registry.get_handler(method).cloned();
        Box::pin(async move {
            match handler {
                // The exact string `httprpc` maps to JSON-RPC -32601. Kept as a sentinel rather
                // than a typed error so the shim stays dependency-free in both directions.
                None => Err("method not found".to_string()),
                Some(h) => h.call_async(params).await.map_err(|e| e.to_string()),
            }
        })
    }

    fn methods(&self) -> Vec<String> {
        self.registry.list_methods().into_iter().cloned().collect()
    }
}

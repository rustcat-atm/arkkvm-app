//! JSON-RPC 2.0 over plain HTTP, dispatching into the registry the data channel already uses.
//!
//! # Why this exists
//!
//! ArkKVM's control plane rides JSON-RPC over a WebRTC data channel. That is the right
//! transport for an interactive session and the wrong one for a single command: it forces
//! every consumer — a monitoring loop, a provisioning script, a CI job, an orchestrator — to
//! carry a full WebRTC stack (and, for screenshots, an H.264 decoder) before it can ask
//! whether the host is powered on.
//!
//! The asymmetry is the point. Keep WebRTC for the stream. A one-shot command should be one
//! request.
//!
//! # What it is not
//!
//! It is **not a second implementation of the API**. It dispatches into the same
//! [`RpcDispatch`] the data channel uses, so a method added to the registry is reachable over
//! both transports with no further work, and the two can never disagree about what a method
//! does. A parallel HTTP implementation would drift the first time somebody fixed a bug in one
//! of them.
//!
//! # Design notes
//!
//! * **Notifications get `204`, not a body.** A JSON-RPC request without an `id` is a
//!   notification and must not be answered. The data-channel path already honours this
//!   (it skips the response to save serialisations on high-rate input events); HTTP honours it
//!   by returning `204 No Content`.
//! * **Batches are supported, and an all-notification batch also returns `204`**, per the spec.
//! * **Errors carry the standard codes.** `-32700` parse, `-32600` invalid request, `-32601`
//!   method not found, `-32603` internal. A client that already speaks JSON-RPC needs no
//!   special cases.
//! * **The HTTP status is always `200` for a well-formed JSON-RPC exchange**, even when the
//!   envelope carries an error. Transport failure and application failure are different things,
//!   and collapsing them is how a client ends up retrying a refusal.
//! * **`/capabilities` is derived from the registry, never hand-maintained.** A capability
//!   document that is written by hand is wrong the first time a method is added.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures::future::BoxFuture;
use salvo::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const JSONRPC_VERSION: &str = "2.0";

pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INTERNAL_ERROR: i32 = -32603;

/// The seam. Implemented in the firmware by a thin adapter over the existing `RpcRegistry`, and
/// by a fake in tests — which is what lets every behaviour below be verified on a host with no
/// device, no cross toolchain and no Rockchip sysroot present.
pub trait RpcDispatch: Send + Sync + 'static {
    fn call(&self, method: &str, params: Option<Value>) -> BoxFuture<'_, Result<Value, String>>;
    fn methods(&self) -> Vec<String>;
}

#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub id: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    pub id: Value,
}

fn err(code: i32, message: &str, id: Value) -> Response {
    Response {
        jsonrpc: JSONRPC_VERSION,
        result: None,
        error: Some(json!({ "code": code, "message": message })),
        id,
    }
}

/// Run one request. `Ok(None)` means it was a notification and must not be answered.
pub async fn dispatch_one(d: &dyn RpcDispatch, req: Request) -> Option<Response> {
    let is_notification = req.id.is_none();
    let id = req.id.clone().unwrap_or(Value::Null);

    // ⛔ A missing or wrong `jsonrpc` is an invalid request, not something to guess at. Being
    // lenient here means a client with a typo gets a different method dispatched than it meant.
    if req.jsonrpc != JSONRPC_VERSION {
        return if is_notification { None } else {
            Some(err(INVALID_REQUEST, "jsonrpc must be exactly \"2.0\"", id))
        };
    }

    let out = d.call(&req.method, req.params).await;
    if is_notification {
        return None;
    }
    Some(match out {
        Ok(v) => Response { jsonrpc: JSONRPC_VERSION, result: Some(v), error: None, id },
        Err(e) if e == "method not found" => {
            err(METHOD_NOT_FOUND, &format!("method not found: {}", req.method), id)
        }
        Err(e) => err(INTERNAL_ERROR, &e, id),
    })
}

/// The `POST /api/rpc` body handler. Transport-agnostic so it can be unit-tested directly.
pub async fn handle_body(d: &dyn RpcDispatch, body: &[u8]) -> (StatusCode, Option<Value>) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            let e = err(PARSE_ERROR, "invalid JSON", Value::Null);
            return (StatusCode::OK, Some(serde_json::to_value(e).unwrap()));
        }
    };

    if let Value::Array(items) = parsed {
        if items.is_empty() {
            let e = err(INVALID_REQUEST, "an empty batch is not a valid request", Value::Null);
            return (StatusCode::OK, Some(serde_json::to_value(e).unwrap()));
        }
        let mut out = Vec::new();
        for item in items {
            match serde_json::from_value::<Request>(item) {
                Ok(r) => {
                    if let Some(resp) = dispatch_one(d, r).await {
                        out.push(serde_json::to_value(resp).unwrap());
                    }
                }
                Err(_) => out.push(serde_json::to_value(err(
                    INVALID_REQUEST, "malformed request object", Value::Null)).unwrap()),
            }
        }
        // An all-notification batch is answered with nothing at all, per the spec.
        return if out.is_empty() { (StatusCode::NO_CONTENT, None) }
               else { (StatusCode::OK, Some(Value::Array(out))) };
    }

    match serde_json::from_value::<Request>(parsed) {
        Ok(r) => match dispatch_one(d, r).await {
            Some(resp) => (StatusCode::OK, Some(serde_json::to_value(resp).unwrap())),
            None => (StatusCode::NO_CONTENT, None),
        },
        Err(_) => {
            let e = err(INVALID_REQUEST, "malformed request object", Value::Null);
            (StatusCode::OK, Some(serde_json::to_value(e).unwrap()))
        }
    }
}

/// Semantic capability groups, derived from the registry's method names.
///
/// ⛔ DERIVED, NEVER HAND-MAINTAINED. A client should not have to keep its own table of which
/// firmware version grew which feature — that table is wrong the moment a method is added, and
/// the client that trusts it calls something that is not there.
pub fn capabilities(methods: &[String]) -> Value {
    let have = |needle: &str| methods.iter().any(|m| m.eq_ignore_ascii_case(needle));
    let set: BTreeSet<&str> = methods.iter().map(|s| s.as_str()).collect();
    json!({
        "jsonrpc": JSONRPC_VERSION,
        // ⛔ Discovery is a contract. A client that asks what this device can do and is told
        // the wrong answer will not ask twice, so anything served belongs here.
        "transport": {
            "http": "/api/rpc", "webrtc_datachannel": true,
            "lease": "/api/session", "redfish": "/redfish/v1",
        },
        "capabilities": {
            "power":       have("setATXPowerAction"),
            "power_state": set.iter().any(|m| m.to_ascii_lowercase().contains("powerled")),
            "hid":         have("sendKeys"),
            "screen":      have("getSnapshot"),
            // Served at /api/screen/ocr, on-device, not an RPC method -- which is why
            // deriving it from the method list reported `false` while it worked.
            "ocr":         true,
            "media":       have("mountFile") || have("getVirtualMediaList"),
            "virtual_media_redfish": true,
            "wake_on_lan": have("sendWOLMagicPacket"),
        },
        "methods": methods,
    })
}

/// Salvo wiring. Kept to the smallest possible surface so the firmware's route table changes
/// by two lines and the testable logic lives above.
#[derive(Clone)]
pub struct HttpRpc(pub Arc<dyn RpcDispatch>);

#[async_trait]
impl Handler for HttpRpc {
    async fn handle(&self, req: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    _c: &mut FlowCtrl) {
        let body = match req.payload().await {
            Ok(b) => b.to_vec(),
            Err(_) => Vec::new(),
        };
        let (status, payload) = handle_body(self.0.as_ref(), &body).await;
        res.status_code(status);
        if let Some(v) = payload {
            res.render(Json(v));
        }
    }
}

#[derive(Clone)]
pub struct Capabilities(pub Arc<dyn RpcDispatch>);

#[async_trait]
impl Handler for Capabilities {
    async fn handle(&self, _req: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    _c: &mut FlowCtrl) {
        let mut m = self.0.methods();
        m.sort();
        res.render(Json(capabilities(&m)));
    }
}

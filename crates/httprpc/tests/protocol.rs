//! The HTTP transport's behaviour, verified on a host with no device, no cross toolchain and
//! no Rockchip sysroot — which is the entire reason this logic lives in its own crate.
//!
//! Protocol tests, not device tests. They pin what a machine client depends on and what is
//! easy to get subtly wrong: notifications that must not be answered, batches, the difference
//! between a transport failure and an application failure, and a capability document that is
//! derived rather than remembered.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use httprpc::{capabilities, handle_body, RpcDispatch};
use salvo::http::StatusCode;
use serde_json::{json, Value};

/// A registry stand-in. Records what it was asked to do, so a test can assert on what was NOT
/// dispatched — which is the half that matters for notifications and malformed input.
struct Fake {
    methods: Vec<String>,
    calls: Mutex<Vec<String>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            methods: ["setATXPowerAction", "sendKeys", "getSnapshot", "mountFile",
                      "getVirtualMediaList", "sendWOLMagicPacket", "powerLedState", "explode"]
                .iter().map(|s| s.to_string()).collect(),
            calls: Mutex::new(Vec::new()),
        })
    }
    fn calls(&self) -> Vec<String> { self.calls.lock().unwrap().clone() }
}

impl RpcDispatch for Fake {
    fn call(&self, method: &str, params: Option<Value>) -> BoxFuture<'_, Result<Value, String>> {
        self.calls.lock().unwrap().push(method.to_string());
        let known = self.methods.iter().any(|m| m == method);
        let m = method.to_string();
        Box::pin(async move {
            if !known { return Err("method not found".to_string()); }
            if m == "explode" { return Err("the device refused".to_string()); }
            Ok(json!({ "ok": true, "method": m, "params": params }))
        })
    }
    fn methods(&self) -> Vec<String> { self.methods.clone() }
}

async fn post(d: &Fake, body: &str) -> (StatusCode, Option<Value>) {
    handle_body(d, body.as_bytes()).await
}

#[tokio::test]
async fn a_request_with_an_id_gets_a_result() {
    let d = Fake::new();
    let (code, body) = post(&d, r#"{"jsonrpc":"2.0","id":1,"method":"getSnapshot"}"#).await;
    assert_eq!(code, StatusCode::OK);
    let b = body.unwrap();
    assert_eq!(b["id"], json!(1));
    assert_eq!(b["result"]["ok"], json!(true));
    assert!(b.get("error").is_none());
}

#[tokio::test]
async fn a_notification_is_dispatched_but_never_answered() {
    // ⛔ Easy to get wrong. A notification has no `id`; answering it breaks clients that are
    // not waiting for a reply, and the data-channel path already skips it.
    let d = Fake::new();
    let (code, body) = post(&d, r#"{"jsonrpc":"2.0","method":"sendKeys","params":{"k":["a"]}}"#).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    assert!(body.is_none(), "a notification was answered");
    assert_eq!(d.calls(), vec!["sendKeys"], "a notification must still be DISPATCHED");
}

#[tokio::test]
async fn an_unknown_method_is_an_envelope_error_not_a_transport_error() {
    // ⛔ HTTP 200 carrying a JSON-RPC error. Collapsing application failure into a transport
    // status is how a client ends up retrying a refusal.
    let d = Fake::new();
    let (code, body) = post(&d, r#"{"jsonrpc":"2.0","id":7,"method":"noSuchThing"}"#).await;
    assert_eq!(code, StatusCode::OK);
    let b = body.unwrap();
    assert_eq!(b["error"]["code"], json!(-32601));
    assert_eq!(b["id"], json!(7));
}

#[tokio::test]
async fn a_handler_that_fails_is_an_internal_error_carrying_its_message() {
    let d = Fake::new();
    let (code, body) = post(&d, r#"{"jsonrpc":"2.0","id":2,"method":"explode"}"#).await;
    assert_eq!(code, StatusCode::OK);
    let b = body.unwrap();
    assert_eq!(b["error"]["code"], json!(-32603));
    assert_eq!(b["error"]["message"], json!("the device refused"));
}

#[tokio::test]
async fn malformed_json_is_a_parse_error_and_reaches_no_handler() {
    let d = Fake::new();
    let (code, body) = post(&d, "{not json").await;
    assert_eq!(code, StatusCode::OK);
    let b = body.unwrap();
    assert_eq!(b["error"]["code"], json!(-32700));
    assert_eq!(b["id"], Value::Null);
    assert!(d.calls().is_empty(), "unparseable input reached a handler");
}

#[tokio::test]
async fn a_wrong_protocol_version_is_refused_rather_than_guessed_at() {
    // Leniency here means a client with a typo gets a different method than it meant.
    let d = Fake::new();
    let (_c, body) = post(&d, r#"{"jsonrpc":"1.0","id":1,"method":"getSnapshot"}"#).await;
    assert_eq!(body.unwrap()["error"]["code"], json!(-32600));
    assert!(d.calls().is_empty(), "a bad-version request was dispatched anyway");
}

#[tokio::test]
async fn a_batch_returns_one_response_per_answerable_request() {
    let d = Fake::new();
    let (code, body) = post(&d, r#"[
        {"jsonrpc":"2.0","id":1,"method":"getSnapshot"},
        {"jsonrpc":"2.0","method":"sendKeys"},
        {"jsonrpc":"2.0","id":3,"method":"nope"}
    ]"#).await;
    assert_eq!(code, StatusCode::OK);
    let b = body.unwrap();
    let arr = b.as_array().unwrap();
    assert_eq!(arr.len(), 2, "the notification should contribute no response");
    assert_eq!(arr[0]["id"], json!(1));
    assert_eq!(arr[1]["error"]["code"], json!(-32601));
    assert_eq!(d.calls(), vec!["getSnapshot", "sendKeys", "nope"]);
}

#[tokio::test]
async fn a_batch_of_only_notifications_is_answered_with_nothing() {
    let d = Fake::new();
    let (code, body) = post(&d, r#"[{"jsonrpc":"2.0","method":"sendKeys"},
                                   {"jsonrpc":"2.0","method":"sendKeys"}]"#).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    assert!(body.is_none());
    assert_eq!(d.calls().len(), 2, "both notifications must still be dispatched");
}

#[tokio::test]
async fn an_empty_batch_is_an_invalid_request() {
    let d = Fake::new();
    let (_c, body) = post(&d, "[]").await;
    assert_eq!(body.unwrap()["error"]["code"], json!(-32600));
}

#[tokio::test]
async fn capabilities_are_derived_from_the_registry_not_remembered() {
    // ⛔ A hand-maintained capability table is wrong the moment a method is added, and the
    // client that trusts it calls something that is not there.
    let d = Fake::new();
    let caps = capabilities(&d.methods());
    for feature in ["power", "hid", "screen", "media", "wake_on_lan"] {
        assert_eq!(caps["capabilities"][feature], json!(true), "{feature} should be present");
    }
    // honest about what this device cannot do today: no server-side OCR
    assert_eq!(caps["capabilities"]["ocr"], json!(false));
    assert_eq!(caps["transport"]["http"], json!("/api/rpc"));
}

#[tokio::test]
async fn a_registry_without_a_feature_reports_it_absent() {
    struct Bare;
    impl RpcDispatch for Bare {
        fn call(&self, _m: &str, _p: Option<Value>) -> BoxFuture<'_, Result<Value, String>> {
            Box::pin(async { Err("method not found".into()) })
        }
        fn methods(&self) -> Vec<String> { vec!["getNetworkState".into()] }
    }
    let caps = capabilities(&Bare.methods());
    assert_eq!(caps["capabilities"]["power"], json!(false));
    assert_eq!(caps["capabilities"]["screen"], json!(false));
}

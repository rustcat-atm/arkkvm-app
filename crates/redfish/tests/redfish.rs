//! Redfish mapping, and above all the pulse guard.
//!
//! ⛔ Redfish clients send `ResetType: On` speculatively, because against a real BMC that is
//! idempotent and harmless. This device's power control is a BUTTON PRESS, so the naive
//! mapping turns a running host OFF in response to a request to turn it on. Most of this file
//! is about that one hazard.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use httprpc::RpcDispatch;
use redfish::{eject_media, insert_media, power, read_power, system, ALLOWED_RESETS};
use salvo::http::StatusCode;
use serde_json::{json, Value};

/// `power_led`: Some(true/false) = the LED reads; None = it cannot be read at all.
struct Dev {
    power_led: Option<bool>,
    calls: Mutex<Vec<(String, Option<Value>)>>,
}

impl Dev {
    fn new(power_led: Option<bool>) -> Arc<Self> {
        Arc::new(Self { power_led, calls: Mutex::new(Vec::new()) })
    }
    fn atx_calls(&self) -> Vec<Value> {
        self.calls.lock().unwrap().iter()
            .filter(|(m, _)| m == "setATXPowerAction")
            .filter_map(|(_, p)| p.clone()).collect()
    }
}

impl RpcDispatch for Dev {
    fn call(&self, method: &str, params: Option<Value>) -> BoxFuture<'_, Result<Value, String>> {
        self.calls.lock().unwrap().push((method.to_string(), params.clone()));
        let led = self.power_led;
        let m = method.to_string();
        Box::pin(async move {
            match m.as_str() {
                "getPowerLedState" | "powerLedState" | "getPowerLed" => match led {
                    Some(b) => Ok(json!({ "on": b })),
                    None => Err("cannot read".into()),
                },
                "setATXPowerAction" | "mountFile" | "unmountImage" => Ok(json!({ "ok": true })),
                _ => Err("method not found".into()),
            }
        })
    }
    fn methods(&self) -> Vec<String> {
        ["setATXPowerAction", "getPowerLedState", "mountFile", "unmountImage"]
            .iter().map(|s| s.to_string()).collect()
    }
}

// --- the negative control -----------------------------------------------------------

#[tokio::test]
async fn a_legitimate_power_on_against_an_off_host_succeeds() {
    // Without this, every refusal below passes against a mapping that refuses everything.
    let d = Dev::new(Some(false));
    let (status, _) = power(d.as_ref(), "On").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(d.atx_calls(), vec![json!({ "action": "power-short" })]);
}

// --- ⛔ the pulse guard ----------------------------------------------------------------

#[tokio::test]
async fn powering_on_an_already_running_host_is_refused_not_pressed() {
    // ⛔ The outage this guard exists to prevent: against a real BMC `On` is idempotent, so
    // clients send it freely; here it would press the button and shut the machine down.
    let d = Dev::new(Some(true));
    let (status, body) = power(d.as_ref(), "On").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(d.atx_calls().is_empty(), "it pressed the power button on a running host");
    let msg = body.unwrap()["error"]["message"].as_str().unwrap().to_string();
    assert!(msg.contains("already on"), "{msg}");
}

#[tokio::test]
async fn shutting_down_an_already_off_host_is_refused_not_pressed() {
    let d = Dev::new(Some(false));
    let (status, _) = power(d.as_ref(), "GracefulShutdown").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(d.atx_calls().is_empty(), "it pressed the button on an already-off host");
}

#[tokio::test]
async fn an_unreadable_power_state_refuses_rather_than_pressing_blind() {
    // ⛔ "Unknown" must never be treated as "off". The dangerous direction of a missing
    // reading is always the reassuring one.
    let d = Dev::new(None);
    let (status, body) = power(d.as_ref(), "On").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(d.atx_calls().is_empty(), "it pressed blind with no state reading");
    assert!(body.unwrap()["error"]["message"].as_str().unwrap().contains("BUTTON PRESS"));
}

#[tokio::test]
async fn push_power_button_deliberately_skips_the_guard() {
    // The escape hatch: this action IS "press the button", so it must work regardless of state.
    let d = Dev::new(Some(true));
    let (status, _) = power(d.as_ref(), "PushPowerButton").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(d.atx_calls(), vec![json!({ "action": "power-short" })]);
}

#[tokio::test]
async fn force_restart_needs_no_state_because_it_has_no_target_state() {
    let d = Dev::new(None);
    let (status, _) = power(d.as_ref(), "ForceRestart").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(d.atx_calls(), vec![json!({ "action": "reset" })]);
}

#[tokio::test]
async fn force_off_uses_a_long_press() {
    // A short press against a hung host does nothing at all — and does it silently.
    let d = Dev::new(Some(true));
    let (status, _) = power(d.as_ref(), "ForceOff").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(d.atx_calls(), vec![json!({ "action": "power-long" })]);
}

// --- protocol shape --------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_reset_type_is_a_400_listing_what_is_allowed() {
    let d = Dev::new(Some(false));
    let (status, body) = power(d.as_ref(), "Nuke").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(d.atx_calls().is_empty());
    let msg = body.unwrap()["error"]["message"].as_str().unwrap().to_string();
    assert!(msg.contains("PushPowerButton"), "the error must list allowable values: {msg}");
}

#[tokio::test]
async fn an_unreadable_led_reports_power_state_unknown_not_off() {
    assert_eq!(system(None)["PowerState"], json!("Unknown"));
    assert_eq!(system(Some(true))["PowerState"], json!("On"));
    assert_eq!(system(Some(false))["PowerState"], json!("Off"));
}

#[tokio::test]
async fn the_system_advertises_exactly_the_reset_types_it_implements() {
    let s = system(Some(true));
    let allowed = s["Actions"]["#ComputerSystem.Reset"]["ResetType@Redfish.AllowableValues"]
        .as_array().unwrap().clone();
    assert_eq!(allowed.len(), ALLOWED_RESETS.len());
    for rt in ALLOWED_RESETS {
        assert!(allowed.contains(&json!(rt)), "{rt} advertised but not implemented, or vice versa");
    }
}

#[tokio::test]
async fn read_power_returns_none_when_no_method_answers() {
    assert_eq!(read_power(Dev::new(None).as_ref()).await, None);
}

#[tokio::test]
async fn virtual_media_insert_and_eject_map_onto_the_device() {
    let d = Dev::new(Some(true));
    assert_eq!(insert_media(d.as_ref(), "http://x/y.iso").await.0, StatusCode::NO_CONTENT);
    assert_eq!(eject_media(d.as_ref()).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn inserting_an_empty_image_is_a_400_not_a_mount_of_nothing() {
    let d = Dev::new(Some(true));
    let (status, _) = insert_media(d.as_ref(), "   ").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(d.calls.lock().unwrap().iter().all(|(m, _)| m != "mountFile"));
}

/// ⛔ EVERY ADVERTISED LINK MUST HAVE A DOCUMENT BEHIND IT.
///
/// `service_root()` advertised `/redfish/v1/Managers` while nothing served it, so a client
/// walking the links 404'd one hop in and never reached the virtual media that did exist.
/// A dangling link is worse than an absent feature: the client finds it and trusts it.
///
/// This walks every `@odata.id` and action `target` the service emits and fails on any path
/// that is not in the served set. Adding a link without routing it breaks this test; the list
/// below is the one place to update, and it must match `init_*_routes` in `arkkvm/src/web.rs`.
#[test]
fn every_link_the_service_advertises_is_a_path_we_serve() {
    const SERVED: &[&str] = &[
        "/redfish/v1",
        "/redfish/v1/Systems",
        "/redfish/v1/Systems/0",
        "/redfish/v1/Systems/0/Actions/ComputerSystem.Reset",
        "/redfish/v1/Managers",
        "/redfish/v1/Managers/BMC",
        "/redfish/v1/Managers/BMC/VirtualMedia",
        "/redfish/v1/Managers/BMC/VirtualMedia/MSD",
        "/redfish/v1/Managers/BMC/VirtualMedia/MSD/Actions/VirtualMedia.InsertMedia",
        "/redfish/v1/Managers/BMC/VirtualMedia/MSD/Actions/VirtualMedia.EjectMedia",
    ];

    fn links(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, val) in m {
                    if (k == "@odata.id" || k == "target") && let Some(s) = val.as_str() {
                        out.push(s.to_string());
                    }
                    links(val, out);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| links(x, out)),
            _ => {}
        }
    }

    let docs = [
        redfish::service_root(),
        redfish::systems_collection(),
        redfish::system(Some(true)),
        redfish::managers_collection(),
        redfish::manager(),
        redfish::virtual_media_collection(),
        redfish::virtual_media(true, Some("http://example/x.iso"), true),
    ];
    let mut found = Vec::new();
    for d in &docs {
        links(d, &mut found);
    }
    found.sort();
    found.dedup();
    assert!(!found.is_empty(), "walked the documents and found no links at all");
    for l in &found {
        assert!(SERVED.contains(&l.as_str()), "advertised {l} but nothing serves it");
    }
}

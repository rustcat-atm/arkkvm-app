//! A minimal Redfish surface, mapped onto the device's own RPC registry.
//!
//! # Why bother, when `/api/rpc` already exists
//!
//! Redfish is the only standard in this space. PiKVM implements it, and so do iDRAC, iLO and
//! Supermicro. A device that speaks it works **on day one** with every tool that already
//! drives a BMC — no per-vendor driver, no reverse engineering, no waiting for someone to
//! write an integration. It is two endpoints for an enormous amount of interoperability.
//!
//! The scope is deliberately small: Redfish has no keyboard and no screen, so this covers
//! power and virtual media only. Anything richer goes through `/api/rpc`.
//!
//! # ⛔ The pulse problem, again
//!
//! `setATXPowerAction` takes `power-short` / `power-long` / `reset` — it presses the physical
//! button. It is **not** a state command, so a naive `ResetType: On` against a host that is
//! already running SHUTS IT DOWN. Redfish clients absolutely do send `On` speculatively,
//! because against a real BMC that is idempotent and harmless.
//!
//! So `On` and the off variants read the power LED first and REFUSE when the host is already
//! in the requested state, or when the state cannot be read at all. A refusal is a 409; a
//! blind press is an unplanned outage.
//!
//! # Status codes
//!
//! Redfish actions answer `204 No Content` on success, which is what clients expect. Errors
//! use Redfish's own `error` envelope rather than a bare string, because a client that parses
//! Redfish errors should not need a special case for this device.

use std::sync::Arc;

use httprpc::RpcDispatch;
use salvo::prelude::*;
use serde_json::{json, Value};

/// Redfish ResetType -> (the device's ATX action, the state this is trying to reach)
fn map_reset(reset_type: &str) -> Option<(&'static str, Option<bool>)> {
    match reset_type {
        "On" | "ForceOn" => Some(("power-short", Some(true))),
        "GracefulShutdown" => Some(("power-short", Some(false))),
        "ForceOff" => Some(("power-long", Some(false))),
        "ForceRestart" | "PowerCycle" => Some(("reset", None)),
        "PushPowerButton" => Some(("power-short", None)), // explicitly a BUTTON: no guard
        _ => None,
    }
}

pub const ALLOWED_RESETS: [&str; 6] =
    ["On", "ForceOn", "GracefulShutdown", "ForceOff", "ForceRestart", "PushPowerButton"];

fn rf_error(code: &str, message: &str) -> Value {
    json!({ "error": { "code": code, "message": message,
                       "@Message.ExtendedInfo": [{ "Message": message, "Severity": "Warning" }] } })
}

/// `true` / `false` / `None` when it cannot be read. ⛔ None is NOT false: see `power()`.
pub async fn read_power(d: &dyn RpcDispatch) -> Option<bool> {
    for method in ["getPowerLedState", "powerLedState", "getPowerLed"] {
        if let Ok(v) = d.call(method, None).await
            && let Some(b) = v
                .get("on")
                .and_then(Value::as_bool)
                .or_else(|| v.get("state").and_then(Value::as_bool))
                .or_else(|| v.as_bool())
        {
            return Some(b);
        }
    }
    None
}

pub fn service_root() -> Value {
    json!({
        "@odata.id": "/redfish/v1",
        "@odata.type": "#ServiceRoot.v1_5_0.ServiceRoot",
        "Id": "RootService", "Name": "ArkKVM Redfish Service", "RedfishVersion": "1.6.0",
        "Systems": { "@odata.id": "/redfish/v1/Systems" },
        "Managers": { "@odata.id": "/redfish/v1/Managers" },
    })
}

pub fn systems_collection() -> Value {
    json!({
        "@odata.id": "/redfish/v1/Systems",
        "@odata.type": "#ComputerSystemCollection.ComputerSystemCollection",
        "Name": "Computer System Collection", "Members@odata.count": 1,
        "Members": [{ "@odata.id": "/redfish/v1/Systems/0" }],
    })
}

pub fn system(power: Option<bool>) -> Value {
    json!({
        "@odata.id": "/redfish/v1/Systems/0",
        "@odata.type": "#ComputerSystem.v1_13_0.ComputerSystem",
        "Id": "0", "Name": "Attached Host",
        // ⛔ "Unknown" is a real Redfish PowerState and the honest answer when the LED cannot
        // be read. Reporting "Off" because we could not tell is how a client powers on a
        // running machine.
        "PowerState": match power { Some(true) => "On", Some(false) => "Off", None => "Unknown" },
        "Actions": { "#ComputerSystem.Reset": {
            "target": "/redfish/v1/Systems/0/Actions/ComputerSystem.Reset",
            "ResetType@Redfish.AllowableValues": ALLOWED_RESETS,
        }},
    })
}

/// Perform a reset. Returns `(status, optional body)`.
pub async fn power(d: &dyn RpcDispatch, reset_type: &str) -> (StatusCode, Option<Value>) {
    let Some((action, want_on)) = map_reset(reset_type) else {
        return (StatusCode::BAD_REQUEST,
                Some(rf_error("Base.1.0.ActionParameterNotSupported",
                              &format!("ResetType {reset_type:?} is not supported; allowable \
                                        values are {ALLOWED_RESETS:?}"))));
    };

    // ⛔ The pulse guard. Only for actions that name a target state — PushPowerButton and
    // ForceRestart deliberately skip it, because those ARE "press the button".
    if let Some(want) = want_on {
        match read_power(d).await {
            None => return (StatusCode::CONFLICT, Some(rf_error(
                "Base.1.0.ResourceAtUriUnauthorized",
                "refusing: the host power state could not be read, and this device's power \
                 control is a BUTTON PRESS rather than a state command. Pressing blind would \
                 turn a running host off. Use PushPowerButton if that is genuinely intended."))),
            Some(is_on) if is_on == want => return (StatusCode::CONFLICT, Some(rf_error(
                "Base.1.0.ActionNotSupported",
                &format!("refusing: the host is already {}, and this device's power control is \
                          a button press — it would move the host AWAY from the requested \
                          state, not towards it",
                         if is_on { "on" } else { "off" })))),
            Some(_) => {}
        }
    }

    match d.call("setATXPowerAction", Some(json!({ "action": action }))).await {
        Ok(_) => (StatusCode::NO_CONTENT, None),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
                   Some(rf_error("Base.1.0.InternalError", &e))),
    }
}

/// The chain the service root promises: Managers -> BMC -> VirtualMedia -> MSD. Every hop
/// needs a document, or a conforming client walking the links 404s before it reaches the
/// media. `service_root` advertised `Managers` while none of these existed.
pub fn managers_collection() -> Value {
    json!({
        "@odata.id": "/redfish/v1/Managers",
        "@odata.type": "#ManagerCollection.ManagerCollection",
        "Name": "Manager Collection", "Members@odata.count": 1,
        "Members": [{ "@odata.id": "/redfish/v1/Managers/BMC" }],
    })
}

pub fn manager() -> Value {
    json!({
        "@odata.id": "/redfish/v1/Managers/BMC",
        "@odata.type": "#Manager.v1_5_0.Manager",
        "Id": "BMC", "Name": "ArkKVM", "ManagerType": "KVM",
        "VirtualMedia": { "@odata.id": "/redfish/v1/Managers/BMC/VirtualMedia" },
    })
}

pub fn virtual_media_collection() -> Value {
    json!({
        "@odata.id": "/redfish/v1/Managers/BMC/VirtualMedia",
        "@odata.type": "#VirtualMediaCollection.VirtualMediaCollection",
        "Name": "Virtual Media Collection", "Members@odata.count": 1,
        "Members": [{ "@odata.id": "/redfish/v1/Managers/BMC/VirtualMedia/MSD" }],
    })
}

pub fn virtual_media(inserted: bool, image: Option<&str>, write_protected: bool) -> Value {
    json!({
        "@odata.id": "/redfish/v1/Managers/BMC/VirtualMedia/MSD",
        "@odata.type": "#VirtualMedia.v1_3_0.VirtualMedia",
        "Id": "MSD", "Name": "Virtual Media", "MediaTypes": ["CD", "USBStick"],
        "Inserted": inserted, "Image": image, "WriteProtected": write_protected,
        "Actions": {
            "#VirtualMedia.InsertMedia": { "target":
                "/redfish/v1/Managers/BMC/VirtualMedia/MSD/Actions/VirtualMedia.InsertMedia" },
            "#VirtualMedia.EjectMedia": { "target":
                "/redfish/v1/Managers/BMC/VirtualMedia/MSD/Actions/VirtualMedia.EjectMedia" },
        },
    })
}

pub async fn insert_media(d: &dyn RpcDispatch, image: &str) -> (StatusCode, Option<Value>) {
    if image.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Some(rf_error(
            "Base.1.0.PropertyMissing", "Image is required")));
    }
    match d.call("mountFile", Some(json!({ "url": image }))).await {
        Ok(_) => (StatusCode::NO_CONTENT, None),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
                   Some(rf_error("Base.1.0.InternalError", &e))),
    }
}

pub async fn eject_media(d: &dyn RpcDispatch) -> (StatusCode, Option<Value>) {
    match d.call("unmountImage", None).await {
        Ok(_) => (StatusCode::NO_CONTENT, None),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
                   Some(rf_error("Base.1.0.InternalError", &e))),
    }
}

// --- salvo wiring ---------------------------------------------------------------------

macro_rules! json_handler {
    ($name:ident, $body:expr) => {
        #[derive(Clone)]
        pub struct $name;
        #[async_trait]
        impl Handler for $name {
            async fn handle(&self, _q: &mut salvo::Request, _d: &mut Depot,
                            res: &mut salvo::Response, _c: &mut FlowCtrl) {
                res.render(Json($body));
            }
        }
    };
}
json_handler!(ServiceRoot, service_root());
json_handler!(SystemsCollection, systems_collection());
json_handler!(ManagersCollection, managers_collection());
json_handler!(Manager, manager());
json_handler!(VirtualMediaCollection, virtual_media_collection());

#[derive(Clone)]
pub struct System(pub Arc<dyn RpcDispatch>);

#[async_trait]
impl Handler for System {
    async fn handle(&self, _q: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    _c: &mut FlowCtrl) {
        res.render(Json(system(read_power(self.0.as_ref()).await)));
    }
}

#[derive(Clone)]
pub struct Reset(pub Arc<dyn RpcDispatch>);

#[async_trait]
impl Handler for Reset {
    async fn handle(&self, q: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    _c: &mut FlowCtrl) {
        let body: Value = q.parse_json().await.unwrap_or(Value::Null);
        let rt = body.get("ResetType").and_then(Value::as_str).unwrap_or("");
        let (status, payload) = power(self.0.as_ref(), rt).await;
        res.status_code(status);
        if let Some(v) = payload { res.render(Json(v)); }
    }
}

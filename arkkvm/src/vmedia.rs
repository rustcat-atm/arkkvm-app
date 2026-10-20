//! Redfish virtual media: read what is mounted, insert an image, eject it.
//!
//! `crates/redfish::virtual_media()` was written and tested but never routed, so the resource
//! it describes did not exist and the function was dead code. The device can already do the
//! work — `mount_with_http` is NBD-backed and is precisely InsertMedia's semantics — so this
//! connects the two rather than deleting a capability that was most of the way there.
//!
//! ⛔ Insert and eject are state-changing and are mounted under the lease guard. Only the GET
//! sits outside it: reading what is mounted must never be the thing that is refused.

use salvo::prelude::*;
use serde_json::{json, Value};

use crate::hardware::usb::storage::{
    get_virtual_media_state, mount_with_http, unmount_image, VirtualMediaMode,
};

fn fail(res: &mut Response, code: StatusCode, msg: String) {
    res.status_code(code);
    res.render(Json(json!({ "error": msg })));
}

#[derive(Clone)]
pub struct Resource;

#[async_trait]
impl Handler for Resource {
    async fn handle(&self, _q: &mut Request, _d: &mut Depot, res: &mut Response, _c: &mut FlowCtrl) {
        let st = get_virtual_media_state();
        let image = st.as_ref().and_then(|s| s.url.clone().or_else(|| s.filename.clone()));
        // CDROM is read-only; Disk is not. Reporting a fixed `true` would have been a lie the
        // moment a caller asked for USBStick -- which the Insert handler below allows.
        let ro = st.as_ref().is_none_or(|s| s.mode == VirtualMediaMode::CDROM);
        res.render(Json(redfish::virtual_media(st.is_some(), image.as_deref(), ro)));
    }
}

#[derive(Clone)]
pub struct Insert;

#[async_trait]
impl Handler for Insert {
    async fn handle(&self, req: &mut Request, _d: &mut Depot, res: &mut Response, _c: &mut FlowCtrl) {
        let body: Value = req.parse_json().await.unwrap_or(Value::Null);
        let Some(image) = body.get("Image").and_then(Value::as_str).filter(|s| !s.is_empty())
        else {
            return fail(res, StatusCode::BAD_REQUEST, "InsertMedia needs a non-empty `Image` URL".into());
        };
        // The resource advertises MediaTypes ["CD","USBStick"]; honour the one the caller picks.
        let mode = match body.get("MediaType").and_then(Value::as_str) {
            Some("USBStick") | Some("Disk") => VirtualMediaMode::Disk,
            _ => VirtualMediaMode::CDROM,
        };
        match mount_with_http(image, mode).await {
            Ok(()) => { res.status_code(StatusCode::NO_CONTENT); }
            Err(e) => fail(res, StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
}

#[derive(Clone)]
pub struct Eject;

#[async_trait]
impl Handler for Eject {
    async fn handle(&self, _q: &mut Request, _d: &mut Depot, res: &mut Response, _c: &mut FlowCtrl) {
        match unmount_image().await {
            Ok(()) => { res.status_code(StatusCode::NO_CONTENT); }
            Err(e) => fail(res, StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
}

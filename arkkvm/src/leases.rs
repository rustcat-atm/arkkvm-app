//! The device's control lease, and the HTTP endpoints that manage it.
//!
//! All the logic lives in `crates/sessionlease`, which is clock-injected and tested on a host.
//! This file holds the one global, the salvo handlers, and the guard that other HTTP routes
//! call.
//!
//! ⛔ SCOPE, STATED PLAINLY. This lease governs the **HTTP control plane** (`/api/rpc`, the
//! Redfish surface). It does NOT yet change WebRTC session promotion, which still seizes
//! unconditionally in `AppState::promote_session_if_registered`. Making the data channel
//! respect the lease is the right end state and a larger behavioural change to the path every
//! interactive user takes — it deserves its own step, and one that can be tried on a device.
//!
//! Stating the boundary rather than implying full coverage: a client that acquires a lease
//! today is protected from other HTTP callers, not from someone opening the web UI.

use std::sync::Arc;

use salvo::prelude::*;
use serde_json::{json, Value};
use sessionlease::{Acquired, LeaseManager, DEFAULT_TTL_SECS};
use tokio::sync::Mutex;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

static MANAGER: std::sync::OnceLock<Arc<Mutex<LeaseManager>>> = std::sync::OnceLock::new();

pub fn manager() -> Arc<Mutex<LeaseManager>> {
    MANAGER
        .get_or_init(|| Arc::new(Mutex::new(LeaseManager::new(DEFAULT_TTL_SECS))))
        .clone()
}

/// Whether this request may drive the device.
///
/// ⛔ NO LEASE HELD BY ANYONE = ALLOWED. Deliberate: making the lease mandatory overnight
/// would break every existing client, including the device's own web UI. The lease is
/// opt-in protection — once somebody takes it, others are locked out until it lapses — which
/// is the increment that can ship without an outage. Mandatory leasing is a later, louder
/// change.
pub async fn may_drive(token: Option<&str>) -> Result<(), Value> {
    let mgr = manager();
    let mut m = mgr.lock().await;
    let now = now_secs();
    match m.current(now) {
        None => Ok(()),
        Some(held) => match token {
            Some(t) if t == held.token => Ok(()),
            _ => Err(json!({
                "error": "the device is leased by another client",
                "held_by": held.holder,
                "expires_in_secs": held.expires_at.saturating_sub(now),
                "hint": "POST /api/session with a reason to take it over, or wait for it to lapse",
            })),
        },
    }
}

fn token_of(req: &salvo::Request) -> Option<String> {
    req.header::<String>("x-arkkvm-lease")
        .or_else(|| req.query::<String>("lease"))
}

#[derive(Clone)]
pub struct Acquire;

#[async_trait]
impl Handler for Acquire {
    async fn handle(&self, req: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    _c: &mut FlowCtrl) {
        let body: Value = req.parse_json().await.unwrap_or(Value::Null);
        let who = body.get("holder").and_then(Value::as_str).unwrap_or("anonymous").to_string();
        let reason = body.get("reason").and_then(Value::as_str).unwrap_or("").to_string();

        let mgr = manager();
        let mut m = mgr.lock().await;
        match m.acquire(&who, now_secs(), &reason) {
            Acquired::Granted { lease } => {
                res.status_code(StatusCode::CREATED);
                res.render(Json(json!({ "outcome": "granted", "lease": lease,
                                        "ttl_secs": m.ttl_secs() })));
            }
            Acquired::TookOver { lease, displaced, reason } => {
                res.status_code(StatusCode::CREATED);
                res.render(Json(json!({ "outcome": "took_over", "lease": lease,
                                        "displaced": displaced, "reason": reason })));
            }
            // ⛔ 409, not 403. The device is fine and the caller is not forbidden — someone
            // else is simply using it, which is a conflict the client can act on.
            Acquired::Refused { held_by, expires_in_secs } => {
                res.status_code(StatusCode::CONFLICT);
                res.render(Json(json!({ "outcome": "refused", "held_by": held_by,
                                        "expires_in_secs": expires_in_secs,
                                        "hint": "include a non-empty `reason` to take it over" })));
            }
        }
    }
}

#[derive(Clone)]
pub struct Renew;

#[async_trait]
impl Handler for Renew {
    async fn handle(&self, req: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    _c: &mut FlowCtrl) {
        let Some(tok) = token_of(req) else {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(json!({ "error": "no lease token (X-ArkKVM-Lease)" })));
            return;
        };
        let mgr = manager();
        let mut m = mgr.lock().await;
        match m.renew(&tok, now_secs()) {
            Some(lease) => res.render(Json(json!({ "outcome": "renewed", "lease": lease }))),
            None => {
                res.status_code(StatusCode::CONFLICT);
                res.render(Json(json!({ "error": "that lease is not the live one; it lapsed \
                                                  or was taken over" })));
            }
        }
    }
}

#[derive(Clone)]
pub struct Release;

#[async_trait]
impl Handler for Release {
    async fn handle(&self, req: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    _c: &mut FlowCtrl) {
        let tok = token_of(req).unwrap_or_default();
        let mgr = manager();
        let mut m = mgr.lock().await;
        if m.release(&tok, now_secs()) {
            res.status_code(StatusCode::NO_CONTENT);
        } else {
            res.status_code(StatusCode::CONFLICT);
            res.render(Json(json!({ "error": "that lease is not the live one" })));
        }
    }
}

#[derive(Clone)]
pub struct Status;

#[async_trait]
impl Handler for Status {
    async fn handle(&self, _q: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    _c: &mut FlowCtrl) {
        let mgr = manager();
        let mut m = mgr.lock().await;
        let now = now_secs();
        let cur = m.current(now);
        res.render(Json(json!({
            "held": cur.is_some(),
            "lease": cur.as_ref().map(|l| json!({
                "holder": l.holder, "acquired_at": l.acquired_at, "expires_at": l.expires_at,
                "expires_in_secs": l.expires_at.saturating_sub(now),
            })),
            "ttl_secs": m.ttl_secs(),
            // ⛔ The breadcrumb trail for "who took the device at 03:14". Bounded in memory;
            // this is a device with 256MB of RAM, not a log server.
            "recent": m.audit(),
        })));
    }
}

/// Middleware: refuse a state-changing call from anyone who is not the lease holder.
///
/// ⛔ APPLIED AS A HOOP, NOT CHECKED INSIDE EACH HANDLER. A per-handler check is one a new
/// route can forget, and the route most likely to forget it is the next power-adjacent one
/// somebody adds in a hurry. A hoop covers everything under it by construction.
#[derive(Clone)]
pub struct LeaseGuard;

#[async_trait]
impl Handler for LeaseGuard {
    async fn handle(&self, req: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    ctrl: &mut FlowCtrl) {
        if let Err(why) = may_drive(token_of(req).as_deref()).await {
            res.status_code(StatusCode::CONFLICT);
            res.render(Json(why));
            ctrl.skip_rest();
        }
    }
}

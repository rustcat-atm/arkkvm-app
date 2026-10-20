//! Server-side frame capture, and reading the host's screen as text.
//!
//! ⛔ THERE WAS NO SERVER-SIDE FRAME GRAB AT ALL. The web UI's "screenshot" is rendered in the
//! browser from the WebRTC video track, which is why `getSnapshot` appears in the frontend
//! bundle and nowhere in the firmware. Every consumer therefore needed a WebRTC stack and an
//! H.264 decoder to see a single frame — and OCR on the device was impossible, because the
//! device never had a frame in hand.
//!
//! ⛔ THE CAPTURE LOOP IS THE HOT PATH AND MUST NOT PAY FOR THIS. The tap is demand-driven:
//! steady-state cost is one relaxed atomic load per frame, and nothing is copied unless an
//! HTTP caller has asked for a frame since the last one was served. A KVM that drops frames
//! because somebody *might* want a screenshot has traded the thing it is for a feature.
//!
//! The format is YUYV 4:2:2 straight off V4L2, so the luminance plane — all OCR needs — is
//! already present in the buffer. No decode, no colour conversion.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use salvo::prelude::*;
use screenocr::{contains, read, Atlas, Gray};
use serde_json::{json, Value};

pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub yuyv: Vec<u8>,
    pub seq: u64,
}

static WANTED: AtomicBool = AtomicBool::new(false);
static SEQ: AtomicU64 = AtomicU64::new(0);

fn slot() -> &'static Mutex<Option<Frame>> {
    static S: OnceLock<Mutex<Option<Frame>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

fn atlas() -> &'static Mutex<Atlas> {
    static A: OnceLock<Mutex<Atlas>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(Atlas::new(0, 0)))
}

/// Ask the capture loop for the next frame. Returns immediately.
pub fn request() {
    WANTED.store(true, Ordering::Relaxed);
}

/// Called from the capture loop for every frame.
///
/// # Safety
/// `addr` must point to at least `len` readable bytes for the duration of this call — which
/// holds for a V4L2 buffer between dequeue and requeue, and nowhere else. The call therefore
/// belongs exactly where it is and must not be moved outside that window.
pub unsafe fn offer(addr: *const u8, len: usize, w: usize, h: usize) {
    // ⛔ The early return that keeps the video path free. One relaxed load per frame.
    if !WANTED.swap(false, Ordering::Relaxed) || addr.is_null() || len == 0 {
        return;
    }
    let yuyv = unsafe { std::slice::from_raw_parts(addr, len) }.to_vec();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    if let Ok(mut g) = slot().lock() {
        *g = Some(Frame { w, h, yuyv, seq });
    }
}

/// The most recent captured frame as grayscale, if one has been taken.
fn latest_gray() -> Option<(Gray, u64)> {
    let g = slot().lock().ok()?;
    let f = g.as_ref()?;
    Gray::from_yuyv(f.w, f.h, &f.yuyv).map(|img| (img, f.seq))
}

/// Request a frame and wait briefly for the capture loop to deliver one.
///
/// ⛔ BOUNDED, AND HONEST WHEN IT FAILS. With no HDMI signal no frame will ever arrive, and
/// the caller must be told that rather than left hanging — "no video" is a real answer about
/// the host and often the most informative one.
async fn grab() -> Option<(Gray, u64)> {
    let before = SEQ.load(Ordering::Relaxed);
    request();
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if let Some((g, seq)) = latest_gray() {
            if seq > before {
                return Some((g, seq));
            }
        }
    }
    None
}

fn no_signal() -> Value {
    json!({ "error": "no frame was captured within 2s",
            "hint": "the host may be off or the HDMI input unplugged; GET /api/screen tells \
                     you whether any frame has ever been seen" })
}

fn region_of(req: &salvo::Request, g: &Gray) -> (usize, usize, usize, usize) {
    let n = |k: &str, d: usize| req.query::<usize>(k).unwrap_or(d);
    (n("x", 0), n("y", 0), n("w", g.w), n("h", g.h))
}

/// `GET /api/screen` — is there a picture, and how big.
#[derive(Clone)]
pub struct Status;

#[async_trait]
impl Handler for Status {
    async fn handle(&self, _q: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    _c: &mut FlowCtrl) {
        match grab().await {
            Some((g, seq)) => res.render(Json(json!({
                "signal": true, "width": g.w, "height": g.h, "sequence": seq,
                "format": "yuyv422", "ocr": { "trained": atlas().lock().map(|a| a.len())
                                                  .unwrap_or(0) },
            }))),
            None => {
                res.status_code(StatusCode::SERVICE_UNAVAILABLE);
                res.render(Json(no_signal()));
            }
        }
    }
}

/// `GET /api/screen/ocr?x=&y=&w=&h=&expect=` — the screen as TEXT.
///
/// `expect` makes this the single call a verified automation step needs: it returns whether
/// the text is present, so the caller does not have to reimplement whitespace and case
/// normalisation and get it subtly different from everyone else.
#[derive(Clone)]
pub struct Ocr;

#[async_trait]
impl Handler for Ocr {
    async fn handle(&self, req: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    _c: &mut FlowCtrl) {
        let Some((g, seq)) = grab().await else {
            res.status_code(StatusCode::SERVICE_UNAVAILABLE);
            res.render(Json(no_signal()));
            return;
        };
        let (x, y, w, h) = region_of(req, &g);
        let min_conf = req.query::<f64>("min_confidence").unwrap_or(0.8);
        let a = atlas().lock().expect("atlas");
        if a.is_empty() {
            res.status_code(StatusCode::PRECONDITION_FAILED);
            res.render(Json(json!({
                "error": "no glyph atlas has been learned for this machine's firmware font",
                "hint": "POST /api/screen/ocr/learn with cell_w, cell_h, a region and the text \
                         that is on it. A template matcher cannot read a font it has never \
                         seen, and guessing would produce a confident wrong answer.",
            })));
            return;
        }
        let reading = read(&g.crop(x, y, w, h).binarize(), &a, min_conf);
        let expect = req.query::<String>("expect");
        res.render(Json(json!({
            "text": reading.text, "confidence": reading.confidence,
            "rows": reading.rows, "cols": reading.cols,
            "unreadable_cells": reading.unreadable_cells,
            "sequence": seq,
            "region": { "x": x, "y": y, "w": w, "h": h },
            "matched": expect.as_deref().map(|e| contains(&reading, e)),
        })));
    }
}

/// `POST /api/screen/ocr/learn` — the one calibration step per machine.
#[derive(Clone)]
pub struct Learn;

#[async_trait]
impl Handler for Learn {
    async fn handle(&self, req: &mut salvo::Request, _d: &mut Depot, res: &mut salvo::Response,
                    _c: &mut FlowCtrl) {
        let body: Value = req.parse_json().await.unwrap_or(Value::Null);
        let cw = body.get("cell_w").and_then(Value::as_u64).unwrap_or(0) as usize;
        let ch = body.get("cell_h").and_then(Value::as_u64).unwrap_or(0) as usize;
        let text = body.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        if cw == 0 || ch == 0 || text.is_empty() {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(json!({ "error": "cell_w, cell_h and text are all required" })));
            return;
        }
        let Some((g, _)) = grab().await else {
            res.status_code(StatusCode::SERVICE_UNAVAILABLE);
            res.render(Json(no_signal()));
            return;
        };
        let n = |k: &str, d: usize| body.get(k).and_then(Value::as_u64).map_or(d, |v| v as usize);
        let region = g.crop(n("x", 0), n("y", 0), n("w", g.w), n("h", g.h)).binarize();

        let mut a = atlas().lock().expect("atlas");
        if a.cell_w != cw || a.cell_h != ch {
            *a = Atlas::new(cw, ch);
        }
        let learned = a.learn(&region, &text);
        res.render(Json(json!({ "learned_cells": learned, "glyphs_known": a.len(),
                                "cell_w": cw, "cell_h": ch })));
    }
}

//! Every routed handler must also be registered for OpenAPI.
//!
//! ⛔ THIS EXISTS BECAUSE A COMMENT WAS NOT ENOUGH.
//!
//! `capabilities()` once reported `"ocr": false` while OCR was routed and serving, because the
//! flag was hand-maintained and nobody revisited it — its own test asserted the stale answer.
//! The fix for that introduced `arkkvm/src/apidoc.rs`: another hand-maintained list, carrying
//! a comment politely asking the next person to keep it in sync. The same smell, one layer
//! along. So check it instead of asking.
//!
//! This reads the two source files rather than the built router, because `arkkvm`'s own tests
//! cannot run on a build host — they would link the ARM vendor libraries.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn arkkvm_src(file: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../arkkvm/src").join(file);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Collect `ns::Type` handler paths. Four character classes do not justify a regex crate.
fn handler_types(text: &str) -> BTreeSet<String> {
    const NS: [&str; 5] =
        ["crate::leases::", "crate::screen::", "crate::vmedia::", "redfish::", "httprpc::"];
    let mut out = BTreeSet::new();
    for ns in NS {
        let mut from = 0;
        while let Some(i) = text[from..].find(ns) {
            let after = from + i + ns.len();
            let end = text[after..]
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .map_or(text.len(), |k| after + k);
            if text[after..end].starts_with(|c: char| c.is_ascii_uppercase()) {
                out.insert(format!("{ns}{}", &text[after..end]));
            }
            from = end.max(from + i + 1);
        }
    }
    out
}

#[test]
fn every_routed_handler_is_registered_for_openapi() {
    let routed: BTreeSet<String> = handler_types(&arkkvm_src("web.rs"))
        .into_iter()
        // A hoop is middleware, not a route, so it has no operation to document.
        .filter(|h| !h.ends_with("LeaseGuard"))
        .collect();
    let documented = handler_types(&arkkvm_src("apidoc.rs"));

    assert!(!routed.is_empty(), "no handlers found in web.rs; the extractor is broken");
    assert!(!documented.is_empty(), "no handlers found in apidoc.rs; the extractor is broken");

    let missing: Vec<_> = routed.difference(&documented).collect();
    assert!(missing.is_empty(), "routed but undocumented: {missing:?}");

    let stale: Vec<_> = documented.difference(&routed).collect();
    assert!(stale.is_empty(), "documented but no longer routed: {stale:?}");
}

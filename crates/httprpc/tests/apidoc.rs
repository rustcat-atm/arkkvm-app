//! Proof that a plain `Handler` struct can appear in the generated OpenAPI document.
//!
//! `OpenApi::merge_router` only reports operations registered against a handler's `TypeId`,
//! which `#[endpoint]` does for annotated functions. Every control-plane handler is a struct,
//! so the published document omitted all of them. `arkkvm/src/apidoc.rs` registers them
//! directly instead of rewriting nineteen handlers — and this is where that technique is
//! checked, because the firmware crate's tests cannot run on a build host.

use salvo::oapi::__private::inventory;
use salvo::oapi::{Components, Endpoint, EndpointRegistry, OpenApi, Operation};
use salvo::prelude::*;

#[derive(Clone)]
struct PlainHandler;

#[async_trait]
impl Handler for PlainHandler {
    async fn handle(&self, _q: &mut Request, _d: &mut Depot, res: &mut Response,
                    _c: &mut FlowCtrl) {
        res.render("ok");
    }
}

inventory::submit! {
    EndpointRegistry::save(
        || std::any::TypeId::of::<PlainHandler>(),
        || Endpoint::new(
            Operation::new().summary("a struct handler, documented").add_tag("proof"),
            Components::new(),
        ),
    )
}

#[test]
fn a_struct_handler_registered_by_type_id_reaches_the_generated_document() {
    let router = Router::with_path("/proof/path").get(PlainHandler);
    let doc = OpenApi::new("t", "0").merge_router(&router);
    let json = serde_json::to_string(&doc).expect("serialise the document");

    assert!(json.contains("/proof/path"),
            "a struct handler stayed invisible to merge_router; registration does not work:\n{json}");
    assert!(json.contains("a struct handler, documented"), "path present but summary lost");

    // Negative control: without registration there is nothing to find, so the assertion above
    // is meaningful rather than matching something merge_router emits for every route.
    let bare = OpenApi::new("t", "0").merge_router(&Router::with_path("/unregistered").get(Undocumented));
    let bare_json = serde_json::to_string(&bare).expect("serialise");
    assert!(!bare_json.contains("/unregistered"),
            "merge_router documents unregistered handlers too, so this test proves nothing");
}

#[derive(Clone)]
struct Undocumented;

#[async_trait]
impl Handler for Undocumented {
    async fn handle(&self, _q: &mut Request, _d: &mut Depot, res: &mut Response,
                    _c: &mut FlowCtrl) {
        res.render("ok");
    }
}

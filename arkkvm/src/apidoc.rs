//! OpenAPI entries for the handlers `merge_router` cannot see.
//!
//! `OpenApi::merge_router` collects operations that the `#[endpoint]` macro registered, keyed
//! by the handler's `TypeId`. Every handler added for the control plane is a `Handler` struct
//! rather than an annotated function — because several carry constructor state — so the
//! generated document listed none of them. The published API described the device's original
//! routes and silently omitted the lease, the screen, Redfish and virtual media.
//!
//! Rewriting nineteen handlers as annotated functions to fix a documentation gap would be a
//! large change to working code. `#[endpoint]` is a thin wrapper over `EndpointRegistry`, so
//! registering the same metadata directly is the small one. The mechanism is proved by a test
//! in `crates/httprpc` (this crate's tests cannot run on a build host).
//!
//! ⛔ THIS LIST MIRRORS THE ROUTE TABLE IN `web.rs`. A route added there and not here is
//! undocumented, which is the defect this file exists to close.

use salvo::oapi::__private::inventory;
use salvo::oapi::{Components, Endpoint, EndpointRegistry, Operation};

macro_rules! document {
    ($ty:ty, $tag:literal, $summary:literal) => {
        inventory::submit! {
            EndpointRegistry::save(
                || std::any::TypeId::of::<$ty>(),
                || Endpoint::new(
                    Operation::new().summary($summary).add_tag($tag),
                    Components::new(),
                ),
            )
        }
    };
}

document!(httprpc::HttpRpc, "control", "JSON-RPC over HTTP; same registry as the data channel");
document!(httprpc::Capabilities, "control", "What this device can actually do");

document!(crate::leases::Acquire, "lease", "Take the control lease, or be told who holds it");
document!(crate::leases::Renew, "lease", "Extend the lease you hold");
document!(crate::leases::Release, "lease", "Give the lease back");
document!(crate::leases::Status, "lease", "Who holds the device, and the recent history");

document!(crate::screen::Status, "screen", "Whether a frame is available, and its shape");
document!(crate::screen::Ocr, "screen", "Read the host's screen as text, on-device");
document!(crate::screen::Learn, "screen", "Teach the glyph atlas a font");

document!(redfish::ServiceRoot, "redfish", "Redfish service root");
document!(redfish::SystemsCollection, "redfish", "Computer system collection");
document!(redfish::System, "redfish", "The host: power state and identity");
document!(redfish::Reset, "redfish", "ComputerSystem.Reset — presses a physical button");
document!(redfish::ManagersCollection, "redfish", "Manager collection");
document!(redfish::Manager, "redfish", "This KVM");
document!(redfish::VirtualMediaCollection, "redfish", "Virtual media collection");

document!(crate::vmedia::Resource, "redfish", "What image is mounted, and whether it is writable");
document!(crate::vmedia::Insert, "redfish", "VirtualMedia.InsertMedia — mount an image by URL");
document!(crate::vmedia::Eject, "redfish", "VirtualMedia.EjectMedia — unmount it");

# An HTTP control plane

## Why

ArkKVM's control plane rides JSON-RPC over a WebRTC data channel. That is the right transport
for an interactive session and the wrong one for a single command: it forces every consumer —
a monitoring loop, a provisioning script, a CI job, an orchestrator — to carry a full WebRTC
stack, and for screenshots an H.264 decoder, before it can ask whether the host is powered on.

The asymmetry is the point. **Keep WebRTC for the stream. A one-shot command should be one
request.**

Measured on a live device (`v0.5.260914221`): there is no HTTP control path today. Probing
`/api/rpc`, `/api/capabilities` and `/redfish/v1` all return `200`, but that is the SPA
catch-all — a deliberately nonsensical path returns the same `index.html`. The only real
non-UI endpoints are `/webrtc/signaling/client` (401) and `/metrics`, which answers
`# ArkKVM metrics not implemented`.

Meanwhile the device already implements a rich API: 114 RPC methods including
`setATXPowerAction`, `sendKeys`, `getSnapshot`, `mountFile`, `getVirtualMediaList` and
`sendWOLMagicPacket`. All of it is reachable only through the data channel.

## What this adds

| route | method | purpose |
|---|---|---|
| `/api/rpc` | POST | JSON-RPC 2.0 — single requests, notifications and batches |
| `/api/capabilities` | GET | what this firmware can do, **derived from the registry** |

Both sit behind the same `auth_middleware` + `csrf_middleware` hoops as every other protected
route. No new authentication path, no new privilege.

⛔ **This is not a second implementation of the API.** It dispatches into the *same*
`RpcRegistry` the data channel uses. A method added to the registry is immediately reachable
over both transports, and the two can never disagree about what a method does. A parallel HTTP
implementation would drift the first time somebody fixed a bug in only one of them.

## Protocol decisions

* **Notifications get `204`, never a body.** A request without an `id` must not be answered.
  The data-channel path already honours this; HTTP now does too, and an all-notification batch
  likewise returns `204`.
* **HTTP `200` for any well-formed JSON-RPC exchange, even one carrying an error.** Transport
  failure and application failure are different things, and collapsing them is how a client
  ends up retrying a refusal.
* **Standard error codes** — `-32700` / `-32600` / `-32601` / `-32603` — so a client that
  already speaks JSON-RPC needs no special cases.
* **A wrong `jsonrpc` version is refused, not guessed at.** Leniency means a client with a typo
  gets a different method dispatched than it meant.
* **Capabilities are derived, never hand-maintained.** A capability table written by hand is
  wrong the moment a method is added, and the client that trusts it calls something absent.

## Layout, and why

```
crates/httprpc/          all protocol logic — compiles and tests on a plain x86 host
arkkvm/src/httprpc_adapter.rs   ~40 lines: RpcRegistry -> httprpc::RpcDispatch
arkkvm/src/web.rs        two route registrations
arkkvm/src/jsonrpc.rs    one accessor: PROCESSOR.registry()
```

The split is deliberate, and it is a response to a real obstacle. **The firmware cannot be
built, or even typechecked, without artifacts this repository does not contain:** a cross
toolchain and proprietary Rockchip sysroot from `arkkvm-system`, prebuilt Opus, the frontend
dist, and a **stage2 Rust compiler built from Rust source tag 1.94.1**. `cargo check` fails in
`build.rs` before compiling a line of Rust.

Putting the protocol in its own crate means the part with the behaviour is verifiable by anyone
with rustup and nothing else.

## Verification status

**Verified on this host** — `cargo test -p httprpc`: 11 tests covering notifications, batches,
all-notification batches, parse errors, protocol-version refusal, method-not-found, handler
errors, and capability derivation in both directions. `cargo clippy -p httprpc`: clean.

⛔ **NOT verified** — the three firmware-side files. They parse (`rustfmt`), but they have never
been typechecked or run, because this machine cannot build the crate they live in. They have
also never run on a device. Treat the wiring as a reviewed patch, not as tested code.

## A build-script change came with it

`arkkvm/build.rs` previously required `BUILDKIT_ROOT` unconditionally, so `cargo check` on a
developer machine died before compiling any Rust — you needed the full Rockchip cross toolchain
to typecheck a one-line change. The `getauxval` shim exists because uClibc lacks that symbol;
glibc already has it, so a host build does not need the shim at all.

It is now conditional: a cross build is byte-identical, and a host build skips the shim with a
warning. `crates/rockchip_mpi_sys` still needs the vendor sysroot for bindgen, so the `arkkvm`
crate itself remains unbuildable here — but the barrier moved by one crate, and the new code
sits on the right side of it.

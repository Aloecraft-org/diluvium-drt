//! The browser tier: DRT's swarm driven over a JS-hosted diluvium instance.
//!
//! `doc/Browser.md` is the contract this implements; read it first. The
//! short version: two wasm modules cannot call each other in a browser, so
//! JS is the host in the middle. DRT's swarm — the whole `dvs.c` port —
//! runs here in wasm, and reaches the interpreter by calling back out.
//!
//! The crate is built so that almost none of it is browser-only:
//! [`bridge::HostBridge`] is the JS contract expressed as a Rust trait, and
//! the engine and the host are written against it. A mock bridge in the
//! tests drives a real `Swarm`, so this gets ordinary `cargo test`
//! coverage rather than only being exercisable in a browser.
//!
//! **What is not here yet (task #31), so nobody reads this crate as
//! finished:** there is no `wasm_bindgen` in it at all. That means no
//! exports — JS cannot call in — and no glue binding a real JS object to
//! `HostBridge` — the wasm cannot call out. Both directions are described
//! and neither is wired, so the crate compiles to a `.wasm` that exports
//! only `memory`. There is also no connector/pump layer: native DRT routes
//! guest hostcalls through `PumpHost` and a `Dispatcher`, and the browser
//! tier's equivalent — pumping the queues the bridge already exposes out
//! to JS-side connectors — is the third piece of #31.

pub mod bridge;
pub mod engine;
pub mod host;

pub use bridge::{Driven, HostBridge, InstanceHandle};
pub use engine::{BrowserEngine, BrowserInstance};
pub use host::JsHost;

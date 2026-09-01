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
//! **Both directions are wired now.** [`exports`] is `doc/Browser.md`'s
//! table as wasm-bindgen classes, so JS can call in; [`js_bridge`] binds a
//! real JS object to [`bridge::HostBridge`], so the wasm can call out. Both
//! are wasm32-only and both are thin: the logic they sit on is exercised
//! natively against the mock bridge.
//!
//! **What is still missing (the third piece of task #31):** the
//! connector/pump layer. Native DRT routes guest hostcalls through
//! `PumpHost` and a `Dispatcher`; the browser tier's equivalent — pumping
//! the queues the bridge already exposes out to JS-side connectors — does
//! not exist. So a program can run, park and be driven in a page, but it
//! cannot reach `host.fs` or `host.time`. `doc/HostBaseline.md` says what a
//! browser host owes when that lands.

pub mod bridge;
pub mod engine;
pub mod host;

/// The JS-facing halves. wasm32 only: `js_bridge` binds a real JS object to
/// [`bridge::HostBridge`] (the wasm calling out), and `exports` is
/// `doc/Browser.md`'s table (JS calling in).
#[cfg(target_arch = "wasm32")]
pub mod exports;
#[cfg(target_arch = "wasm32")]
pub mod js_bridge;

pub use bridge::{Driven, HostBridge, InstanceHandle};
pub use engine::{BrowserEngine, BrowserInstance};
pub use host::JsHost;

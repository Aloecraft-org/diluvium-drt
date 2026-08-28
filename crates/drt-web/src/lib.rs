//! The browser tier: DRT's swarm driven over a JS-hosted diluvium instance.
//!
//! `doc/Browser.md` is the contract this implements; read it first. The
//! short version: two wasm modules cannot call each other in a browser, so
//! JS is the host in the middle. DRT's swarm — the whole `dvs.c` port —
//! runs here in wasm, and reaches the interpreter by calling back out.
//!
//! The crate is built so that almost none of it is browser-only:
//! [`bridge::HostBridge`] is the JS contract expressed as a Rust trait, and
//! the engine, the host and the exports are written against it. A mock
//! bridge in the tests drives a real `Swarm`, so this gets ordinary
//! `cargo test` coverage rather than only being exercisable in a browser.

pub mod bridge;
pub mod engine;
pub mod host;

pub use bridge::{Driven, HostBridge, InstanceHandle};
pub use engine::{BrowserEngine, BrowserInstance};
pub use host::JsHost;

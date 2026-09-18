//! The plugin channel: new host functionality without a new binary.
//!
//! # Surface
//!
//! Entry points:
//! - [`manifest`] — what a plugin declares about itself, read before
//!   anything starts: its family, its transport, its scope, its limits.
//! - [`frame`] — the wire: length-prefixed msgpack, request and reply.
//! - [`channel`] — the byte stream under it, and its test double.
//! - [`session`] — many calls over one stream, polled and never blocking.
//! - [`process`] — the unix transport: fork, exec, and keep fd 3. A
//!   platform-bound module, `cfg(unix)`.
//! - [`tcp`] — the dialed transport: `process` minus the fork
//!   (`doc/Plan-0.7.0.md` §8). Native and wasi, where `std::net` is.
//! - [`spawn`] — the native default (`doc/Plugins.md` §4.1): DRT starts
//!   the plugin and the plugin dials back, so DRT owns the lifetime the
//!   way `process` does and inherits nothing, the way Windows requires.
//!
//! Configurable values: each module's own, named in its surface block.
//!
//! Fan-out: the transports — `socketpair` (`process`) and `tcp` here,
//! `spawn` natively, a Worker or a WebSocket in a page. They
//! differ only in how the byte stream is *obtained*; the frames on it are
//! identical, which is the claim that makes one plugin run everywhere.
//!
//! # What this crate is
//!
//! `doc/Plugins.md` §1: the plugin channel is a connector *backing* behind
//! the existing `Connector` trait, not a second
//! protocol. A plugin is a subprocess speaking msgpack frames, and every
//! guarantee a built-in connector carries — capability gating, token echo,
//! the answered-always rule, replay — belongs to the dispatcher and is
//! inherited rather than reimplemented. A guest cannot tell the two apart,
//! and that is the acceptance test, not an aspiration.
//!
//! **Target-neutral on purpose.** No process spawning, no sockets and no
//! `tokio` in this crate's root: platform code lives in leaf `Channel`
//! impls so the same frames and the same state machine run in a page,
//! where there are no threads at all.

pub mod channel;
pub mod frame;
pub mod manifest;
#[cfg(unix)]
pub mod process;
pub mod session;
/// DRT starts the plugin; the plugin dials back over loopback.
///
/// Native only, both halves of it: starting a process needs
/// `drt_platform::process::Tree`, which neither wasm target has.
#[cfg(any(unix, windows))]
pub mod spawn;
#[cfg(any(unix, windows, target_os = "wasi"))]
pub mod tcp;

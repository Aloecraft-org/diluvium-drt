//! The plugin channel: new host functionality without a new binary.
//!
//! # Surface
//!
//! Entry points:
//! - [`frame`] — the wire: length-prefixed msgpack, request and reply.
//!
//! Configurable values: each module's own, named in its surface block.
//!
//! Fan-out: the transports, once `channel` lands — `socketpair` and
//! `spawn` and `tcp` natively, a Worker or a WebSocket in a page. They
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

pub mod frame;

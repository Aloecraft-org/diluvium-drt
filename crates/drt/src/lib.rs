//! The DRT runtime, as a library. The `drt` binary is a thin CLI over
//! this; keeping the flow here is what lets it be tested end to end.

/// The command surface, parsed and assembled once for every host.
pub mod cli;
pub mod config;
/// The drive loop as a state machine: what `run`, `repl` and the browser
/// tier drive an instance with (doc/Wasm.md D6).
pub mod drive;
#[cfg(feature = "listen")]
pub mod listen;
/// `drt netcheck`: the NAT diagnostic. The verdict table is pure and
/// always compiled; the measurements that need STUN are behind `stun`.
pub mod netcheck;
/// The reflect fetch `drt netcheck --reflect` uses. Behind `netcheck`
/// because it is the half that links a TLS stack.
#[cfg(feature = "netcheck")]
pub mod reflect;
#[cfg(feature = "relay")]
pub mod relay;
pub mod repl;
pub mod run;
pub mod start;
#[cfg(feature = "stun")]
pub mod stun;
#[cfg(feature = "tunnel")]
pub mod tunnel;

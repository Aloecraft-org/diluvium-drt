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
/// PEM trust anchors named with `--extra-root`, shared by every verb that
/// dials TLS from a flag. Behind either feature that has one, because both
/// carry the TLS stack it needs.
#[cfg(any(feature = "tunnel", feature = "netcheck"))]
pub mod roots;
pub mod run;
pub mod runtime;
pub mod start;
#[cfg(feature = "stun")]
pub mod stun;
#[cfg(feature = "tunnel")]
pub mod tunnel;
#[cfg(feature = "turn")]
pub mod turn;
#[cfg(feature = "wireguard")]
pub mod wireguard;

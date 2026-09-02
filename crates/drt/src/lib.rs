//! The DRT runtime, as a library. The `drt` binary is a thin CLI over
//! this; keeping the flow here is what lets it be tested end to end.

pub mod config;
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

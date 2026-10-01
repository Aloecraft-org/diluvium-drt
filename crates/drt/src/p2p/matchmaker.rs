//! `--match` (`doc/P2P.md` §2.4): the reference signalling server. Not
//! built yet; the stdlib program that grows from `examples/30-signaling-room`
//! lands with it.
//!
//! ## surface block
//!
//! - Entry points: [`run`].
//! - Configurable: none yet.
//! - Fan-out: none yet.

use drt_config::RootConfig;

use super::MatchRole;

pub fn run(_role: &MatchRole, _config: &RootConfig) -> Result<(), String> {
    Err("--match is not built yet; examples/30-signaling-room is the server until it is".into())
}

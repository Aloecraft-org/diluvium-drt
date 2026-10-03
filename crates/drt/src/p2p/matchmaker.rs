//! `--match` (`doc/P2P.md` §2.4): the reference signalling server, as a
//! deployment this verb assembles from flags. The program is
//! `stdlib/p2p_match.dlua`, grown from `examples/30-signaling-room`; the
//! listener is the `http` listener with streaming on, for the call
//! notification stream. A TLS terminator goes in front for pages on
//! `https://` origins; certificate flags can come later.
//!
//! ## surface block
//!
//! - Entry points: [`run`].
//! - Configurable: [`CONN_DEADLINE_MS`], past the program's hold time so a
//!   call nobody answers gets the program's 504; [`MAX_BODY`] (§6);
//!   [`HEADERS`], what the program reads; [`RESP_HEADERS`], what it sets.
//! - Fan-out: none; one listener, one program.

use std::collections::BTreeMap;

use drt_config::resolve::ArgValue;
use drt_config::{Listener, Program, RootConfig};
use drt_connector::Dispatcher;

use super::MatchRole;

/// The listener's deadline: the program holds a call for 25 s.
pub const CONN_DEADLINE_MS: u64 = 30_000;
/// A request body: a record is 512 bytes, the profile allows 1 KiB.
pub const MAX_BODY: usize = 1024;
/// The request headers the program is handed.
pub const HEADERS: &[&str] = &[
    "authorization",
    "last-event-id",
    "drt-accept",
    "drt-caller-token",
];
/// The response headers the program may set.
pub const RESP_HEADERS: &[&str] = &[
    "access-control-allow-origin",
    "access-control-allow-methods",
    "access-control-allow-headers",
    "access-control-expose-headers",
    "cache-control",
    "location",
    "retry-after",
];

/// The server, foreground forever, as `drt start` on the equivalent
/// config would be. The config given is the base: its connectors and
/// principals stand, and the listener and program are this verb's.
pub fn run(role: &MatchRole, config: &RootConfig) -> Result<(), String> {
    let address = std::net::SocketAddr::new(role.host.bind_ip(), role.port);
    let mut config = config.clone();
    let Some(crate::stdlib::Kind::Source(source)) = crate::stdlib::lookup(crate::stdlib::P2P_MATCH)
    else {
        return Err("this build carries no `stdlib:p2p-match`".into());
    };
    config.root.program = Some(Program::Source(source.into()));
    config.entry = None;
    config.args = BTreeMap::from([(
        "capacity".to_string(),
        ArgValue::Int(i64::try_from(role.capacity).unwrap_or(i64::MAX)),
    )]);
    config.connectors.entry("time".to_string()).or_default();
    config.listeners = vec![Listener {
        scheme: "http".into(),
        address: address.to_string(),
        conn_deadline_ms: CONN_DEADLINE_MS,
        max_body: MAX_BODY,
        streaming: true,
        headers: HEADERS.iter().map(|h| h.to_string()).collect(),
        resp_headers: RESP_HEADERS.iter().map(|h| h.to_string()).collect(),
        ..listener_defaults()
    }];
    if !role.host.accept().is_empty() {
        eprintln!(
            "drt p2p: --host {}: the signalling port is bound inside the range; callers from \
             outside it reach nothing",
            role.host.accept()[0]
        );
    }
    let dispatcher = Dispatcher::new(crate::cli::wire_connectors(&config)?);
    crate::start::start(&config, dispatcher)
}

/// A listener with every default, to fill the fields above in.
fn listener_defaults() -> Listener {
    serde_json::from_str(r#"{"scheme":"http","address":"127.0.0.1:0"}"#)
        .expect("the two required fields make a listener")
}

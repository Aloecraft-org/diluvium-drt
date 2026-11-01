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
//!   call nobody answers gets the program's 504; [`MAX_BODY`] (§6), and
//!   [`KV_MAX_BODY`] when `--kv` is on (§10); [`HEADERS`], what the program
//!   reads; [`RESP_HEADERS`], what it sets; [`KV_DB`], the database's name
//!   inside the `--kv` directory.
//! - Fan-out: none; one listener, one program. `--kv` adds the `sql`
//!   connector, scoped to its directory, and the grants the program needs.

use std::collections::BTreeMap;

use drt_config::resolve::ArgValue;
use drt_config::{Listener, Program, RootConfig};
use drt_connector::Dispatcher;

use super::MatchRole;

/// The listener's deadline: the program holds a call for 25 s.
pub const CONN_DEADLINE_MS: u64 = 30_000;
/// A request body: a record is 512 bytes, the profile allows 1 KiB.
pub const MAX_BODY: usize = 1024;
/// A request body with `--kv` on: a value is at most 16 KiB (the program's
/// `KV_VALUE_MAX`), so a body past that is refused by the program by name.
pub const KV_MAX_BODY: usize = 17 * 1024;
/// The database `--kv <dir>` keeps, inside that directory.
pub const KV_DB: &str = "kv.db";
/// The request headers the program is handed.
pub const HEADERS: &[&str] = &[
    "authorization",
    "last-event-id",
    "drt-accept",
    "drt-caller-token",
    "if-match",
    "if-none-match",
];
/// The response headers the program may set.
pub const RESP_HEADERS: &[&str] = &[
    "access-control-allow-origin",
    "access-control-allow-methods",
    "access-control-allow-headers",
    "access-control-expose-headers",
    "cache-control",
    "etag",
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
    if let Some(dir) = &role.kv {
        wire_kv(&mut config, dir)?;
    }
    config.listeners = vec![Listener {
        scheme: "http".into(),
        address: address.to_string(),
        conn_deadline_ms: CONN_DEADLINE_MS,
        max_body: if role.kv.is_some() {
            KV_MAX_BODY
        } else {
            MAX_BODY
        },
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

/// `--kv <dir>`: the directory made if absent, the `sql` connector scoped
/// to it with write access and WAL, the program told the database's name,
/// and the grants for `sql` and the wall clock (a TTL outlives a restart,
/// so it is kept in wall time, not monotonic).
fn wire_kv(config: &mut RootConfig, dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("--kv {}: {e}", dir.display()))?;
    let scope = serde_json::json!({
        "scope": dir,
        "access": "readwrite",
        "create": true,
        "journal_mode": "wal",
    });
    let scope: drt_caps::Scope = serde_json::from_value(scope).map_err(|e| format!("--kv: {e}"))?;
    config.connectors.insert(
        "sql".to_string(),
        drt_config::ConnectorWiring {
            backing: None,
            scope: Some(scope),
        },
    );
    config
        .args
        .insert("kv".to_string(), ArgValue::Str(KV_DB.to_string()));
    for cap in ["host:sql/*", "host:time", "host:time/*"] {
        if !config.root.caps.iter().any(|g| g.capability == cap) {
            config.root.caps.push(drt_caps::Grant::grant(cap));
        }
    }
    Ok(())
}

/// A listener with every default, to fill the fields above in.
fn listener_defaults() -> Listener {
    serde_json::from_str(r#"{"scheme":"http","address":"127.0.0.1:0"}"#)
        .expect("the two required fields make a listener")
}

//! `--match` (`doc/P2P.md` §2.4): the reference signalling server, as a
//! deployment this verb assembles from flags. The program is
//! `stdlib/p2p_match.dlua`, grown from `examples/30-signaling-room`; the
//! listener is the `http` listener with streaming on, for the call
//! notification stream. A TLS terminator goes in front for pages on
//! `https://` origins; certificate flags can come later.
//!
//! ## surface block
//!
//! - Entry points: [`run`]; [`load_topics`], the `--topics` file read and
//!   checked into what the program is handed.
//! - Configurable: [`CONN_DEADLINE_MS`], past the program's hold time so a
//!   call nobody answers gets the program's 504; [`MAX_BODY`] (§6), and
//!   [`KV_MAX_BODY`] when `--kv` is on (§10); [`HEADERS`], what the program
//!   reads; [`RESP_HEADERS`], what it sets; [`ROLES`], what a topic token
//!   may hold; [`NAME_MAX`]; [`KV_DB`], the database's name inside the
//!   `--kv` directory.
//! - Fan-out: none; one listener, one program. `--kv` adds the `sql`
//!   connector, scoped to its directory, and the grants the program needs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

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

/// The roles a topic token may hold (`doc/P2P.md` §2.4): `park` answers
/// for names in the topic, `call` calls them, `read` lists them and reads
/// the topic's keys, `write` writes its keys.
pub const ROLES: &[&str] = &["park", "call", "read", "write"];
/// A topic's longest name, as a parked name's (`doc/DRT-Signalling.md` §1).
pub const NAME_MAX: usize = 64;

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
    let topics = match &role.topics {
        Some(path) => load_topics(path)?,
        None => String::new(),
    };
    config.args = BTreeMap::from([
        (
            "capacity".to_string(),
            ArgValue::Int(i64::try_from(role.capacity).unwrap_or(i64::MAX)),
        ),
        ("topics".to_string(), ArgValue::Str(topics)),
    ]);
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

// depth: the topics file

/// The `--topics` file: `{"topics": {"<topic>": {"tokens": [{"token": "…",
/// "roles": ["park", "call", "read", "write"]}]}}}`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TopicsFile {
    topics: BTreeMap<String, TopicEntry>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TopicEntry {
    tokens: Vec<TokenEntry>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenEntry {
    token: String,
    roles: Vec<String>,
}

/// Read and check the `--topics` file, and return it as the program takes
/// it: JSON text, topic -> token -> roles, a token listed twice in a topic
/// holding the union of its roles. Every refusal names the file and the
/// entry.
pub fn load_topics(path: &Path) -> Result<String, String> {
    let shown = path.display();
    let text = std::fs::read_to_string(path).map_err(|e| format!("--topics {shown}: {e}"))?;
    topics_from_text(&text).map_err(|e| format!("--topics {shown}: {e}"))
}

fn topics_from_text(text: &str) -> Result<String, String> {
    let file: TopicsFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut out: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    for (topic, entry) in file.topics {
        if !is_name(&topic) {
            return Err(format!(
                "topic '{topic}': a topic is 1 to {NAME_MAX} of a-z, 0-9, - and ."
            ));
        }
        let tokens = out.entry(topic.clone()).or_default();
        for (i, t) in entry.tokens.into_iter().enumerate() {
            // A token rides in `Authorization: Bearer` or `?k=`, unescaped.
            if t.token.is_empty()
                || !t
                    .token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-._~+/=".contains(c))
            {
                return Err(format!(
                    "topic '{topic}', token {}: a token is one or more of A-Z, a-z, 0-9 and -._~+/=",
                    i + 1
                ));
            }
            if t.roles.is_empty() {
                return Err(format!(
                    "topic '{topic}', token {}: no roles; give one or more of {}",
                    i + 1,
                    ROLES.join(", ")
                ));
            }
            let roles = tokens.entry(t.token).or_default();
            for role in t.roles {
                if !ROLES.contains(&role.as_str()) {
                    return Err(format!(
                        "topic '{topic}', token {}: '{role}' is not a role; the roles are {}",
                        i + 1,
                        ROLES.join(", ")
                    ));
                }
                roles.insert(role);
            }
        }
    }
    Ok(serde_json::to_string(&out).expect("strings encode"))
}

/// A topic: the charset of a parked name.
fn is_name(s: &str) -> bool {
    (1..=NAME_MAX).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_topics_file_becomes_topic_token_roles_with_duplicates_merged() {
        let text = r#"{"topics": {"home": {"tokens": [
            {"token": "a", "roles": ["park"]},
            {"token": "a", "roles": ["call", "park"]},
            {"token": "r", "roles": ["read"]}
        ]}, "calls": {"tokens": []}}}"#;
        assert_eq!(
            topics_from_text(text).unwrap(),
            r#"{"calls":{},"home":{"a":["call","park"],"r":["read"]}}"#
        );
    }

    #[test]
    fn a_topics_file_refuses_what_the_server_could_not_honour() {
        for (text, why) in [
            (r#"{"topics":{"Home":{"tokens":[]}}}"#, "topic 'Home'"),
            (
                r#"{"topics":{"h":{"tokens":[{"token":"a b","roles":["park"]}]}}}"#,
                "token 1",
            ),
            (
                r#"{"topics":{"h":{"tokens":[{"token":"a","roles":[]}]}}}"#,
                "no roles",
            ),
            (
                r#"{"topics":{"h":{"tokens":[{"token":"a","roles":["admin"]}]}}}"#,
                "'admin' is not a role",
            ),
            (
                r#"{"topics":{"h":{"tokens":[],"extra":1}}}"#,
                "unknown field",
            ),
        ] {
            let e = topics_from_text(text).unwrap_err();
            assert!(e.contains(why), "{text}: {e}");
        }
    }
}

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
//!   checked into what the program is handed ([`Topics`]), at start and
//!   again by [`TopicsWatch`] whenever the file changes;
//!   [`StatusToken::parse`], the `--status-token-file` token checked. With
//!   one, the program answers `GET /v1/status` (`doc/DRT-Signalling.md`
//!   §11) to a request bearing it.
//! - Configurable: [`CONN_DEADLINE_MS`], past the program's hold time so a
//!   call nobody answers gets the program's 504; [`MAX_BODY`] (§6), and
//!   [`KV_MAX_BODY`] when `--kv` is on (§10); [`HEADERS`], what the program
//!   reads; [`RESP_HEADERS`], what it sets; [`KV_DB`], the database's name
//!   inside the `--kv` directory; [`ROLES`], what a topic token may hold;
//!   [`NAME_MAX`]; [`GROUPS_CLAIM`], the claim a JWT's groups are read
//!   from unless the file names another; [`JWKS_MAX_KEYS`];
//!   [`STATUS_TOKEN_MIN`], the shortest `--status-token-file` taken;
//!   [`RELOAD_CHECK`], how often the `--topics` file is read for a change.
//! - Fan-out: none; one listener, one program. `--kv` adds the `sql`
//!   connector, scoped to its directory, and the grants the program needs.
//!   `--topics` adds the `crypto` connector, wired `verify_only` unless the
//!   config wires one, and `host:crypto/jwt_verify`, so a `jwt` block the
//!   file gains while the server runs has what it needs.
//!
//! Reload: the program's arguments are the one way its settings arrive
//! (`args`), so a changed topics file is the same table handed again on
//! the same queue, with the file's topics, groups, jwt block and
//! `capacity` in place of the old. A file that no longer checks is
//! refused by name and the settings in force stay.

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
/// The claim a JWT carries its directory groups in: what `ldap/bind`
/// answers as `groups`, signed by the session framework.
pub const GROUPS_CLAIM: &str = "groups";
/// An inline JWKS: the crypto connector's own limit on a key set.
pub const JWKS_MAX_KEYS: usize = 64;
/// The shortest status token: a server reachable from anywhere answers
/// whoever holds it with every name it holds.
pub const STATUS_TOKEN_MIN: usize = 16;
/// How often the `--topics` file is read again for a change. It is small
/// and local, so reading it is cheaper than trusting a modification time
/// that a quick rewrite leaves unchanged.
pub const RELOAD_CHECK: std::time::Duration = std::time::Duration::from_secs(2);

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
    let (topics, topics_text) = match &role.topics {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("--topics {}: {e}", path.display()))?;
            (checked_topics(path, &text, role.kv.is_some())?, Some(text))
        }
        None => (Topics::default(), None),
    };
    config.args = BTreeMap::from([
        (
            "status_token".to_string(),
            ArgValue::Str(
                role.status_token
                    .as_ref()
                    .map(|t| t.0.clone())
                    .unwrap_or_default(),
            ),
        ),
        (
            "version".to_string(),
            ArgValue::Str(env!("CARGO_PKG_VERSION").to_string()),
        ),
    ]);
    put_topics(&mut config.args, &topics, role.capacity);
    config.connectors.entry("time".to_string()).or_default();
    if let Some(dir) = &role.kv {
        wire_kv(&mut config, dir)?;
    }
    if role.topics.is_some() {
        wire_jwt_verify(&mut config)?;
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
    match &role.topics {
        Some(path) => {
            let mut watch = TopicsWatch {
                path: path.clone(),
                args: config.args.clone(),
                capacity: role.capacity,
                has_kv: role.kv.is_some(),
                seen: topics_text,
                next: std::time::Instant::now() + RELOAD_CHECK,
                pending: None,
            };
            crate::start::start_observed(&config, dispatcher, |sw, root| watch.pass(sw, root))
        }
        None => crate::start::start(&config, dispatcher),
    }
}

/// The parts of the program's arguments the topics file decides; the
/// file's `capacity`, when it names one, in place of `--capacity`.
fn put_topics(args: &mut BTreeMap<String, ArgValue>, topics: &Topics, capacity: usize) {
    let capacity = topics.capacity.unwrap_or(capacity);
    args.insert(
        "capacity".to_string(),
        ArgValue::Int(i64::try_from(capacity).unwrap_or(i64::MAX)),
    );
    args.insert("topics".to_string(), ArgValue::Str(topics.tokens.clone()));
    args.insert("groups".to_string(), ArgValue::Str(topics.groups.clone()));
    args.insert("jwt".to_string(), ArgValue::Str(topics.jwt.clone()));
}

/// The topics file's text checked, and checked against this server: a
/// JWKS or revocations in kv need `--kv`.
fn checked_topics(path: &Path, text: &str, has_kv: bool) -> Result<Topics, String> {
    let shown = path.display();
    let topics = topics_from_text(text).map_err(|e| format!("--topics {shown}: {e}"))?;
    if topics.jwks_in_kv && !has_kv {
        return Err(format!(
            "--topics {shown}: jwt.jwks names a kv key, and this server keeps no kv; add --kv <dir>"
        ));
    }
    if topics.revoked_in_kv && !has_kv {
        return Err(format!(
            "--topics {shown}: jwt.revoked names a kv topic, and this server keeps no kv; add --kv <dir>"
        ));
    }
    Ok(topics)
}

// depth: reloading the topics file

/// The `--topics` file, read every [`RELOAD_CHECK`] on the drive thread.
/// When its text changes and still checks, the program's arguments with
/// the new topics are pushed onto its `args` queue; held and pushed again
/// next pass while the queue is full. A file that does not check, or will
/// not read, is said once per change and the settings in force stay.
pub struct TopicsWatch {
    path: std::path::PathBuf,
    /// The arguments as the program was started with them.
    args: BTreeMap<String, ArgValue>,
    capacity: usize,
    has_kv: bool,
    /// The text last read, whether or not it checked; a read error's
    /// text, so it is said once.
    seen: Option<String>,
    next: std::time::Instant,
    /// Encoded arguments the queue had no room for yet.
    pending: Option<Vec<u8>>,
}

impl TopicsWatch {
    /// The arguments to hand the program, when the file changed since it
    /// was last read and checks; `Err` when it changed and does not.
    fn changed(&mut self) -> Result<Option<BTreeMap<String, ArgValue>>, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) => {
                // A file mid-replace is briefly absent: said once, and read
                // again the next time round.
                let why = format!("--topics {}: {e}", self.path.display());
                if self.seen.as_deref() == Some(why.as_str()) {
                    return Ok(None);
                }
                self.seen = Some(why.clone());
                return Err(why);
            }
        };
        if self.seen.as_deref() == Some(text.as_str()) {
            return Ok(None);
        }
        self.seen = Some(text.clone());
        let topics = checked_topics(&self.path, &text, self.has_kv)?;
        let mut args = self.args.clone();
        put_topics(&mut args, &topics, self.capacity);
        Ok(Some(args))
    }

    /// One drive-loop pass: push what is held, or read the file when due.
    fn pass(&mut self, sw: &mut crate::start::Deployment, root: drt_swarm::InstanceId) {
        if self.pending.is_none() {
            let now = std::time::Instant::now();
            if now < self.next {
                return;
            }
            self.next = now + RELOAD_CHECK;
            match self.changed() {
                Ok(None) => return,
                Ok(Some(args)) => {
                    let mut bytes = Vec::new();
                    let value = drt_config::resolve::args_to_msgpack(&args);
                    if rmpv::encode::write_value(&mut bytes, &value).is_err() {
                        return;
                    }
                    self.pending = Some(bytes);
                }
                Err(why) => {
                    eprintln!("drt p2p: {why}; the topics in force stay");
                    return;
                }
            }
        }
        let Some(bytes) = &self.pending else { return };
        match sw.push(
            root,
            drt_config::resolve::ARGS_QUEUE,
            &crate::start::runtime_sender(),
            bytes,
        ) {
            // Full: the program has not taken the last one yet.
            Err(drt_swarm::swarm::SwarmError::Limit(_)) => {}
            _ => self.pending = None,
        }
    }
}

// depth: the status token

/// A match server's status token, read and checked. Its `Debug` does not
/// print it.
#[derive(Clone, PartialEq, Eq)]
pub struct StatusToken(pub String);

impl std::fmt::Debug for StatusToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StatusToken(..)")
    }
}

impl StatusToken {
    /// The token as `what` names it, checked: it rides in `Authorization:
    /// Bearer`, unescaped, so its characters are a topic token's.
    pub fn parse(value: &str, what: &str) -> Result<StatusToken, String> {
        let value = value.trim();
        if value.len() < STATUS_TOKEN_MIN
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~+/=".contains(c))
        {
            return Err(format!(
                "{what}: a status token is {STATUS_TOKEN_MIN} or more of A-Z, a-z, 0-9 and -._~+/="
            ));
        }
        Ok(StatusToken(value.to_string()))
    }
}

// depth: the topics file

/// The `--topics` file:
///
/// ```json
/// {"jwt": {"issuer": "…", "audience": "…", "groups_claim": "groups",
///          "jwks": {"keys": […]} | {"kv": "<topic>/<key>"},
///          "revoked": {"kv": "<topic>"}},
///  "topics": {"<topic>": {"tokens": [{"token": "…", "roles": ["park", …]}],
///                         "groups": {"<group>": ["call", …]}}},
///  "capacity": 256}
/// ```
///
/// `capacity`, when present, is the names held at once in place of
/// `--capacity`, so it changes with the file.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TopicsFile {
    #[serde(default)]
    jwt: Option<JwtEntry>,
    topics: BTreeMap<String, TopicEntry>,
    #[serde(default)]
    capacity: Option<usize>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TopicEntry {
    #[serde(default)]
    tokens: Vec<TokenEntry>,
    /// A directory group's name, as a JWT's groups claim carries it, to the
    /// roles a member holds in this topic.
    #[serde(default)]
    groups: BTreeMap<String, Vec<String>>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenEntry {
    token: String,
    roles: Vec<String>,
}

/// The `jwt` block: whose tokens a topic admits, and where their public
/// keys are.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JwtEntry {
    issuer: String,
    #[serde(default)]
    audience: Option<String>,
    #[serde(default)]
    groups_claim: Option<String>,
    jwks: serde_json::Value,
    /// The kv topic whose `revoked/<sub>` keys revoke a subject's tokens;
    /// left out, the topic `jwks` is kept in, when it is kept in kv.
    #[serde(default)]
    revoked: Option<serde_json::Value>,
}

/// The topics file as the program takes it, each part JSON text or empty.
#[derive(Debug, Default)]
pub struct Topics {
    /// topic -> token -> roles; empty for a server with no `--topics`.
    pub tokens: String,
    /// topic -> group -> roles; empty when no topic maps a group.
    pub groups: String,
    /// `{issuer, audience?, groups_claim, jwks | jwks_kv, revoked_topic?}`;
    /// empty for none.
    pub jwt: String,
    /// Whether `jwks` names a key in this server's kv, which needs `--kv`.
    pub jwks_in_kv: bool,
    /// Whether `revoked` names a topic in this server's kv, which needs
    /// `--kv`. A `jwks` in kv implies it.
    pub revoked_in_kv: bool,
    /// The file's `capacity`: names held at once, in place of `--capacity`.
    pub capacity: Option<usize>,
}

/// Read and check the `--topics` file, and return it as the program takes
/// it: a token listed twice in a topic, or a group mapped twice, holds the
/// union of its roles. Every refusal names the file and the entry.
pub fn load_topics(path: &Path) -> Result<Topics, String> {
    let shown = path.display();
    let text = std::fs::read_to_string(path).map_err(|e| format!("--topics {shown}: {e}"))?;
    topics_from_text(&text).map_err(|e| format!("--topics {shown}: {e}"))
}

fn topics_from_text(text: &str) -> Result<Topics, String> {
    let file: TopicsFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if file.capacity == Some(0) {
        return Err(
            "capacity: 0 holds no name; one or more, or leave it out for --capacity".into(),
        );
    }
    let mut out: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    let mut groups: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
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
            let what = format!("topic '{topic}', token {}", i + 1);
            roles_into(tokens.entry(t.token).or_default(), t.roles, &what)?;
        }
        for (group, roles) in entry.groups {
            if group.is_empty() {
                return Err(format!("topic '{topic}': a group's name is empty"));
            }
            if file.jwt.is_none() {
                return Err(format!(
                    "topic '{topic}', group '{group}': groups are read from a JWT, and the file \
                     has no jwt block naming its issuer and keys"
                ));
            }
            let what = format!("topic '{topic}', group '{group}'");
            let held = groups.entry(topic.clone()).or_default();
            roles_into(held.entry(group).or_default(), roles, &what)?;
        }
    }
    let (jwt, jwks_in_kv, revoked_in_kv) = match file.jwt {
        None => (String::new(), false, false),
        Some(j) => jwt_block(j)?,
    };
    Ok(Topics {
        tokens: serde_json::to_string(&out).expect("strings encode"),
        groups: if groups.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&groups).expect("strings encode")
        },
        jwt,
        jwks_in_kv,
        revoked_in_kv,
        capacity: file.capacity,
    })
}

/// `roles`, checked, into `into`; `what` names the entry in a refusal.
fn roles_into(into: &mut BTreeSet<String>, roles: Vec<String>, what: &str) -> Result<(), String> {
    if roles.is_empty() {
        return Err(format!(
            "{what}: no roles; give one or more of {}",
            ROLES.join(", ")
        ));
    }
    for role in roles {
        if !ROLES.contains(&role.as_str()) {
            return Err(format!(
                "{what}: '{role}' is not a role; the roles are {}",
                ROLES.join(", ")
            ));
        }
        into.insert(role);
    }
    Ok(())
}

/// The `jwt` block checked, as the program takes it, whether its keys are
/// in kv, and whether its revocations are. An inline JWKS must hold at least one Ed25519 key with a
/// `kid` and an `x`, the only kind `crypto/jwt_verify {jwks}` uses, so a
/// file with none is refused here rather than refusing every token.
fn jwt_block(j: JwtEntry) -> Result<(String, bool, bool), String> {
    if j.issuer.is_empty() {
        return Err("jwt.issuer: name the issuer every token's `iss` must be".into());
    }
    if j.audience.as_deref() == Some("") {
        return Err("jwt.audience: empty; leave it out to not check `aud`".into());
    }
    let groups_claim = j.groups_claim.unwrap_or_else(|| GROUPS_CLAIM.to_string());
    if groups_claim.is_empty() {
        return Err("jwt.groups_claim: empty".into());
    }
    let mut out = serde_json::json!({"issuer": j.issuer, "groups_claim": groups_claim});
    if let Some(aud) = j.audience {
        out["audience"] = aud.into();
    }
    let in_kv = match &j.jwks {
        serde_json::Value::Object(o) if o.len() == 1 && o.contains_key("kv") => {
            let place = o["kv"].as_str().unwrap_or_default();
            let Some((topic, key)) = place.split_once('/') else {
                return Err(format!(
                    "jwt.jwks.kv: '{place}' is not <topic>/<key>, the kv key the JWKS is kept at"
                ));
            };
            // The kv's own rules (doc/DRT-Signalling.md §10), so the key
            // named is one a publisher can write.
            if !is_kv_topic(topic)
                || !(1..=256).contains(&key.len())
                || !kv_chars(key, true)
                || key.starts_with('/')
                || key.contains("//")
                || key.split('/').any(|seg| seg == "." || seg == "..")
            {
                return Err(format!(
                    "jwt.jwks.kv: '{place}' is not <topic>/<key> as the kv spells them"
                ));
            }
            out["jwks_kv"] = serde_json::json!({"topic": topic, "key": key});
            true
        }
        serde_json::Value::Object(o) if o.contains_key("keys") => {
            let keys = o["keys"]
                .as_array()
                .ok_or("jwt.jwks.keys: a list of JWKs")?;
            if keys.len() > JWKS_MAX_KEYS {
                return Err(format!("jwt.jwks.keys: more than {JWKS_MAX_KEYS} keys"));
            }
            let usable = keys.iter().any(|k| {
                k["kty"] == "OKP"
                    && k["crv"] == "Ed25519"
                    && k["kid"].is_string()
                    && k["x"].is_string()
            });
            if !usable {
                return Err(
                    "jwt.jwks.keys: no Ed25519 key with a kid and an x; tokens are checked \
                     against EdDSA keys only (`crypto/public_key` answers one)"
                        .into(),
                );
            }
            out["jwks"] = j.jwks.to_string().into();
            false
        }
        _ => {
            return Err(
                "jwt.jwks: a JWKS, {\"keys\": [...]}, or the kv key it is kept at, \
                 {\"kv\": \"<topic>/<key>\"}"
                    .into(),
            )
        }
    };
    // Revocations (doc/P2P.md §2.4): the topic named, or the one the keys
    // are published in, since whoever publishes the keys issues the tokens.
    let revoked = match j.revoked {
        None => out["jwks_kv"]["topic"].as_str().map(str::to_string),
        Some(serde_json::Value::Object(o)) if o.len() == 1 && o.contains_key("kv") => {
            let topic = o["kv"].as_str().unwrap_or_default();
            if !is_kv_topic(topic) {
                return Err(format!(
                    "jwt.revoked.kv: '{topic}' is not a kv topic: 1 to 64 of A-Z, a-z, 0-9 and ._~-"
                ));
            }
            Some(topic.to_string())
        }
        Some(_) => {
            return Err(
                "jwt.revoked: the kv topic whose revoked/<sub> keys revoke tokens, \
                 {\"kv\": \"<topic>\"}"
                    .into(),
            )
        }
    };
    let revoked_in_kv = revoked.is_some();
    if let Some(topic) = revoked {
        out["revoked_topic"] = topic.into();
    }
    Ok((out.to_string(), in_kv, revoked_in_kv))
}

/// A kv topic as the kv spells it (doc/DRT-Signalling.md §10).
fn is_kv_topic(s: &str) -> bool {
    (1..=64).contains(&s.len()) && kv_chars(s, false)
}

/// The kv's key charset; `slash` admits the segment separator.
fn kv_chars(s: &str, slash: bool) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "._~-".contains(c) || (slash && c == '/'))
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

/// A `jwt` block: the `crypto` connector, wired `{"verify_only": true}`
/// unless the config wires one already (a key of its own changes nothing,
/// since `jwt_verify {jwks}` consults only the keys it is handed), and the
/// one grant the program uses. Naming a grant makes the root's grants
/// explicit, so the clock the server already reads is named beside it,
/// as `wire_kv` names it.
fn wire_jwt_verify(config: &mut RootConfig) -> Result<(), String> {
    if !config.connectors.contains_key("crypto") {
        let scope: drt_caps::Scope =
            serde_json::from_value(serde_json::json!({"verify_only": true}))
                .map_err(|e| format!("--topics: {e}"))?;
        config.connectors.insert(
            "crypto".to_string(),
            drt_config::ConnectorWiring {
                backing: None,
                scope: Some(scope),
            },
        );
    }
    for cap in ["host:crypto/jwt_verify", "host:time", "host:time/*"] {
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
    fn a_topics_files_capacity_replaces_the_flags_and_zero_is_refused() {
        let named = topics_from_text(r#"{"topics": {}, "capacity": 3}"#).unwrap();
        let mut args = BTreeMap::new();
        put_topics(&mut args, &named, 256);
        assert_eq!(args["capacity"], ArgValue::Int(3));
        let unnamed = topics_from_text(r#"{"topics": {}}"#).unwrap();
        put_topics(&mut args, &unnamed, 256);
        assert_eq!(args["capacity"], ArgValue::Int(256));
        let e = topics_from_text(r#"{"topics": {}, "capacity": 0}"#)
            .err()
            .unwrap_or_default();
        assert!(e.contains("capacity: 0 holds no name"), "{e}");
    }

    /// A reload is the start's table with the file's parts replaced: what
    /// the verb set beside them (kv, the status token) is kept.
    #[test]
    fn a_changed_file_is_handed_on_once_and_a_bad_one_is_said_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("topics.json");
        std::fs::write(&path, r#"{"topics": {}}"#).unwrap();
        let mut watch = TopicsWatch {
            path: path.clone(),
            args: BTreeMap::from([("kv".to_string(), ArgValue::Str(KV_DB.into()))]),
            capacity: 256,
            has_kv: true,
            seen: std::fs::read_to_string(&path).ok(),
            next: std::time::Instant::now(),
            pending: None,
        };
        assert!(watch.changed().unwrap().is_none());
        std::fs::write(
            &path,
            r#"{"topics": {"home": {"tokens": [{"token": "t", "roles": ["park"]}]}},
                "capacity": 2}"#,
        )
        .unwrap();
        let args = watch.changed().unwrap().unwrap();
        assert_eq!(args["capacity"], ArgValue::Int(2));
        assert_eq!(
            args["topics"],
            ArgValue::Str(r#"{"home":{"t":["park"]}}"#.into())
        );
        assert_eq!(args["kv"], ArgValue::Str(KV_DB.into()));
        assert!(watch.changed().unwrap().is_none());
        std::fs::write(&path, "{").unwrap();
        assert!(watch.changed().is_err());
        assert!(watch.changed().unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
        assert!(watch.changed().is_err());
        assert!(watch.changed().unwrap().is_none());
    }

    #[test]
    fn a_topics_file_becomes_topic_token_roles_with_duplicates_merged() {
        let text = r#"{"topics": {"home": {"tokens": [
            {"token": "a", "roles": ["park"]},
            {"token": "a", "roles": ["call", "park"]},
            {"token": "r", "roles": ["read"]}
        ]}, "calls": {"tokens": []}}}"#;
        let t = topics_from_text(text).unwrap();
        assert_eq!(
            t.tokens,
            r#"{"calls":{},"home":{"a":["call","park"],"r":["read"]}}"#
        );
        assert_eq!(
            (t.groups.as_str(), t.jwt.as_str(), t.jwks_in_kv),
            ("", "", false)
        );
    }

    #[test]
    fn a_jwt_block_and_group_maps_become_what_the_program_takes() {
        let text = r#"{
          "jwt": {"issuer": "drt-session", "audience": "match1",
                  "jwks": {"keys": [{"kty": "RSA", "kid": "r"},
                                    {"kty": "OKP", "crv": "Ed25519", "kid": "s1", "x": "AAAA"}]}},
          "topics": {"home": {
            "tokens": [{"token": "static", "roles": ["park"]}],
            "groups": {"family": ["call", "read"], "admins": ["park"], "Domain Admins": ["write"]}
          }, "lab": {"groups": {"family": ["call"], "admins": ["call", "park"]}}}}"#;
        let t = topics_from_text(text).unwrap();
        assert_eq!(t.tokens, r#"{"home":{"static":["park"]},"lab":{}}"#);
        assert_eq!(
            t.groups,
            r#"{"home":{"Domain Admins":["write"],"admins":["park"],"family":["call","read"]},"lab":{"admins":["call","park"],"family":["call"]}}"#
        );
        let jwt: serde_json::Value = serde_json::from_str(&t.jwt).unwrap();
        assert_eq!(jwt["issuer"], "drt-session");
        assert_eq!(jwt["audience"], "match1");
        assert_eq!(jwt["groups_claim"], "groups");
        let jwks: serde_json::Value = serde_json::from_str(jwt["jwks"].as_str().unwrap()).unwrap();
        assert_eq!(jwks["keys"][1]["kid"], "s1");
        assert!(!t.jwks_in_kv);
        assert!(!t.revoked_in_kv && jwt.get("revoked_topic").is_none());

        let kv = r#"{"jwt": {"issuer": "i", "groups_claim": "memberOf", "jwks": {"kv": "auth/jwks"}},
                     "topics": {"home": {"groups": {"g": ["call"]}}}}"#;
        let t = topics_from_text(kv).unwrap();
        let jwt: serde_json::Value = serde_json::from_str(&t.jwt).unwrap();
        assert_eq!(
            jwt["jwks_kv"],
            serde_json::json!({"topic": "auth", "key": "jwks"})
        );
        assert_eq!(jwt["groups_claim"], "memberOf");
        assert!(jwt.get("audience").is_none());
        assert!(t.jwks_in_kv);
        // Revocations default to the topic the keys are published in.
        assert_eq!(jwt["revoked_topic"], "auth");
        assert!(t.revoked_in_kv);

        let named = r#"{"jwt": {"issuer": "i", "jwks": {"kv": "auth/jwks"}, "revoked": {"kv": "logins"}},
                        "topics": {}}"#;
        let jwt: serde_json::Value =
            serde_json::from_str(&topics_from_text(named).unwrap().jwt).unwrap();
        assert_eq!(jwt["revoked_topic"], "logins");
    }

    #[test]
    fn a_jwt_block_refuses_what_could_admit_nobody_or_anybody() {
        let jwks = r#"{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"k","x":"AAAA"}]}"#;
        for (jwt, topics, why) in [
            ("", r#"{"h":{"groups":{"g":["call"]}}}"#, "no jwt block"),
            (
                &format!(r#""jwt":{{"issuer":"","jwks":{jwks}}},"#),
                "{}",
                "jwt.issuer",
            ),
            (
                &format!(r#""jwt":{{"issuer":"i","audience":"","jwks":{jwks}}},"#),
                "{}",
                "jwt.audience",
            ),
            (r#""jwt":{"issuer":"i"},"#, "{}", "missing field `jwks`"),
            (
                r#""jwt":{"issuer":"i","jwks":{"keys":[{"kty":"RSA","kid":"k"}]}},"#,
                "{}",
                "no Ed25519 key",
            ),
            (
                r#""jwt":{"issuer":"i","jwks":{"kv":"jwks"}},"#,
                "{}",
                "not <topic>/<key>",
            ),
            (
                r#""jwt":{"issuer":"i","jwks":{"kv":"auth/../x"}},"#,
                "{}",
                "as the kv spells them",
            ),
            (
                r#""jwt":{"issuer":"i","jwks":"https://example/jwks"},"#,
                "{}",
                "jwt.jwks: a JWKS",
            ),
            (
                &format!(r#""jwt":{{"issuer":"i","jwks":{jwks},"revoked":"auth"}},"#),
                "{}",
                "jwt.revoked: the kv topic",
            ),
            (
                &format!(r#""jwt":{{"issuer":"i","jwks":{jwks},"revoked":{{"kv":"a/b"}}}},"#),
                "{}",
                "jwt.revoked.kv: 'a/b'",
            ),
            (
                &format!(r#""jwt":{{"issuer":"i","jwks":{jwks}}},"#),
                r#"{"h":{"groups":{"g":["admin"]}}}"#,
                "group 'g': 'admin' is not a role",
            ),
            (
                &format!(r#""jwt":{{"issuer":"i","jwks":{jwks}}},"#),
                r#"{"h":{"groups":{"g":[]}}}"#,
                "group 'g': no roles",
            ),
            (
                &format!(r#""jwt":{{"issuer":"i","jwks":{jwks},"extra":1}},"#),
                "{}",
                "unknown field",
            ),
        ] {
            let text = format!(r#"{{{jwt}"topics":{topics}}}"#);
            let e = topics_from_text(&text).err().unwrap_or_default();
            assert!(e.contains(why), "{text}: {e}");
        }
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

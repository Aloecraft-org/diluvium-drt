//! Assembling the root config (SPEC.md §5): the root config is a property
//! of the OS process, merged from a file, flags and env into one object.
//!
//! The file's shape is [`drt_config::RootConfig`] — those serde types are
//! the source of truth, so this module carries no schema of its own, only
//! the reading and the startup checks. JSON is the format read today;
//! everything here is plain serde, so TOML or a `.dlua` surface is a
//! deserializer swap and not a schema change.
//!
//! **Grants are validated here, at startup, by name.** A capability whose
//! scope is malformed or ill-typed for the connector it names must fail
//! while the operator is still looking at the terminal — never as a
//! mystifying `denied` at first call.

use std::path::Path;

use drt_caps::ScopeRegistry;
use drt_config::RootConfig;
use drt_connector::Registry;

/// Read a root config file. An absent path is the empty root object, which
/// is a legitimate configuration: locked out of the box, granting nothing.
pub fn load(path: Option<&Path>) -> Result<RootConfig, String> {
    let Some(path) = path else {
        return Ok(RootConfig::default());
    };
    if path.extension().is_some_and(|e| e == "lua") {
        return load_host_lua(path);
    }
    let text = drt_platform::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Check every grant against the scope-types the wired connectors declare.
///
/// The registry gates *shape*, not existence: a grant naming a capability
/// no connector declares passes here and is answered `denied` at call time,
/// which is the honest split — "this build does not carry that" is a
/// different fact from "that grant is malformed".
pub fn validate_grants(config: &RootConfig, registry: &Registry) -> Result<(), String> {
    let mut scopes = ScopeRegistry::new();
    registry.declare_scope_types(&mut scopes);
    scopes
        .validate(&config.root.caps)
        .map_err(|e| e.to_string())
}

/// What a run should be allowed to reach.
///
/// A config that names its ceiling gets exactly that ceiling. A run with no
/// config at all is the operator running their own program locally, and
/// takes the wide grant — what is actually reachable is then whatever the
/// build wires, since an unwired family answers `denied` either way.
pub fn ceiling(config: &RootConfig) -> Vec<drt_caps::Grant> {
    if config.root.caps.is_empty() {
        vec![drt_caps::Grant::grant("host:*")]
    } else {
        config.root.caps.clone()
    }
}

// ---------------------------------------------------------------------------
// `.host.lua`: the C host's config dialect, natively
// ---------------------------------------------------------------------------

/// Load a `diluvium-host` style `*.host.lua` config, so a deployment moves
/// from the C host to DRT by swapping the binary and **changing no files**.
///
/// The file is evaluated the way `dhost.c` evaluates it — by the language
/// itself, sealed, text-only — except the interpreter here is the same
/// diluvium engine that will run the deployment, under an instruction
/// budget, so a config that loops never returns and a config that is not
/// data all the way down (a function, a userdata) fails to encode and is
/// refused with the engine's own message. An unknown key is an error and
/// names itself, exactly as the C's loader promises: a typo about to become
/// a silent default is the failure mode this loader exists to catch.
pub fn load_host_lua(path: &Path) -> Result<RootConfig, String> {
    use drt_swarm::engine::{
        diluvium_engine::DiluviumEngine, Engine, LoadSpec, ProgramBytes, Step,
    };

    let text = drt_platform::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    // The file's own shape is `return { ... }`; wrapping it in a function
    // keeps that contract verbatim and pushes the result out on a queue —
    // the one channel the ABI has.
    let program = format!(
        "local __config = queue.declare('config', {{capacity = 1, exported = true}})\n\
         queue.push(__config, (function()\n{text}\nend)())\n"
    );
    let engine = DiluviumEngine::new().map_err(|e| e.to_string())?;
    let mut inst = engine
        .load(LoadSpec {
            program: ProgramBytes::Source(&program),
            name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("config"),
            budget: drt_config::Budget {
                // Evaluating a data literal costs thousands of instructions;
                // ten million is a config computing something unreasonable.
                instructions: Some(10_000_000),
                memory_kb: Some(16 * 1024),
            },
            unsafe_stdlib: false,
        })
        .map_err(|e| e.to_string())?;
    match inst.run() {
        Ok(Step::Done) => {}
        Ok(Step::Parked(_)) => {
            return Err(format!(
                "{}: a config file evaluates and returns; this one waits",
                path.display()
            ))
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let queue = inst
        .queue("config")
        .ok_or_else(|| format!("{}: the config never arrived", path.display()))?;
    let raw = inst
        .pop(queue)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{}: the file returned nothing", path.display()))?;
    let value = rmpv::decode::read_value(&mut raw.as_slice())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    map_host_lua(path, value)
}

/// Map the evaluated table onto [`RootConfig`]: the C host's field names,
/// including `connectors.listen`'s `port`/`bind`/`deadline_ms` and bare
/// capability strings.
fn map_host_lua(path: &Path, value: rmpv::Value) -> Result<RootConfig, String> {
    let rmpv::Value::Map(entries) = value else {
        return Err(format!("{}: the file must return a table", path.display()));
    };
    let mut config = RootConfig::default();
    let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
    for (key, value) in entries {
        let Some(key) = key.as_str() else {
            return Err(format!("{}: a non-string key", path.display()));
        };
        match key {
            // `supervisor` is the C's name for the root program, resolved
            // beside the config file — the deployment directory is the
            // unit that moves.
            "supervisor" => {
                let name = value
                    .as_str()
                    .ok_or_else(|| format!("{}: supervisor must be a path", path.display()))?;
                config.root.program = Some(drt_config::Program::Path(config_dir.join(name)));
            }
            "caps" => {
                let rmpv::Value::Array(items) = value else {
                    return Err(format!("{}: caps must be a list", path.display()));
                };
                for cap in items {
                    let cap = cap
                        .as_str()
                        .ok_or_else(|| format!("{}: caps entries are strings", path.display()))?;
                    config.root.caps.push(drt_caps::Grant::grant(cap));
                }
            }
            "connectors" => {
                let rmpv::Value::Map(connectors) = value else {
                    return Err(format!("{}: connectors must be a table", path.display()));
                };
                for (name, block) in connectors {
                    let Some(name) = name.as_str() else {
                        return Err(format!("{}: a non-string connector name", path.display()));
                    };
                    if name == "listen" {
                        config.listeners.push(map_listener(path, block)?);
                    } else if let rmpv::Value::Boolean(wired) = block {
                        // `time = true`: wired, with no scope of its own.
                        // `false` is the connector explicitly not wired,
                        // which is the same as absent.
                        if wired {
                            config
                                .connectors
                                .insert(name.to_string(), Default::default());
                        }
                    } else {
                        // The C's connector block *is* the scope in DRT's
                        // terms; an empty block wires the connector with no
                        // scope of its own (`time = {}`).
                        let scope = match &block {
                            rmpv::Value::Nil => None,
                            rmpv::Value::Map(m) if m.is_empty() => None,
                            rmpv::Value::Array(a) if a.is_empty() => None,
                            _ => Some(drt_caps::Scope(block)),
                        };
                        config.connectors.insert(
                            name.to_string(),
                            drt_config::ConnectorWiring {
                                backing: None,
                                scope,
                            },
                        );
                    }
                }
            }
            // DRT's own: the C host has no relay, so this key is an
            // extension rather than a dialect match. A rendezvous
            // fetchpoint is configured the same way everything else on the
            // box is, and its supervisor arbitrates over the queue bridge.
            #[cfg(feature = "relay")]
            "relay" => config.relay = Some(map_relay(path, value)?),
            // DRT's own, for the relay's reasons: the C host has no STUN.
            #[cfg(feature = "stun")]
            "stun" => config.stun = Some(map_stun(path, value)?),
            // DRT's own, for the same reasons once more: the relay for the
            // peers `stun` says cannot punch.
            #[cfg(feature = "turn")]
            "turn" => config.turn = Some(map_turn(path, value)?),
            // DRT's own once more, and the rung above the other three: what
            // carries traffic over a path the first three only measured.
            #[cfg(feature = "wireguard")]
            "wireguard" => config.wireguard = Some(map_wireguard(path, value)?),
            other => {
                // The C's loader promise, kept: an unknown key is a typo
                // about to become a silent default, so it is an error and
                // names itself.
                return Err(format!(
                    "{}: unknown key '{other}' (known: supervisor, caps, \
                     connectors, relay, stun, turn, wireguard)",
                    path.display()
                ));
            }
        }
    }
    Ok(config)
}

/// The `stun` block: the binding server as `drt start` and `drt stun` run
/// it. `bind` takes a whole `host:port` or pairs with `port`, the way
/// `relay` and `listen` do, so the three read alike.
#[cfg(feature = "stun")]
fn map_stun(path: &Path, block: rmpv::Value) -> Result<drt_config::StunConfig, String> {
    let rmpv::Value::Map(entries) = block else {
        return Err(format!("{}: stun must be a table", path.display()));
    };
    let mut stun = drt_config::StunConfig {
        bind: String::new(),
        queue: "stun_in".into(),
        report_ms: 10_000,
    };
    let mut bind = String::new();
    let mut port: Option<u64> = None;
    for (key, value) in entries {
        let Some(key) = key.as_str() else {
            return Err(format!("{}: a non-string stun key", path.display()));
        };
        let bad = |what: &str| format!("{}: stun.{key} must be {what}", path.display());
        match key {
            "bind" => bind = value.as_str().ok_or_else(|| bad("an address"))?.to_string(),
            "port" => port = Some(value.as_u64().ok_or_else(|| bad("a port number"))?),
            "queue" => stun.queue = value.as_str().ok_or_else(|| bad("a queue name"))?.into(),
            "report_ms" => stun.report_ms = value.as_u64().ok_or_else(|| bad("milliseconds"))?,
            other => {
                return Err(format!(
                    "{}: unknown stun key '{other}' (known: bind, port, \
                     queue, report_ms)",
                    path.display()
                ));
            }
        }
    }
    stun.bind = match (bind.is_empty(), port) {
        (true, _) => return Err(format!("{}: stun needs a bind address", path.display())),
        (false, Some(p)) => format!("{bind}:{p}"),
        (false, None) => bind,
    };
    Ok(stun)
}

/// The `wireguard` block: the peer this deployment is. `wg-quick`'s field
/// names, so a `[Interface]`/`[Peer]` pair transcribes rather than
/// translates, and the secret's three knobs are every other block's.
#[cfg(feature = "wireguard")]
fn map_wireguard(path: &Path, block: rmpv::Value) -> Result<drt_config::WireguardConfig, String> {
    let rmpv::Value::Map(entries) = block else {
        return Err(format!("{}: wireguard must be a table", path.display()));
    };
    let mut wg = drt_config::WireguardConfig {
        listen_port: 51820,
        interface: "drt0".into(),
        address: None,
        mtu: 1420,
        stun: Vec::new(),
        private_key: None,
        private_key_file: None,
        private_key_env: None,
        peers: Vec::new(),
        queue: "wg_in".into(),
        reply_queue: String::new(),
        report_ms: 10_000,
    };
    for (key, value) in entries {
        let Some(key) = key.as_str() else {
            return Err(format!("{}: a non-string wireguard key", path.display()));
        };
        let bad = |what: &str| format!("{}: wireguard.{key} must be {what}", path.display());
        match key {
            "listen_port" => {
                wg.listen_port = u16::try_from(value.as_u64().ok_or_else(|| bad("a port number"))?)
                    .map_err(|_| bad("a port number"))?
            }
            "interface" => {
                wg.interface = value
                    .as_str()
                    .ok_or_else(|| bad("an interface name"))?
                    .into()
            }
            "address" => {
                wg.address = Some(value.as_str().ok_or_else(|| bad("a CIDR address"))?.into())
            }
            "mtu" => {
                wg.mtu = u16::try_from(value.as_u64().ok_or_else(|| bad("an MTU"))?)
                    .map_err(|_| bad("an MTU"))?
            }
            "stun" => {
                for server in value.as_array().ok_or_else(|| bad("a list of host:port"))? {
                    wg.stun.push(
                        server
                            .as_str()
                            .ok_or_else(|| bad("a list of host:port"))?
                            .into(),
                    );
                }
            }
            "private_key" => {
                wg.private_key = Some(value.as_str().ok_or_else(|| bad("a base64 key"))?.into())
            }
            "private_key_file" => {
                wg.private_key_file = Some(value.as_str().ok_or_else(|| bad("a path"))?.into())
            }
            "private_key_env" => {
                wg.private_key_env =
                    Some(value.as_str().ok_or_else(|| bad("a variable name"))?.into())
            }
            "queue" => wg.queue = value.as_str().ok_or_else(|| bad("a queue name"))?.into(),
            "reply_queue" => {
                wg.reply_queue = value.as_str().ok_or_else(|| bad("a queue name"))?.into()
            }
            "report_ms" => wg.report_ms = value.as_u64().ok_or_else(|| bad("milliseconds"))?,
            "peers" => {
                let list = value.as_array().ok_or_else(|| bad("a list of peers"))?;
                for entry in list {
                    wg.peers.push(map_wireguard_peer(path, entry)?);
                }
            }
            other => {
                return Err(format!(
                    "{}: unknown wireguard key '{other}' (known: listen_port, interface, address, mtu, \
                     stun, private_key, private_key_file, private_key_env, peers, \
                     queue, reply_queue, report_ms)",
                    path.display()
                ));
            }
        }
    }
    Ok(wg)
}

/// One entry of `wireguard.peers`.
#[cfg(feature = "wireguard")]
fn map_wireguard_peer(
    path: &Path,
    entry: &rmpv::Value,
) -> Result<drt_config::WireguardPeer, String> {
    let rmpv::Value::Map(fields) = entry else {
        return Err(format!(
            "{}: each wireguard.peers entry must be a table",
            path.display()
        ));
    };
    let mut peer = drt_config::WireguardPeer {
        public_key: String::new(),
        allowed_ips: Vec::new(),
        endpoint: None,
        keepalive: None,
        preshared_key_env: None,
    };
    for (key, value) in fields {
        let Some(key) = key.as_str() else {
            return Err(format!(
                "{}: a non-string wireguard peer key",
                path.display()
            ));
        };
        let bad = |what: &str| format!("{}: wireguard peer {key} must be {what}", path.display());
        match key {
            "public_key" => {
                peer.public_key = value.as_str().ok_or_else(|| bad("a base64 key"))?.into()
            }
            "endpoint" => {
                peer.endpoint = Some(value.as_str().ok_or_else(|| bad("a host:port"))?.into())
            }
            "keepalive" => {
                peer.keepalive = Some(
                    u16::try_from(value.as_u64().ok_or_else(|| bad("seconds"))?)
                        .map_err(|_| bad("seconds"))?,
                )
            }
            "preshared_key_env" => {
                peer.preshared_key_env =
                    Some(value.as_str().ok_or_else(|| bad("a variable name"))?.into())
            }
            "allowed_ips" => {
                for cidr in value.as_array().ok_or_else(|| bad("a list of CIDRs"))? {
                    peer.allowed_ips
                        .push(cidr.as_str().ok_or_else(|| bad("a list of CIDRs"))?.into());
                }
            }
            other => {
                return Err(format!(
                    "{}: unknown wireguard peer key '{other}' (known: public_key, \
                     allowed_ips, endpoint, keepalive, preshared_key_env)",
                    path.display()
                ));
            }
        }
    }
    if peer.public_key.is_empty() {
        return Err(format!(
            "{}: a wireguard peer needs a public_key; it is the only name a peer has",
            path.display()
        ));
    }
    Ok(peer)
}

/// The `turn` block: the relay for the peers `stun` says cannot punch, as
/// `drt start` and `drt turn` run it. `bind` composes with `port` the way
/// `stun`'s does; the secret's three knobs are `connectors.crypto.turn`'s,
/// because it is the same secret.
#[cfg(feature = "turn")]
fn map_turn(path: &Path, block: rmpv::Value) -> Result<drt_config::TurnConfig, String> {
    let rmpv::Value::Map(entries) = block else {
        return Err(format!("{}: turn must be a table", path.display()));
    };
    let mut turn = drt_config::TurnConfig {
        bind: String::new(),
        relay_address: String::new(),
        relay_bind: "0.0.0.0".into(),
        realm: "drt".into(),
        key: None,
        key_file: None,
        key_env: None,
        max_allocations: 256,
        queue: "turn_in".into(),
        report_ms: 10_000,
    };
    let mut bind = String::new();
    let mut port: Option<u64> = None;
    for (key, value) in entries {
        let Some(key) = key.as_str() else {
            return Err(format!("{}: a non-string turn key", path.display()));
        };
        let bad = |what: &str| format!("{}: turn.{key} must be {what}", path.display());
        match key {
            "bind" => bind = value.as_str().ok_or_else(|| bad("an address"))?.to_string(),
            "port" => port = Some(value.as_u64().ok_or_else(|| bad("a port number"))?),
            "relay_address" => {
                turn.relay_address = value.as_str().ok_or_else(|| bad("an IP address"))?.into()
            }
            "relay_bind" => {
                turn.relay_bind = value.as_str().ok_or_else(|| bad("an IP address"))?.into()
            }
            "realm" => turn.realm = value.as_str().ok_or_else(|| bad("a realm"))?.into(),
            "key" => turn.key = Some(value.as_str().ok_or_else(|| bad("a string"))?.into()),
            "key_file" => turn.key_file = Some(value.as_str().ok_or_else(|| bad("a path"))?.into()),
            "key_env" => {
                turn.key_env = Some(value.as_str().ok_or_else(|| bad("a variable name"))?.into())
            }
            "max_allocations" => {
                turn.max_allocations = value.as_u64().ok_or_else(|| bad("a count"))? as usize
            }
            "queue" => turn.queue = value.as_str().ok_or_else(|| bad("a queue name"))?.into(),
            "report_ms" => turn.report_ms = value.as_u64().ok_or_else(|| bad("milliseconds"))?,
            other => {
                return Err(format!(
                    "{}: unknown turn key '{other}' (known: bind, port, relay_address, \
                     relay_bind, realm, key, key_file, key_env, max_allocations, queue, \
                     report_ms)",
                    path.display()
                ));
            }
        }
    }
    turn.bind = match (bind.is_empty(), port) {
        (true, _) => return Err(format!("{}: turn needs a bind address", path.display())),
        (false, Some(p)) => format!("{bind}:{p}"),
        (false, None) => bind,
    };
    Ok(turn)
}

/// The `relay` block: the rendezvous relay as `drt start` runs it, with
/// its labels and (optionally) its arbitration queues. `bind` takes a
/// whole `host:port` or pairs with `port`, the way `listen` does.
#[cfg(feature = "relay")]
fn map_relay(path: &Path, block: rmpv::Value) -> Result<drt_config::RelayConfig, String> {
    let rmpv::Value::Map(entries) = block else {
        return Err(format!("{}: relay must be a table", path.display()));
    };
    let mut relay = drt_config::RelayConfig {
        bind: String::new(),
        labels: Default::default(),
        queue: "relay_in".into(),
        reply_queue: String::new(),
        admit_timeout_ms: 2000,
    };
    let mut bind = String::new();
    let mut port: Option<u64> = None;
    for (key, value) in entries {
        let Some(key) = key.as_str() else {
            return Err(format!("{}: a non-string relay key", path.display()));
        };
        let bad = |what: &str| format!("{}: relay.{key} must be {what}", path.display());
        match key {
            "bind" => bind = value.as_str().ok_or_else(|| bad("an address"))?.to_string(),
            "port" => port = Some(value.as_u64().ok_or_else(|| bad("a port number"))?),
            "queue" => relay.queue = value.as_str().ok_or_else(|| bad("a queue name"))?.into(),
            "reply_queue" => {
                relay.reply_queue = value.as_str().ok_or_else(|| bad("a queue name"))?.into()
            }
            "admit_timeout_ms" => {
                relay.admit_timeout_ms = value.as_u64().ok_or_else(|| bad("milliseconds"))?
            }
            "labels" => {
                let rmpv::Value::Map(labels) = value else {
                    return Err(bad("a table of labels"));
                };
                for (name, entry) in labels {
                    let name = name
                        .as_str()
                        .ok_or_else(|| bad("a table keyed by label name"))?
                        .to_string();
                    let rmpv::Value::Map(fields) = entry else {
                        return Err(format!(
                            "{}: relay.labels.{name} must be a table",
                            path.display()
                        ));
                    };
                    let mut label = drt_config::RelayLabel::default();
                    for (k, v) in fields {
                        let text = |v: &rmpv::Value| {
                            v.as_str().map(str::to_string).ok_or_else(|| {
                                format!("{}: relay.labels.{name} keys are strings", path.display())
                            })
                        };
                        match k.as_str() {
                            Some("park_key") => label.park_key = text(&v)?,
                            Some("caller_key") => label.caller_key = text(&v)?,
                            other => {
                                return Err(format!(
                                    "{}: unknown relay label key {other:?} (known: park_key, \
                                     caller_key)",
                                    path.display()
                                ))
                            }
                        }
                    }
                    // An empty key is a closed door in `verify_key`, and a
                    // config that reaches that state has a typo in it —
                    // say so here, where the line number is still known.
                    if label.park_key.is_empty() || label.caller_key.is_empty() {
                        return Err(format!(
                            "{}: relay.labels.{name} needs both park_key and caller_key; \
                             an absent key refuses every leg",
                            path.display()
                        ));
                    }
                    relay.labels.insert(name, label);
                }
            }
            other => {
                return Err(format!(
                    "{}: unknown relay key '{other}' (known: bind, port, labels, queue, \
                     reply_queue, admit_timeout_ms)",
                    path.display()
                ))
            }
        }
    }
    relay.bind = match (bind.is_empty(), port) {
        (true, _) => return Err(format!("{}: relay names no bind address", path.display())),
        (false, Some(p)) => format!("{bind}:{p}"),
        (false, None) if bind.contains(':') => bind,
        (false, None) => {
            return Err(format!(
                "{}: relay.bind has no port; give `port` or write host:port",
                path.display()
            ))
        }
    };
    Ok(relay)
}

fn map_listener(path: &Path, block: rmpv::Value) -> Result<drt_config::Listener, String> {
    let rmpv::Value::Map(entries) = block else {
        return Err(format!(
            "{}: connectors.listen must be a table",
            path.display()
        ));
    };
    let mut port: Option<u64> = None;
    let mut bind = "127.0.0.1".to_string();
    let mut listener = drt_config::Listener {
        scheme: "http".into(),
        address: String::new(),
        queue: "http_in".into(),
        reply_queue: "http_out".into(),
        max_body: 65536,
        conn_deadline_ms: 10_000,
        admit_timeout_ms: 2000,
        max_conns: 64,
        headers: Vec::new(),
        resp_headers: Vec::new(),
    };
    for (key, value) in entries {
        let Some(key) = key.as_str() else {
            return Err(format!("{}: a non-string listener key", path.display()));
        };
        let bad = |what: &str| format!("{}: listen.{key} must be {what}", path.display());
        match key {
            "port" => port = Some(value.as_u64().ok_or_else(|| bad("a port number"))?),
            "bind" => bind = value.as_str().ok_or_else(|| bad("an address"))?.to_string(),
            "queue" => listener.queue = value.as_str().ok_or_else(|| bad("a queue name"))?.into(),
            "reply_queue" => {
                listener.reply_queue = value.as_str().ok_or_else(|| bad("a queue name"))?.into()
            }
            "max_body" => listener.max_body = value.as_u64().ok_or_else(|| bad("a size"))? as usize,
            "deadline_ms" => {
                listener.conn_deadline_ms = value.as_u64().ok_or_else(|| bad("milliseconds"))?
            }
            "max_conns" => {
                listener.max_conns = value.as_u64().ok_or_else(|| bad("a count"))? as usize
            }
            "admit_timeout_ms" => {
                listener.admit_timeout_ms = value.as_u64().ok_or_else(|| bad("milliseconds"))?
            }
            "headers" | "resp_headers" | "response_headers" => {
                let rmpv::Value::Array(items) = value else {
                    return Err(bad("a list of lowercased names"));
                };
                let out = if key == "headers" {
                    &mut listener.headers
                } else {
                    &mut listener.resp_headers
                };
                for h in items {
                    out.push(
                        h.as_str()
                            .ok_or_else(|| bad("a list of names"))?
                            .to_string(),
                    );
                }
            }
            other => {
                return Err(format!(
                    "{}: unknown listener key '{other}'",
                    path.display()
                ))
            }
        }
    }
    let port = port.ok_or_else(|| format!("{}: listen names no port", path.display()))?;
    listener.address = format!("{bind}:{port}");
    Ok(listener)
}

//! `drt p2p`: one verb for peer-to-peer sessions (`doc/P2P.md`).
//!
//! > With no `--relay` and no `--fallback`, no machine other than the two
//! > ends carries a byte of the session. When no such path exists,
//! > `drt p2p` fails and says so.
//!
//! The bare command is direct, and every way a third machine can enter
//! the path is a flag the user typed. A signalling server carries one
//! record each way and never a byte of the session; a relay carries every
//! byte, so it is asked for by name.
//!
//! ## surface block
//!
//! - Entry points: [`Args`], the command line; [`resolve`], the file's
//!   `p2p` block and the flags merged into one [`Role`]; [`run`], that role
//!   carried out; [`from_tunnel`], what `drt tunnel` was given, as this
//!   verb takes it (§9).
//! - Configurable: nothing here; each module lists its own.
//! - Fan-out: [`Role`], the four roles, and the one match on it in [`run`];
//!   the modules: [`peer`] (peer addresses, port maps, forward targets),
//!   [`http`] (the profile's requests), [`call`], [`serve`] (the host
//!   behind listen and park, and listen itself), [`park`], [`signal`] (a
//!   listening peer's own signalling port).

pub mod call;
pub mod http;
pub mod matchmaker;
pub mod park;
pub mod peer;
pub mod serve;
pub mod signal;

use std::collections::BTreeMap;
use std::path::PathBuf;

use drt_config::{P2pConfig, RootConfig, TunnelConfig};
use drt_rtc::Cidr;

use call::Dial;
use peer::{ForwardSpec, HostSpec, Peer, PortMap};

/// `drt p2p`'s command line. Each flag is one key of the `p2p` block under
/// its name (`-p` is `ports`, `-P` `forward_ports`, `-A` `all_ports`,
/// `--H` the `headers` map); [`resolve`] merges them over the file.
#[derive(clap::Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    /// The peer to call: drt://host[:port]/v1/<name> at a signalling
    /// server, drt+<service>://… to open one of its named services, a bare
    /// host (drt://), an http(s):// URL as written, a record or a file
    /// holding one (direct mode: nothing is sent). Absent with --park,
    /// --listen or --match.
    #[arg(value_name = "PEER")]
    pub peer: Option<String>,
    /// Print the canonical form of a peer address and exit: one spelling
    /// for every form that names the same peer, for keying things by peer.
    #[arg(long, value_name = "PEER")]
    pub show: Option<String>,
    /// Map a port, in ssh -L's shape: <local>:<remote> binds 127.0.0.1:<local>
    /// and asks the far side for <remote> on each connection; :<remote>
    /// makes stdio ask for it; <local> alone asks for whatever the far side
    /// forwards to. <remote> is a port, a service name, or host:port.
    /// Repeatable. Without -p, stdio is the session (the ProxyCommand form).
    #[arg(short = 'p', value_name = "LOCAL:REMOTE")]
    pub ports: Vec<String>,
    /// Carry the session through this peer, which calls the destination
    /// for you. A wss:// relay URL carries bytes to the label it names.
    #[arg(long, value_name = "PEER")]
    pub relay: Option<String>,
    /// Like --relay, but tried only when no direct path exists.
    #[arg(long, value_name = "PEER")]
    pub fallback: Option<String>,
    /// Answer calls for a name at this signalling server
    /// (drt://host/v1/<name>), serving --forward; or hold a leg at a wss://
    /// relay's /park URL.
    #[arg(long, value_name = "SIGNALLING")]
    pub park: Option<String>,
    /// Serve --forward on this UDP port with a fixed record, which is
    /// printed with the command that calls it. No signalling unless
    /// --signal.
    #[arg(long, value_name = "PORT")]
    pub listen: Option<u16>,
    /// Be a signalling server on this TCP port: the reference server for
    /// doc/DRT-Signalling.md. Names are claimed by the first answerer token
    /// to poll them; a caller token is optional, so without one anyone may
    /// call a name. Admission is by token, not origin, so every reply
    /// carries access-control-allow-origin: *. Put a TLS terminator in
    /// front for pages on https:// origins.
    #[arg(long = "match", value_name = "PORT")]
    pub match_port: Option<u16>,
    /// Who may connect to --listen or --match: an address to bind (default
    /// 127.0.0.1), 0.0.0.0 for anyone, or a CIDR such as a WireGuard subnet,
    /// which admits that range and binds this machine's address inside it.
    #[arg(long, value_name = "ADDRESS|CIDR")]
    pub host: Option<String>,
    /// With --park: admit callers from this range only. Sent to the
    /// signalling server and checked here as well. Repeatable.
    #[arg(long, value_name = "CIDR")]
    pub accept: Vec<String>,
    /// With --park: let the signalling server pair this side with another
    /// parked peer by telling it whom to call: * for any name at the server
    /// it is parked at, or drt://<server>/v1/<glob> for a name pattern at a
    /// named server. Without it, such a request is declined and the server
    /// is told so.
    #[arg(long, value_name = "ALLOW")]
    pub pair: Option<String>,
    /// What --listen or --park serves: host:port for one target;
    /// ssh://host:port for one target as the named service ssh; a host
    /// with -P or -A for its ports; drt://… for another DRT peer; - for
    /// this process's stdio; absent for the REPL, through the built-in SSH
    /// server (service ssh) and as raw terminal bytes (service repl).
    /// Bare --forward is a relay: the caller names the destination.
    #[arg(long, value_name = "TARGET", num_args = 0..=1, default_missing_value = "")]
    pub forward: Option<String>,
    /// With --forward <host>: every port of it.
    #[arg(short = 'A')]
    pub all_ports: bool,
    /// With --forward <host>: these ports of it, 80,8080:8090,31200.
    #[arg(short = 'P', value_name = "PORTS")]
    pub forward_ports: Option<String>,
    /// With --listen: also answer the caller's signalling request on this
    /// TCP port (bare: one the system chooses), so a caller needs an
    /// address and not the record. Answers POST / and /v1/<any>/calls, with
    /// access-control-allow-origin: *.
    #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "0")]
    pub signal: Option<u16>,
    /// A STUN server (host:port) asked for this side's public address, so
    /// a peer behind another NAT has one to reach. Repeatable.
    #[arg(long, value_name = "HOST:PORT")]
    pub stun: Vec<String>,
    /// The answerer's DTLS fingerprint, as the listening side prints it
    /// (SHA256:…). A record whose fingerprint differs is refused: the
    /// defence against a signalling server answering with its own.
    #[arg(long, alias = "fingerp", value_name = "SHA256:…")]
    pub fingerprint: Option<String>,
    /// A header for the signalling side, name=value; auth=<token> is
    /// shorthand for Authorization: Bearer <token>. Never reaches the peer.
    /// Repeatable.
    #[arg(long = "H", value_name = "NAME=VALUE")]
    pub header: Vec<String>,
    /// With --match: names held at once.
    #[arg(long, value_name = "NAMES")]
    pub capacity: Option<usize>,
    /// Trust this PEM certificate beside the public roots, for a signalling
    /// server behind an internal CA. Repeatable; added, never substituted.
    #[arg(long = "extra-root", value_name = "PEM")]
    pub extra_root: Vec<PathBuf>,
    /// The REPL's authorized_keys file, in place of ~/.ssh/authorized_keys.
    #[arg(long = "authorized-keys", value_name = "FILE")]
    pub authorized_keys: Option<PathBuf>,
}

/// What the serving side needs beside its forward.
#[derive(Debug, Clone, Default)]
pub struct ServeSettings {
    pub authorized_keys: Option<PathBuf>,
    pub identity_file: Option<PathBuf>,
    /// For a `drt://` forward: how the forwarder calls.
    pub stun: Vec<String>,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct CallRole {
    pub peer: Peer,
    /// The peer as typed: what a relay is told to call (§4.1).
    pub destination: String,
    pub maps: Vec<PortMap>,
    pub dial: Dial,
    pub relay: Option<Peer>,
    pub fallback: Option<Peer>,
}

#[derive(Debug)]
pub struct ParkRole {
    pub signalling: Peer,
    pub forward: ForwardSpec,
    pub accept: Vec<Cidr>,
    /// Whom the server may tell this side to call; `None` declines all.
    pub pair: Option<park::PairRule>,
    pub headers: Vec<(String, String)>,
    pub stun: Vec<String>,
    pub settings: ServeSettings,
}

#[derive(Debug)]
pub struct ListenRole {
    pub port: u16,
    pub host: HostSpec,
    pub forward: ForwardSpec,
    /// `Some(0)`: a port the system chooses.
    pub signal: Option<u16>,
    pub stun: Vec<String>,
    pub settings: ServeSettings,
}

#[derive(Debug)]
pub struct MatchRole {
    pub port: u16,
    pub host: HostSpec,
    pub capacity: usize,
}

/// The four roles (`doc/P2P.md` §2).
#[derive(Debug)]
pub enum Role {
    Call(CallRole),
    Park(ParkRole),
    Listen(ListenRole),
    Match(MatchRole),
}

#[derive(Debug)]
pub struct Resolved {
    pub role: Role,
    pub extra_roots: Vec<PathBuf>,
    pub extra_roots_key: &'static str,
}

/// Where a key came from, so a conflict names the line and the flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum From {
    File,
    Flag,
}

fn spelled(key: &str, from: From) -> String {
    match from {
        From::File => format!("`p2p.{key}` in the config"),
        From::Flag => match key {
            "peer" => "the peer on the command line".to_string(),
            "ports" => "`-p`".to_string(),
            "forward_ports" => "`-P`".to_string(),
            "all_ports" => "`-A`".to_string(),
            "headers" => "`--H`".to_string(),
            "match" => "`--match`".to_string(),
            other => format!("`--{}`", other.replace('_', "-")),
        },
    }
}

/// The file's `p2p` block (or its `tunnel` block, read as an alias) and
/// the flags, merged into one [`Role`]. Flags win per key; two roles in one
/// invocation are refused by name and source, as `drt tunnel` refused two
/// modes.
pub fn resolve(config: &RootConfig, flags: &Args) -> Result<Resolved, String> {
    let aliased;
    let file: Option<&P2pConfig> = match (&config.p2p, &config.tunnel) {
        (Some(_), Some(_)) => {
            return Err("the config names both `p2p` and `tunnel`; `tunnel` is `p2p` now".into())
        }
        (Some(p), None) => Some(p),
        (None, Some(t)) => {
            let (block, warnings) = from_tunnel_block(t)?;
            for w in warnings {
                eprintln!("drt p2p: {w}");
            }
            aliased = block;
            Some(&aliased)
        }
        (None, None) => None,
    };
    let empty = P2pConfig::default();
    let f = file.unwrap_or(&empty);

    // One key, one source, the flag's when both name it.
    fn pick<T: Clone>(flag: Option<&T>, file: Option<&T>) -> Option<(T, From)> {
        match (flag, file) {
            (Some(v), _) => Some((v.clone(), From::Flag)),
            (None, Some(v)) => Some((v.clone(), From::File)),
            (None, None) => None,
        }
    }
    let peer = pick(flags.peer.as_ref(), f.peer.as_ref());
    let ports = if flags.ports.is_empty() {
        (!f.ports.is_empty()).then(|| (f.ports.clone(), From::File))
    } else {
        Some((flags.ports.clone(), From::Flag))
    };
    let relay = pick(flags.relay.as_ref(), f.relay.as_ref());
    let fallback = pick(flags.fallback.as_ref(), f.fallback.as_ref());
    let park = pick(flags.park.as_ref(), f.park.as_ref());
    let listen = pick(flags.listen.as_ref(), f.listen.as_ref());
    let matching = pick(flags.match_port.as_ref(), f.match_port.as_ref());
    let host = pick(flags.host.as_ref(), f.host.as_ref());
    let accept = if flags.accept.is_empty() {
        (!f.accept.is_empty()).then(|| (f.accept.clone(), From::File))
    } else {
        Some((flags.accept.clone(), From::Flag))
    };
    let pair = pick(flags.pair.as_ref(), f.pair.as_ref());
    let forward = pick(flags.forward.as_ref(), f.forward.as_ref());
    let all_ports = if flags.all_ports {
        Some((true, From::Flag))
    } else {
        f.all_ports.then_some((true, From::File))
    };
    let forward_ports = pick(flags.forward_ports.as_ref(), f.forward_ports.as_ref());
    let signal = pick(flags.signal.as_ref(), f.signal.as_ref());
    let stun = if flags.stun.is_empty() {
        f.stun.clone()
    } else {
        flags.stun.clone()
    };
    let fingerprint = pick(flags.fingerprint.as_ref(), f.fingerprint.as_ref());
    let capacity = pick(flags.capacity.as_ref(), f.capacity.as_ref());
    let authorized_keys = pick(flags.authorized_keys.as_ref(), f.authorized_keys.as_ref());
    let (extra_roots, extra_roots_key) = if !flags.extra_root.is_empty() {
        (flags.extra_root.clone(), "--extra-root")
    } else {
        (f.extra_roots.clone(), "p2p.extra_roots")
    };
    let headers = headers(&f.headers, &flags.header)?;
    let headers_from = if !flags.header.is_empty() {
        Some(From::Flag)
    } else if !f.headers.is_empty() {
        Some(From::File)
    } else {
        None
    };

    // One role, named by the key that chose it.
    let roles: Vec<(&str, From)> = [
        ("peer", peer.as_ref().map(|(_, s)| *s)),
        ("park", park.as_ref().map(|(_, s)| *s)),
        ("listen", listen.as_ref().map(|(_, s)| *s)),
        ("match", matching.as_ref().map(|(_, s)| *s)),
    ]
    .into_iter()
    .filter_map(|(name, from)| from.map(|from| (name, from)))
    .collect();
    if let [(a, sa), (b, sb), ..] = roles[..] {
        return Err(format!(
            "{} and {} name two roles; a p2p is one call, one park, one listen, or one match",
            spelled(a, sa),
            spelled(b, sb)
        ));
    }
    // A key from another role, named rather than ignored.
    let belongs = |key: &str, owner: &str, present: Option<From>| match present {
        Some(from) => Err(format!(
            "{} belongs with {owner}, and this is not one",
            spelled(key, from)
        )),
        None => Ok(()),
    };
    let from = |o: &Option<(String, From)>| o.as_ref().map(|(_, s)| *s);
    let settings = |stun: &Vec<String>, headers: &Vec<(String, String)>| ServeSettings {
        authorized_keys: authorized_keys.as_ref().map(|(p, _)| p.clone()),
        identity_file: f.identity_file.clone(),
        stun: stun.clone(),
        headers: headers.clone(),
    };
    let serving_only = |role: &str| -> Result<(), String> {
        belongs("ports", "a call", ports.as_ref().map(|(_, s)| *s))?;
        belongs("fingerprint", "a call", from(&fingerprint))?;
        belongs("fallback", "a call", from(&fallback))?;
        let _ = role;
        Ok(())
    };
    let role = match (peer, park, listen, matching) {
        (Some((peer, _)), None, None, None) => {
            belongs("host", "a listen or a match", from(&host))?;
            belongs("accept", "a park", accept.as_ref().map(|(_, s)| *s))?;
            belongs("forward", "a park or a listen", from(&forward))?;
            belongs("all_ports", "a park or a listen", all_ports.map(|(_, s)| s))?;
            belongs("forward_ports", "a park or a listen", from(&forward_ports))?;
            belongs("signal", "a listen", signal.as_ref().map(|(_, s)| *s))?;
            belongs("capacity", "a match", capacity.as_ref().map(|(_, s)| *s))?;
            belongs(
                "authorized_keys",
                "a park or a listen",
                authorized_keys.as_ref().map(|(_, s)| *s),
            )?;
            let destination = peer.clone();
            let peer = Peer::parse(&peer)?;
            let maps = ports
                .map(|(list, _)| {
                    list.iter()
                        .map(|m| PortMap::parse(m))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            if peer.service.is_some() && maps.iter().any(|m| m.local.is_none()) {
                return Err("the peer's drt+<service>:// already says what stdio asks for; -p :<remote> says another".into());
            }
            if maps.iter().filter(|m| m.local.is_none()).count() > 1 {
                return Err("stdio can ask for one thing: one -p :<remote>".into());
            }
            let relay = relay
                .map(|(r, _)| Peer::parse(&r).map_err(|e| format!("--relay {e}")))
                .transpose()?;
            let fallback = fallback
                .map(|(r, _)| Peer::parse(&r).map_err(|e| format!("--fallback {e}")))
                .transpose()?;
            if relay.is_some() && fallback.is_some() {
                return Err("--relay and --fallback are one choice: always through the peer, or only when nothing is direct".into());
            }
            Role::Call(CallRole {
                peer,
                destination,
                maps,
                dial: Dial {
                    stun,
                    headers,
                    fingerprint: fingerprint
                        .map(|(f, _)| peer::fingerprint(&f))
                        .transpose()?,
                },
                relay,
                fallback,
            })
        }
        (None, None, None, None) if relay.is_some() => {
            // A wss:// relay's label names the destination (§9): a call with
            // no positional.
            belongs("fallback", "a call to a peer", from(&fallback))?;
            let (relay, _) = relay.expect("checked");
            let relay = Peer::parse(&relay).map_err(|e| format!("--relay {e}"))?;
            if !matches!(relay.how, peer::How::Ws(_)) {
                return Err("--relay through a DRT peer needs the peer to reach; a wss:// relay's label names it".into());
            }
            let maps = ports
                .map(|(list, _)| {
                    list.iter()
                        .map(|m| PortMap::parse(m))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            Role::Call(CallRole {
                peer: relay.clone(),
                destination: String::new(),
                maps,
                dial: Dial {
                    stun,
                    headers,
                    fingerprint: None,
                },
                relay: Some(relay),
                fallback: None,
            })
        }
        (None, Some((park, _)), None, None) => {
            serving_only("a park")?;
            belongs("host", "a listen or a match", from(&host))?;
            belongs("signal", "a listen", signal.as_ref().map(|(_, s)| *s))?;
            belongs("capacity", "a match", capacity.as_ref().map(|(_, s)| *s))?;
            if relay.is_some() {
                return Err("--relay beside --park (\"answer for me\") is not built yet; a park that cannot use UDP holds a wss:// leg instead (--park wss://…)".into());
            }
            let signalling = Peer::parse(&park).map_err(|e| format!("--park {e}"))?;
            let forward = ForwardSpec::parse(
                forward.as_ref().map(|(f, _)| f.as_str()),
                forward_ports.as_ref().map(|(p, _)| p.as_str()),
                all_ports.is_some(),
            )?;
            let accept = accept
                .map(|(list, _)| {
                    list.iter()
                        .map(|c| Cidr::parse(c).map_err(|e| format!("--accept {e}")))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            let pair = pair
                .map(|(allow, _)| park::PairRule::parse(&allow).map_err(|e| format!("--pair {e}")))
                .transpose()?;
            Role::Park(ParkRole {
                signalling,
                forward,
                accept,
                pair,
                settings: settings(&stun, &headers),
                headers,
                stun,
            })
        }
        (None, None, Some((port, _)), None) => {
            serving_only("a listen")?;
            belongs("accept", "a park", accept.as_ref().map(|(_, s)| *s))?;
            belongs("pair", "a park", from(&pair))?;
            belongs("capacity", "a match", capacity.as_ref().map(|(_, s)| *s))?;
            belongs("relay", "a call or a park", from(&relay))?;
            if let Some(from) = headers_from {
                return Err(format!("{} belongs with a call, a park or a match's callers; a listen sends no request", spelled("headers", from)));
            }
            let host = match host {
                Some((h, _)) => HostSpec::parse(&h)?,
                None => HostSpec::Addr(std::net::Ipv4Addr::LOCALHOST.into()),
            };
            let forward = ForwardSpec::parse(
                forward.as_ref().map(|(f, _)| f.as_str()),
                forward_ports.as_ref().map(|(p, _)| p.as_str()),
                all_ports.is_some(),
            )?;
            Role::Listen(ListenRole {
                port,
                host,
                forward,
                signal: signal.map(|(p, _)| p),
                settings: settings(&stun, &Vec::new()),
                stun,
            })
        }
        (None, None, None, Some((port, _))) => {
            serving_only("a match")?;
            belongs("accept", "a park", accept.as_ref().map(|(_, s)| *s))?;
            belongs("pair", "a park", from(&pair))?;
            belongs("signal", "a listen", signal.as_ref().map(|(_, s)| *s))?;
            belongs(
                "authorized_keys",
                "a park or a listen",
                authorized_keys.as_ref().map(|(_, s)| *s),
            )?;
            // §2.4: a signalling server carries no session bytes.
            for (key, present) in [
                ("forward", from(&forward)),
                ("relay", from(&relay)),
                ("all_ports", all_ports.map(|(_, s)| s)),
                ("forward_ports", from(&forward_ports)),
            ] {
                if let Some(from) = present {
                    return Err(format!("{} is refused with --match: a signalling server carries no session bytes (doc/P2P.md §10)", spelled(key, from)));
                }
            }
            let host = match host {
                Some((h, _)) => HostSpec::parse(&h)?,
                None => HostSpec::Addr(std::net::Ipv4Addr::LOCALHOST.into()),
            };
            Role::Match(MatchRole {
                port,
                host,
                capacity: capacity.map(|(c, _)| c).unwrap_or(256),
            })
        }
        _ => {
            return Err(
                "name a peer to call, --park <signalling>, --listen <port>, or --match <port>; \
                 or `p2p` in the --config file, which takes the same keys (drt p2p --help)"
                    .into(),
            )
        }
    };
    Ok(Resolved {
        role,
        extra_roots,
        extra_roots_key,
    })
}

/// The file's `headers` map, then each `--H` over it, a flag replacing the
/// file's header of the same name. `auth` is `Authorization: Bearer`.
fn headers(
    file: &BTreeMap<String, String>,
    flags: &[String],
) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |name: &str, value: &str, how: &str| -> Result<(), String> {
        let (name, value) = if name.eq_ignore_ascii_case("auth") {
            (
                "Authorization".to_string(),
                format!("Bearer {}", value.trim()),
            )
        } else {
            (name.trim().to_string(), value.trim().to_string())
        };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(format!("{how}: `{name}` is not a header name"));
        }
        if value.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(format!(
                "{how}: the value of `{name}` is not a header value"
            ));
        }
        match out
            .iter_mut()
            .find(|(have, _)| have.eq_ignore_ascii_case(&name))
        {
            Some(entry) => *entry = (name, value),
            None => out.push((name, value)),
        }
        Ok(())
    };
    for (name, value) in file {
        put(name, value, "`p2p.headers` in the config")?;
    }
    for spec in flags {
        let (name, value) = spec
            .split_once('=')
            .ok_or_else(|| format!("`--H '{spec}'` is not name=value"))?;
        put(name, value, &format!("`--H '{spec}'`"))?;
    }
    Ok(out)
}

/// Carry out a [`Resolved`]: the one match on [`Role`]. `--match` runs a
/// deployment and never returns; the others run on a runtime of their own.
pub fn run(args: &Args, config: &RootConfig) -> Result<(), String> {
    if let Some(peer) = &args.show {
        println!("{}", Peer::parse(peer)?.canonical());
        return Ok(());
    }
    let resolved = resolve(config, args)?;
    let roots = crate::roots::load_roots_named(resolved.extra_roots_key, &resolved.extra_roots)?;
    if let Role::Match(m) = &resolved.role {
        return matchmaker::run(m, config);
    }
    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("a runtime: {e}"))?;
    let outcome = runtime.block_on(async {
        match &resolved.role {
            Role::Call(c) => call::run(c, &roots).await,
            Role::Park(p) => park::run(p, config, &roots).await,
            Role::Listen(l) => serve::listen(l, config, &roots).await,
            Role::Match(_) => unreachable!("dispatched above"),
        }
    });
    // Leaked for the reason every other verb here leaks one: tokio 1.53.1's
    // teardown race (cli.rs's note beside `drt tunnel`).
    std::mem::forget(runtime);
    outcome
}

// depth: `drt tunnel`, as this verb takes it (§9)

/// A `tunnel` block as the `p2p` block it means, with one warning per key
/// naming its replacement. `listen` has none: the WebSocket to TCP bridge
/// is a server-side shim and moves to the `relay` block.
pub fn from_tunnel_block(t: &TunnelConfig) -> Result<(P2pConfig, Vec<String>), String> {
    let mut block = P2pConfig::default();
    let mut warnings = vec!["`tunnel` in the config is read as `p2p` for one release".to_string()];
    if let Some(claim) = &t.claim {
        if let Some(rest) = claim.strip_prefix("rtc:") {
            warnings.push("`tunnel.claim` with `rtc:` is `p2p.peer`, without the `rtc:`".into());
            block.peer = Some(rest.to_string());
            if let Some(to) = &t.to {
                warnings
                    .push("`tunnel.to` on an `rtc:` claim is `p2p.ports` as [\":<to>\"]".into());
                block.ports = vec![format!(":{to}")];
            }
        } else {
            warnings.push(
                "`tunnel.claim` is `p2p.relay`: the relay's label names the destination".into(),
            );
            block.relay = Some(claim.clone());
        }
        if let Some(bind) = &t.bind {
            let port = bind.rsplit_once(':').map(|(_, p)| p).unwrap_or(bind);
            warnings.push(format!(
                "`tunnel.bind` is `p2p.ports` as [\"{port}\"], on 127.0.0.1"
            ));
            block.ports = vec![port.to_string()];
        }
    }
    if let Some(park) = &t.park {
        warnings.push("`tunnel.park` is `p2p.park`, and `tunnel.to` is `p2p.forward`".into());
        block.park = Some(park.clone());
        block.forward = t.to.clone();
    }
    if t.listen.is_some() {
        return Err("`tunnel.listen`, the WebSocket to TCP bridge, is not a peer; it is the `relay` block's to serve now".into());
    }
    block.extra_roots = t.extra_roots.clone();
    block.headers = t.headers.clone();
    Ok((block, warnings))
}

/// What `drt tunnel` was given, as the `drt p2p` command it is now, for the
/// alias to print and run. `None` for the bridge, which is not a peer.
pub fn from_tunnel(
    mode: &crate::tunnel::Mode,
    resolved: &crate::tunnel::Resolved,
) -> Option<(String, Args)> {
    use crate::tunnel::Mode;
    let mut args = Args {
        extra_root: resolved.extra_roots.clone(),
        header: resolved
            .headers
            .iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect(),
        ..Args::default()
    };
    let mut words: Vec<String> = vec!["drt".into(), "p2p".into()];
    match mode {
        Mode::Stdio { claim } => {
            args.relay = Some(claim.clone());
            words.extend(["--relay".to_string(), crate::tunnel::shown(claim)]);
        }
        Mode::Local { claim, bind } => {
            let port = bind
                .rsplit_once(':')
                .map(|(_, p)| p)
                .unwrap_or(bind)
                .to_string();
            args.relay = Some(claim.clone());
            args.ports = vec![port.clone()];
            words.extend([
                "--relay".to_string(),
                crate::tunnel::shown(claim),
                "-p".to_string(),
                port,
            ]);
        }
        Mode::Park { park, to } => {
            args.park = Some(park.clone());
            args.forward = Some(to.clone());
            words.extend([
                "--park".to_string(),
                crate::tunnel::shown(park),
                "--forward".to_string(),
                to.clone(),
            ]);
        }
        Mode::Listen { .. } => return None,
        Mode::Rtc { peer, to } => {
            args.peer = Some(peer.clone());
            words.push(if peer.starts_with('{') {
                "<record>".to_string()
            } else {
                peer.clone()
            });
            if to != "ssh" || !peer.starts_with('{') {
                args.ports = vec![format!(":{to}")];
                words.extend(["-p".to_string(), format!(":{to}")]);
            }
        }
    }
    for (n, v) in &resolved.headers {
        words.extend([
            "--H".to_string(),
            format!(
                "{n}={}",
                if n.eq_ignore_ascii_case("authorization") {
                    "…"
                } else {
                    v
                }
            ),
        ]);
    }
    for root in &resolved.extra_roots {
        words.extend(["--extra-root".to_string(), root.display().to_string()]);
    }
    Some((words.join(" "), args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(json: &str) -> RootConfig {
        serde_json::from_str(json).unwrap()
    }

    fn role(config: &RootConfig, args: Args) -> Result<Role, String> {
        resolve(config, &args).map(|r| r.role)
    }

    #[test]
    fn each_role_is_told_by_its_key_and_two_are_refused_by_name() {
        let none = cfg("{}");
        assert!(matches!(
            role(
                &none,
                Args {
                    peer: Some("drt://s.example/v1/a".into()),
                    ..Args::default()
                }
            ),
            Ok(Role::Call(_))
        ));
        assert!(matches!(
            role(
                &none,
                Args {
                    park: Some("drt://s.example/v1/a".into()),
                    ..Args::default()
                }
            ),
            Ok(Role::Park(_))
        ));
        assert!(matches!(
            role(
                &none,
                Args {
                    listen: Some(5000),
                    ..Args::default()
                }
            ),
            Ok(Role::Listen(_))
        ));
        assert!(matches!(
            role(
                &none,
                Args {
                    match_port: Some(8443),
                    ..Args::default()
                }
            ),
            Ok(Role::Match(_))
        ));
        let e = role(
            &none,
            Args {
                listen: Some(5000),
                park: Some("drt://x/v1/a".into()),
                ..Args::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("`--park`") && e.contains("`--listen`"), "{e}");
        let e = role(&none, Args::default()).unwrap_err();
        assert!(e.contains("--listen <port>"), "{e}");
    }

    #[test]
    fn a_flag_replaces_the_file_key_it_names_and_a_role_conflict_names_both_sources() {
        let file = cfg(r#"{"p2p":{"listen":5000,"forward":"127.0.0.1:8080","host":"0.0.0.0"}}"#);
        let Ok(Role::Listen(l)) = role(
            &file,
            Args {
                listen: Some(6000),
                ..Args::default()
            },
        ) else {
            panic!()
        };
        assert_eq!(l.port, 6000);
        assert_eq!(l.host, HostSpec::Addr("0.0.0.0".parse().unwrap()));
        let e = role(
            &file,
            Args {
                peer: Some("drt://x/v1/a".into()),
                ..Args::default()
            },
        )
        .unwrap_err();
        assert!(
            e.contains("`p2p.listen` in the config") && e.contains("the peer on the command line"),
            "{e}"
        );
    }

    #[test]
    fn pair_is_a_parks_key_and_parses_to_its_rule() {
        let none = cfg("{}");
        let Ok(Role::Park(p)) = role(
            &none,
            Args {
                park: Some("drt://s.example/v1/a".into()),
                pair: Some("*".into()),
                ..Args::default()
            },
        ) else {
            panic!()
        };
        assert_eq!(p.pair, Some(park::PairRule::AnyHere));
        let file =
            cfg(r#"{"p2p":{"park":"drt://s.example/v1/a","pair":"drt://s.example/v1/room-*"}}"#);
        let Ok(Role::Park(p)) = role(&file, Args::default()) else {
            panic!()
        };
        assert!(matches!(p.pair, Some(park::PairRule::At { .. })));
        let e = role(
            &none,
            Args {
                listen: Some(5000),
                pair: Some("*".into()),
                ..Args::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("`--pair`") && e.contains("a park"), "{e}");
    }

    #[test]
    fn a_key_from_another_role_is_refused_rather_than_ignored() {
        let none = cfg("{}");
        let e = role(
            &none,
            Args {
                listen: Some(5000),
                fingerprint: Some("SHA256:x".into()),
                ..Args::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("`--fingerprint` belongs with a call"), "{e}");
        let e = role(
            &none,
            Args {
                match_port: Some(1),
                forward: Some("127.0.0.1:22".into()),
                ..Args::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("carries no session bytes"), "{e}");
        let e = role(
            &none,
            Args {
                peer: Some("drt://x/v1/a".into()),
                signal: Some(0),
                ..Args::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("`--signal` belongs with a listen"), "{e}");
    }

    #[test]
    fn headers_merge_by_name_and_auth_is_a_bearer() {
        let file = cfg(
            r#"{"p2p":{"park":"drt://s.example/v1/a","headers":{"auth":"file-token","X-One":"1"}}}"#,
        );
        let Ok(Role::Park(p)) = role(
            &file,
            Args {
                header: vec!["auth=flag-token".into(), "DRT-Caller-Token=c".into()],
                ..Args::default()
            },
        ) else {
            panic!()
        };
        assert_eq!(
            p.headers,
            vec![
                ("X-One".to_string(), "1".to_string()),
                ("Authorization".to_string(), "Bearer flag-token".to_string()),
                ("DRT-Caller-Token".to_string(), "c".to_string()),
            ]
        );
        let e = role(
            &cfg("{}"),
            Args {
                peer: Some("drt://x/v1/a".into()),
                header: vec!["nope".into()],
                ..Args::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("name=value"), "{e}");
    }

    #[test]
    fn a_tunnel_block_is_read_as_p2p_with_a_warning_per_key() {
        let park = cfg(r#"{"tunnel":{"park":"ws://127.0.0.1:1/park/fp?k=x","to":"127.0.0.1:22"}}"#);
        let Ok(Role::Park(p)) = role(&park, Args::default()) else {
            panic!()
        };
        assert!(matches!(p.signalling.how, peer::How::Ws(_)));
        assert_eq!(p.forward.describe(), "127.0.0.1:22");
        let claim =
            cfg(r#"{"tunnel":{"claim":"ws://127.0.0.1:1/s/fp?k=x","bind":"127.0.0.1:2222"}}"#);
        let Ok(Role::Call(c)) = role(&claim, Args::default()) else {
            panic!()
        };
        assert!(c.relay.is_some());
        assert_eq!(c.maps[0].local, Some(2222));
        let rtc = cfg(r#"{"tunnel":{"claim":"rtc:https://box.example/v1/box/calls","to":"ssh"}}"#);
        let Ok(Role::Call(c)) = role(&rtc, Args::default()) else {
            panic!()
        };
        assert_eq!(
            c.peer.calls_url().as_deref(),
            Some("https://box.example/v1/box/calls")
        );
        assert_eq!(
            c.maps[0].target,
            drt_rtc::caller::Target::Service("ssh".into())
        );
        let bridge = cfg(r#"{"tunnel":{"listen":"127.0.0.1:8022","to":"127.0.0.1:22"}}"#);
        assert!(role(&bridge, Args::default())
            .unwrap_err()
            .contains("`relay` block"));
    }
}

//! What `drt p2p` is pointed at: a peer address (`doc/P2P.md` §3), a port
//! map (`-p`, §2.1), a forward target (`--forward`, §5), a host or range
//! (`--host`, §6), and a fingerprint (`--fingerprint`). Each is parsed here
//! and nowhere else, so every refusal about one is one list in one
//! vocabulary.
//!
//! ## surface block
//!
//! - Entry points: [`Peer::parse`], [`PortMap::parse`], [`ForwardSpec::parse`],
//!   [`HostSpec::parse`], [`fingerprint`], [`fingerprint_text`].
//! - Configurable: none; the shapes are the proposal's.
//! - Fan-out: [`How`], the three ways a peer is reached (a signalling
//!   request, a record in hand, a WebSocket leg); [`ForwardSpec`], the
//!   rows of §5's table.

use std::net::IpAddr;
use std::path::Path;

use drt_rtc::caller::Target;
use drt_rtc::{Cidr, Entry, PortSet, Record};

/// A peer address: how the far side is reached, and the named service to
/// open on it when the address said one (`drt+<service>://`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub how: How,
    pub service: Option<String>,
}

/// The three ways a peer is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// The caller's request of `doc/DRT-Signalling.md` §3 at `base`:
    /// `http(s)://host[:port]/v1/<name>` when the address names an
    /// answerer, or `http(s)://host[:port]` for a `--listen` peer's
    /// `--signal` port, which answers `/`.
    Signal {
        base: String,
        name: Option<String>,
        /// A query to put on every request: `k=<token>`.
        query: Option<String>,
    },
    /// The answerer's record itself: direct mode, nothing is sent.
    Record(Record),
    /// A WebSocket relay leg, as a carrier only (`doc/P2P.md` §4.3).
    Ws(String),
}

impl Peer {
    /// The forms of `doc/P2P.md` §3. A bare word is a file when one of
    /// that name exists, and a host otherwise.
    pub fn parse(s: &str) -> Result<Peer, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("a peer address is empty".into());
        }
        if s.starts_with('{') {
            return Ok(Peer {
                how: How::Record(record(s)?),
                service: None,
            });
        }
        let Some((scheme, rest)) = s.split_once("://") else {
            if Path::new(s).is_file() {
                let text = std::fs::read_to_string(s).map_err(|e| format!("{s}: {e}"))?;
                return Ok(Peer {
                    how: How::Record(record(text.trim())?),
                    service: None,
                });
            }
            return Peer::parse(&format!("drt://{s}"));
        };
        let scheme = scheme.to_ascii_lowercase();
        match scheme.as_str() {
            "drt" => Ok(Peer {
                how: signal(rest, None)?,
                service: None,
            }),
            "http" | "https" => Ok(Peer {
                how: signal(rest, Some(&scheme))?,
                service: None,
            }),
            "ws" | "wss" => Ok(Peer {
                how: How::Ws(s.to_string()),
                service: None,
            }),
            other => match other.strip_prefix("drt+") {
                Some(service) if super::reflect::RESERVED_SERVICES.contains(&service) => {
                    Err(format!(
                        "'{s}' names a reflect server, which is asked and not called: \
                     `drt p2p {s}` alone"
                    ))
                }
                Some(service) if drt_rtc::scope::is_service_name(service) => Ok(Peer {
                    how: signal(rest, None)?,
                    service: Some(service.to_string()),
                }),
                Some(service) => Err(format!(
                    "'{s}': '{service}' cannot name a service; a name is 1 to 32 of a-z, 0-9 \
                     and -, starting with a letter or digit"
                )),
                None => Err(format!(
                    "'{s}': a peer is drt://host[:port]/v1/<name>, drt+<service>://…, a bare \
                     host, an http(s):// URL, a record, a file holding one, or wss:// as a relay"
                )),
            },
        }
    }

    /// The URL the caller's request goes to: `<base>/calls` for a named
    /// answerer, `<base>/` for a peer's own signalling port.
    pub fn calls_url(&self) -> Option<String> {
        match &self.how {
            How::Signal { name: Some(_), .. } => Some(self.url("/calls")),
            How::Signal { name: None, .. } => Some(self.url("/")),
            _ => None,
        }
    }

    /// `<base><path>`, with the address's query on it when it had one.
    pub fn url(&self, path: &str) -> String {
        match &self.how {
            How::Signal { base, query, .. } => match query {
                Some(q) => format!("{base}{path}?{q}"),
                None => format!("{base}{path}"),
            },
            _ => String::new(),
        }
    }

    /// The address as a line on stderr shows it: a URL without its query,
    /// where a `?k=` key lives; a record by its fingerprint only.
    /// One spelling for every form that names the same peer, for whoever
    /// keys something by peer (a launcher's credential scope): a signalling
    /// address as its base with the scheme resolved and no query or
    /// `/calls`; a record as `record:` and its fingerprint; a relay URL
    /// without its key. `drt p2p --show <peer>` prints it, and
    /// `canonicalPeer` in the browser library computes the same.
    pub fn canonical(&self) -> String {
        match &self.how {
            How::Signal { base, .. } => base.clone(),
            How::Record(r) => format!("record:{}", fingerprint_text(&r.fingerprint)),
            How::Ws(url) => url.split('?').next().unwrap_or(url).to_string(),
        }
    }

    pub fn shown(&self) -> String {
        match &self.how {
            How::Signal { base, .. } => crate::tunnel::shown(base),
            How::Record(r) => format!("a record ({})", fingerprint_text(&r.fingerprint)),
            How::Ws(url) => crate::tunnel::shown(url),
        }
    }
}

fn record(text: &str) -> Result<Record, String> {
    Record::decode(text).map_err(|e| format!("the peer's record: {e}"))
}

/// `host[:port][/path]` after a `drt://` or `http(s)://`, as the base of
/// the profile's requests. `drt://` is HTTPS, and HTTP for a loopback
/// address (`doc/P2P.md` §3, §11).
fn signal(rest: &str, scheme: Option<&str>) -> Result<How, String> {
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    };
    let (authority, path) = match authority.split_once('?') {
        // `drt://host?k=…`: a query with no path belongs to the path.
        Some((a, q)) => (a, format!("?{q}")),
        None => (authority, path.to_string()),
    };
    let path = path.as_str();
    let host = authority
        .rsplit_once(':')
        .filter(|(h, p)| {
            !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && !h.ends_with(']')
                || h.starts_with('[')
        })
        .map_or(authority, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() || authority.contains(['@', '#']) {
        return Err(format!("'{rest}' does not start with a host"));
    }
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    let scheme = scheme.unwrap_or(if loopback { "http" } else { "https" });
    // A query (`?k=<token>`) rides on every request made of the base, so a
    // caller token in the URL reaches the server as the profile says.
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (path, None),
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix("/calls").unwrap_or(path);
    let name = match path.strip_prefix("/v1/") {
        Some(name) if !name.is_empty() && !name.contains('/') => Some(name.to_string()),
        _ => None,
    };
    let base = if path.is_empty() {
        format!("{scheme}://{authority}")
    } else {
        format!("{scheme}://{authority}{path}")
    };
    Ok(How::Signal { base, name, query })
}

/// One `-p`: a local port to bind, and what each connection to it asks
/// the far side for. `<local>:<remote>`, `:<remote>` (stdio asks), or
/// `<local>` (whatever the far side forwards to). `<remote>` is a port, a
/// service name, or `host:port` for a peer that serves a scope by address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMap {
    pub local: Option<u16>,
    pub target: Target,
}

impl PortMap {
    pub fn parse(s: &str) -> Result<PortMap, String> {
        let s = s.trim();
        let bad = |why: &str| format!("-p '{s}': {why}");
        let port = |p: &str| p.parse::<u16>().ok().filter(|p| *p != 0);
        let (local, remote) = match s.split_once(':') {
            None => (
                port(s).ok_or_else(|| bad("the local side is not a port"))?,
                "",
            ),
            Some(("", remote)) => {
                if remote.is_empty() {
                    return Err(bad("names neither side"));
                }
                return Ok(PortMap {
                    local: None,
                    target: Target::parse(remote).map_err(|e| bad(&e))?,
                });
            }
            Some((local, remote)) => match port(local) {
                Some(l) => (l, remote),
                None if local.bytes().all(|b| b.is_ascii_digit()) => {
                    return Err(bad("the local side is not a port"))
                }
                // No local port in front: the whole thing is the remote side.
                None => {
                    return Ok(PortMap {
                        local: None,
                        target: Target::parse(s).map_err(|e| bad(&e))?,
                    })
                }
            },
        };
        Ok(PortMap {
            local: Some(local),
            target: Target::parse(remote).map_err(|e| bad(&e))?,
        })
    }
}

/// What a serving side serves: the rows of `doc/P2P.md` §5's table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardSpec {
    /// `--forward` absent: the REPL, through the built-in SSH server.
    Repl,
    /// One TCP target; `ssh://` and the other schemes name a service too.
    One(Entry),
    /// Those ports (`-P`), or every port (`-A`), on that host.
    Ports { host: String, ports: PortSet },
    /// `-`: this process's stdio, one session at a time.
    Stdio,
    /// Another DRT peer: the forwarder calls it and joins the sessions.
    Peer(Peer),
    /// Bare `--forward`: a relay; the caller names the destination.
    Relay,
}

impl ForwardSpec {
    /// `--forward <target>`, with `-P <ports>` and `-A` beside it.
    pub fn parse(
        forward: Option<&str>,
        ports: Option<&str>,
        all: bool,
    ) -> Result<ForwardSpec, String> {
        let set = match (ports, all) {
            (Some(_), true) => return Err("-P and -A both name the ports; use one".into()),
            (Some(list), false) => Some(PortSet::parse(list).map_err(|e| format!("-P {e}"))?),
            (None, true) => Some(PortSet::any()),
            (None, false) => None,
        };
        let only_a_host = |spec: &ForwardSpec| match set {
            Some(_) => Err(format!(
                "-P and -A go with a `--forward <host>` that names no port; {} does",
                spec.describe()
            )),
            None => Ok(()),
        };
        let Some(forward) = forward else {
            let spec = ForwardSpec::Repl;
            if set.is_some() {
                return Err(
                    "-P and -A go with `--forward <host>`, and there is no --forward".into(),
                );
            }
            return Ok(spec);
        };
        let forward = forward.trim();
        if forward.is_empty() {
            let spec = ForwardSpec::Relay;
            only_a_host(&spec)?;
            return Ok(spec);
        }
        if forward == "-" {
            let spec = ForwardSpec::Stdio;
            only_a_host(&spec)?;
            return Ok(spec);
        }
        if let Some((scheme, _)) = forward.split_once("://") {
            let scheme = scheme.to_ascii_lowercase();
            if scheme == "drt" || scheme.starts_with("drt+") {
                let spec =
                    ForwardSpec::Peer(Peer::parse(forward).map_err(|e| format!("--forward {e}"))?);
                only_a_host(&spec)?;
                return Ok(spec);
            }
            if super::reflect::RESERVED_SERVICES.contains(&scheme.as_str()) {
                return Err(format!(
                    "--forward {forward}: `{scheme}` is reserved for reflect servers, and names no service"
                ));
            }
            let entry = Entry::parse(forward).map_err(|e| format!("--forward: {e}"))?;
            let spec = ForwardSpec::One(entry);
            only_a_host(&spec)?;
            return Ok(spec);
        }
        // `host:port` is one target; `host` alone needs -P or -A.
        let host_port = forward
            .rsplit_once(':')
            .filter(|(h, _)| !h.ends_with(']') || h.starts_with('['))
            .and_then(|(h, p)| p.parse::<u16>().ok().filter(|p| *p != 0).map(|p| (h, p)));
        match host_port {
            Some((host, port)) => {
                let host = host.trim_start_matches('[').trim_end_matches(']');
                if host.is_empty() {
                    return Err(format!("--forward '{forward}' has no host"));
                }
                let spec = ForwardSpec::One(Entry {
                    scheme: "tcp".into(),
                    host: host.to_string(),
                    port,
                });
                only_a_host(&spec)?;
                Ok(spec)
            }
            None => {
                let host = forward.trim_start_matches('[').trim_end_matches(']');
                if host.contains(['/', '?', '#', '@', ' ']) {
                    return Err(format!(
                        "--forward '{forward}' is not a host, host:port, scheme://host[:port], \
                         drt://…, or -"
                    ));
                }
                match set {
                    Some(ports) => Ok(ForwardSpec::Ports {
                        host: host.to_string(),
                        ports,
                    }),
                    None => Err(format!(
                        "--forward {forward} names a host and no port: add -P <ports> for some of \
                         its ports, -A for every port, or :<port> for one"
                    )),
                }
            }
        }
    }

    /// What is served, for the lines this verb prints.
    pub fn describe(&self) -> String {
        match self {
            ForwardSpec::Repl => "the REPL, as the service ssh".into(),
            ForwardSpec::One(e) if e.scheme == "tcp" => format!("{}:{}", e.host, e.port),
            ForwardSpec::One(e) => format!("{} (the service {})", e.url(), e.scheme),
            ForwardSpec::Ports { host, ports } => format!("{host}, {ports}"),
            ForwardSpec::Stdio => "this process's stdio, one session at a time".into(),
            ForwardSpec::Peer(p) => format!("the peer {}", p.shown()),
            ForwardSpec::Relay => "whatever the caller names (a relay)".into(),
        }
    }
}

/// `--host`: an address to bind, or a range to admit (`doc/P2P.md` §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostSpec {
    Addr(IpAddr),
    Range(Cidr),
}

impl HostSpec {
    pub fn parse(s: &str) -> Result<HostSpec, String> {
        let s = s.trim().trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(HostSpec::Addr(ip));
        }
        if s.contains('/') {
            let range = Cidr::parse(s).map_err(|e| format!("--host {e}"))?;
            return Ok(if range.is_single() {
                HostSpec::Addr(range.ip)
            } else {
                HostSpec::Range(range)
            });
        }
        Err(format!(
            "--host '{s}' is not an address or a CIDR range; a name is not bound here, since \
             which of its addresses is meant is not a guess this side makes"
        ))
    }

    /// The address to bind: the address itself, or, for a range, this
    /// machine's own address inside it when the routing table names one,
    /// and everywhere otherwise.
    pub fn bind_ip(&self) -> IpAddr {
        match self {
            HostSpec::Addr(ip) => *ip,
            HostSpec::Range(range) => {
                let probe = std::net::SocketAddr::new(range.ip, 9);
                let own = std::net::UdpSocket::bind(if range.ip.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                })
                .and_then(|s| s.connect(probe).and_then(|_| s.local_addr()))
                .map(|a| a.ip())
                .ok()
                .filter(|ip| range.contains(*ip));
                own.unwrap_or(if range.ip.is_ipv4() {
                    IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
                } else {
                    IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
                })
            }
        }
    }

    /// Who is admitted: everyone for an address (the bind decides), the
    /// range for a range.
    pub fn accept(&self) -> Vec<Cidr> {
        match self {
            HostSpec::Addr(_) => Vec::new(),
            HostSpec::Range(range) => vec![*range],
        }
    }
}

/// `SHA256:<base64>` as OpenSSH prints one, or the colon-separated hex of
/// an SDP `a=fingerprint` line, either way the SHA-256 of the answerer's
/// DTLS certificate.
pub fn fingerprint(s: &str) -> Result<[u8; 32], String> {
    use base64::Engine;
    let s = s.trim();
    let bytes = if let Some(b64) = s
        .strip_prefix("SHA256:")
        .or_else(|| s.strip_prefix("sha256:"))
    {
        let b64 = b64.trim_end_matches('=');
        base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(b64)
            .map_err(|_| format!("--fingerprint '{s}': not base64 after SHA256:"))?
    } else {
        let hex: String = s.chars().filter(|c| *c != ':').collect();
        if hex.len() != 64 {
            return Err(format!(
                "--fingerprint '{s}': SHA256:<base64> or 32 hex bytes, as the listening side prints it"
            ));
        }
        (0..32)
            .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .map_err(|_| format!("--fingerprint '{s}': not hex"))?
    };
    bytes
        .try_into()
        .map_err(|_| format!("--fingerprint '{s}': not a 32-byte digest"))
}

/// A fingerprint as this verb prints one, and as `--fingerprint` takes it.
pub fn fingerprint_text(digest: &[u8; 32]) -> String {
    use base64::Engine;
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spelling_of_a_peer_has_one_canonical_form() {
        let c = |s: &str| Peer::parse(s).unwrap().canonical();
        assert_eq!(
            c("drt://signal.example/v1/mypc"),
            "https://signal.example/v1/mypc"
        );
        assert_eq!(
            c("drt+ssh://signal.example/v1/mypc/"),
            "https://signal.example/v1/mypc"
        );
        assert_eq!(
            c("https://signal.example/v1/mypc/calls?k=tok"),
            "https://signal.example/v1/mypc"
        );
        assert_eq!(c("signal.example"), "https://signal.example");
        assert_eq!(c("drt://127.0.0.1:5001"), "http://127.0.0.1:5001");
        assert_eq!(
            c("wss://relay.example/s/xps?k=1"),
            "wss://relay.example/s/xps"
        );
    }

    #[test]
    fn a_peer_address_is_read_as_section_3_says() {
        let signal = |s: &str| match Peer::parse(s).unwrap().how {
            How::Signal { base, name, .. } => (base, name),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            signal("drt://signal.example/v1/mypc"),
            ("https://signal.example/v1/mypc".into(), Some("mypc".into()))
        );
        assert_eq!(
            signal("drt://127.0.0.1:5001"),
            ("http://127.0.0.1:5001".into(), None)
        );
        assert_eq!(
            signal("drt://localhost:5001/"),
            ("http://localhost:5001".into(), None)
        );
        assert_eq!(
            signal("https://signal.example/v1/mypc/calls"),
            ("https://signal.example/v1/mypc".into(), Some("mypc".into()))
        );
        assert_eq!(
            signal("http://10.9.0.5:5001/v1/box"),
            ("http://10.9.0.5:5001/v1/box".into(), Some("box".into()))
        );
        assert_eq!(
            signal("signal.example/v1/mypc"),
            ("https://signal.example/v1/mypc".into(), Some("mypc".into()))
        );
        let p = Peer::parse("drt+ssh://signal.example/v1/mypc").unwrap();
        assert_eq!(p.service.as_deref(), Some("ssh"));
        assert_eq!(
            p.calls_url().as_deref(),
            Some("https://signal.example/v1/mypc/calls")
        );
        assert_eq!(
            Peer::parse("drt://127.0.0.1:5001")
                .unwrap()
                .calls_url()
                .as_deref(),
            Some("http://127.0.0.1:5001/")
        );
        assert_eq!(
            Peer::parse("DRT+SSH://box.lan/v1/a")
                .unwrap()
                .service
                .as_deref(),
            Some("ssh"),
            "a scheme is case-insensitive"
        );
        assert!(matches!(
            Peer::parse("wss://relay.example/s/xps?k=1").unwrap().how,
            How::Ws(_)
        ));
        // A query rides on every request: the profile's `?k=<token>`.
        let keyed = Peer::parse("http://127.0.0.1:18495/v1/page/calls?k=tok").unwrap();
        assert_eq!(
            keyed.calls_url().as_deref(),
            Some("http://127.0.0.1:18495/v1/page/calls?k=tok")
        );
        assert_eq!(
            keyed.url("/events"),
            "http://127.0.0.1:18495/v1/page/events?k=tok"
        );
        assert_eq!(
            Peer::parse("drt://signal.example?k=tok")
                .unwrap()
                .calls_url()
                .as_deref(),
            Some("https://signal.example/?k=tok")
        );
        let record = r#"{"v":1,"u":"abcd","p":"0123456789abcdefghijKL","f":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","c":[]}"#;
        assert!(matches!(Peer::parse(record).unwrap().how, How::Record(_)));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("box.record.json");
        std::fs::write(&file, record).unwrap();
        assert!(matches!(
            Peer::parse(file.to_str().unwrap()).unwrap().how,
            How::Record(_)
        ));
        for bad in [
            "drt+Not a name://x/v1/a",
            "ftp://x",
            "drt://",
            "",
            "drt://user@host/v1/a",
        ] {
            assert!(Peer::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_port_map_is_local_remote_or_either_alone() {
        let map = |s: &str| PortMap::parse(s).unwrap();
        assert_eq!(
            map("8080:80"),
            PortMap {
                local: Some(8080),
                target: Target::Port(80)
            }
        );
        assert_eq!(
            map(":22"),
            PortMap {
                local: None,
                target: Target::Port(22)
            }
        );
        assert_eq!(
            map(":ssh"),
            PortMap {
                local: None,
                target: Target::Service("ssh".into())
            }
        );
        assert_eq!(
            map("2222"),
            PortMap {
                local: Some(2222),
                target: Target::Default
            }
        );
        assert_eq!(
            map("2222:127.0.0.1:22"),
            PortMap {
                local: Some(2222),
                target: Target::Address {
                    host: "127.0.0.1".into(),
                    port: 22
                }
            }
        );
        assert_eq!(
            map("box.lan:22"),
            PortMap {
                local: None,
                target: Target::Address {
                    host: "box.lan".into(),
                    port: 22
                }
            }
        );
        for bad in [":", "x", "0:80", "80:0", "", "a:b:c:d"] {
            assert!(PortMap::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_forward_is_one_row_of_section_5s_table() {
        let f = |fw: Option<&str>, p: Option<&str>, a: bool| ForwardSpec::parse(fw, p, a);
        assert_eq!(f(None, None, false), Ok(ForwardSpec::Repl));
        assert_eq!(
            f(Some("127.0.0.1:8080"), None, false),
            Ok(ForwardSpec::One(Entry {
                scheme: "tcp".into(),
                host: "127.0.0.1".into(),
                port: 8080
            }))
        );
        assert_eq!(
            f(Some("ssh://127.0.0.1:22"), None, false),
            Ok(ForwardSpec::One(
                Entry::parse("ssh://127.0.0.1:22").unwrap()
            ))
        );
        assert_eq!(
            f(Some("127.0.0.1"), Some("80,8080:8090"), false),
            Ok(ForwardSpec::Ports {
                host: "127.0.0.1".into(),
                ports: PortSet::parse("80,8080:8090").unwrap()
            })
        );
        assert_eq!(
            f(Some("127.0.0.1"), None, true),
            Ok(ForwardSpec::Ports {
                host: "127.0.0.1".into(),
                ports: PortSet::any()
            })
        );
        assert_eq!(f(Some("-"), None, false), Ok(ForwardSpec::Stdio));
        assert_eq!(f(Some(""), None, false), Ok(ForwardSpec::Relay));
        assert!(matches!(
            f(Some("drt://s.example/v1/b"), None, false),
            Ok(ForwardSpec::Peer(_))
        ));
        let e = f(Some("127.0.0.1"), None, false).unwrap_err();
        assert!(e.contains("-P") && e.contains("-A"), "{e}");
        assert!(f(Some("127.0.0.1:22"), Some("80"), false).is_err());
        assert!(f(None, Some("80"), false).is_err());
        assert!(f(Some("127.0.0.1"), Some("80"), true).is_err());
        assert!(f(Some("127.0.0.1"), Some("80-90"), false).is_err());
    }

    #[test]
    fn a_host_is_an_address_or_a_range() {
        assert_eq!(
            HostSpec::parse("0.0.0.0"),
            Ok(HostSpec::Addr("0.0.0.0".parse().unwrap()))
        );
        assert_eq!(
            HostSpec::parse("[::1]"),
            Ok(HostSpec::Addr("::1".parse().unwrap()))
        );
        assert_eq!(
            HostSpec::parse("10.9.0.0/24"),
            Ok(HostSpec::Range(Cidr::parse("10.9.0.0/24").unwrap()))
        );
        assert_eq!(
            HostSpec::parse("10.9.0.5/32"),
            Ok(HostSpec::Addr("10.9.0.5".parse().unwrap()))
        );
        assert!(HostSpec::parse("box.lan").is_err());
        let range = HostSpec::parse("10.9.0.0/24").unwrap();
        assert_eq!(range.accept().len(), 1);
        assert!(HostSpec::parse("127.0.0.1").unwrap().accept().is_empty());
    }

    #[test]
    fn a_fingerprint_reads_both_spellings_and_prints_one() {
        let digest: [u8; 32] = (0..32)
            .map(|i| i as u8 * 7)
            .collect::<Vec<u8>>()
            .try_into()
            .unwrap();
        let text = fingerprint_text(&digest);
        assert!(
            text.starts_with("SHA256:") && !text.ends_with('='),
            "{text}"
        );
        assert_eq!(fingerprint(&text).unwrap(), digest);
        assert_eq!(
            fingerprint(&format!("{text}=")).unwrap(),
            digest,
            "padding is allowed"
        );
        assert_eq!(
            fingerprint(&drt_rtc::record::fingerprint_hex(&digest)).unwrap(),
            digest
        );
        assert!(fingerprint("SHA256:nope").is_err());
        assert!(fingerprint("ab:cd").is_err());
    }
}

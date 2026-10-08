//! The asking side: `drt p2p drt+stun://<location>` and
//! `drt p2p drt+reflect://<location>` (`doc/Reflect.md`, Asking).
//!
//! It prints what the gates answered and attaches no verdict. One location
//! is enough: the first answer names the other gate in OTHER-ADDRESS, and
//! the checks that need it are sent from the same socket.
//!
//! ## surface block
//!
//! - Entry points: [`Location::parse`]; [`run`], ask and print; [`ask`],
//!   the [`Report`] alone; [`render_text`] and [`render_json`].
//! - Configurable: [`ATTEMPTS`], [`FIRST_WAIT`], [`TCP_WAIT`],
//!   [`TOKEN_WAIT`], [`MAX_PORTS`].
//! - Fan-out: [`Scheme`], what each scheme runs; the checks, one function
//!   each, in the order [`ask`] runs them: [`udp_binding`],
//!   [`tcp_binding`], [`mapping`], [`filtering`], [`cross`].

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use ego_transport::stun::{
    decode, decode_rfc5780, encode_binding_request_with, ChangeRequest, StunMessage, TransactionId,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::timeout;

use super::serve::{read_message, PEER_TIMEOUT};
use super::{wire, Capability, Code, DEFAULT_PORT};

/// UDP sends per request before it counts as unanswered.
pub const ATTEMPTS: u32 = 3;
/// The wait after the first UDP send, doubled after each (RFC 5389).
pub const FIRST_WAIT: Duration = Duration::from_millis(500);
/// How long a TCP connection and its answer may take.
pub const TCP_WAIT: Duration = Duration::from_millis(3500);
/// How long the listener waits for the token after `connected`.
pub const TOKEN_WAIT: Duration = Duration::from_secs(2);
/// Cross requests one run makes; each counts against the server's rate.
pub const MAX_PORTS: usize = 4;

/// Which checks a scheme runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// `drt+stun://`: the UDP checks only.
    Stun,
    /// `drt+reflect://`: every check the server offers.
    Reflect,
}

/// `drt+stun://host[:port]` or `drt+reflect://host[:port]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
}

impl Location {
    /// `None` when `s` is not one of the two schemes, so a caller can try
    /// it as a peer instead.
    pub fn parse(s: &str) -> Option<Result<Location, String>> {
        let (scheme, rest) = s.trim().split_once("://")?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "drt+stun" => Scheme::Stun,
            "drt+reflect" => Scheme::Reflect,
            _ => return None,
        };
        let authority = rest.trim_end_matches('/');
        Some(Location::authority(scheme, authority).map_err(|e| format!("'{s}': {e}")))
    }

    fn authority(scheme: Scheme, authority: &str) -> Result<Location, String> {
        if authority.is_empty() || authority.contains('/') {
            return Err("a reflect location is host[:port], with no path".into());
        }
        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            let (host, after) = rest.split_once(']').ok_or("an IPv6 address needs its ]")?;
            match after.strip_prefix(':') {
                Some(p) => (host, Some(p)),
                None if after.is_empty() => (host, None),
                None => return Err("text after the ]".into()),
            }
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        let port = match port {
            Some(p) => p
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| format!("'{p}' is not a port"))?,
            None => DEFAULT_PORT,
        };
        if host.is_empty() {
            return Err("no host".into());
        }
        Ok(Location {
            scheme,
            host: host.to_string(),
            port,
        })
    }

    pub fn shown(&self) -> String {
        let scheme = match self.scheme {
            Scheme::Stun => "drt+stun",
            Scheme::Reflect => "drt+reflect",
        };
        if self.host.contains(':') {
            format!("{scheme}://[{}]:{}", self.host, self.port)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }
}

/// One check's outcome: a [`Code`], and what the gates said beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check<T> {
    pub code: Code,
    pub answer: Option<T>,
}

impl<T> Check<T> {
    fn ok(answer: T) -> Check<T> {
        Check {
            code: Code::Ok,
            answer: Some(answer),
        }
    }

    fn not(code: Code) -> Check<T> {
        Check { code, answer: None }
    }
}

/// A binding answer: the address the server saw, from whom, and from what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub mapped: SocketAddr,
    pub local: SocketAddr,
    pub from: SocketAddr,
    pub rtt_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingAnswer {
    /// `endpoint_independent`, `endpoint_dependent`, or `none` when the
    /// mapped address is the local one.
    pub kind: &'static str,
    pub other: Binding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteringAnswer {
    /// RFC 4787's name: `endpoint_independent`, `address_dependent`,
    /// `address_and_port_dependent`, or `address_dependent_or_stricter`
    /// when the server offers no change-port test.
    pub kind: &'static str,
    pub change_ip_reply: Option<SocketAddr>,
    pub change_port_reply: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossAnswer {
    pub port: u16,
    /// `received`, `not_received`, or `not_listening` when this side could
    /// not listen on the port.
    pub token: &'static str,
}

/// Everything one run learned, and nothing derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub location: String,
    pub server: Option<SocketAddr>,
    /// `None` when no answer carried them: not a reflect server, or none
    /// answered.
    pub capabilities: Option<Vec<String>>,
    pub other_address: Option<SocketAddr>,
    pub udp: Check<Binding>,
    pub mapping: Check<MappingAnswer>,
    pub filtering: Check<FilteringAnswer>,
    /// Absent for `drt+stun://`.
    pub tcp: Option<Check<Binding>>,
    pub cross: Vec<Check<CrossAnswer>>,
}

impl Report {
    /// Whether any gate answered at all.
    pub fn answered(&self) -> bool {
        self.udp.code == Code::Ok || self.tcp.as_ref().is_some_and(|t| t.code == Code::Ok)
    }

    fn offers(&self, c: Capability) -> bool {
        self.capabilities
            .as_ref()
            .is_some_and(|caps| caps.iter().any(|n| n == c.name()))
    }

    /// Why a check that needs the peer gate cannot run: the server has
    /// none, or is not a reflect server and does not say.
    fn without_peer(&self) -> Code {
        if self.capabilities.is_some() {
            Code::NoPeer
        } else {
            Code::NotOffered
        }
    }
}

/// Ask, print, and fail only when nothing answered.
pub async fn run(location: &Location, ports: &[u16], json: bool) -> Result<(), String> {
    let report = ask(location, ports).await?;
    if json {
        println!("{}", render_json(&report));
    } else {
        print!("{}", render_text(&report));
    }
    if report.answered() {
        Ok(())
    } else {
        Err(format!("{}: no answer over UDP or TCP", location.shown()))
    }
}

/// Every check the scheme runs and the server offers, in order.
pub async fn ask(location: &Location, ports: &[u16]) -> Result<Report, String> {
    if ports.len() > MAX_PORTS {
        return Err(format!(
            "--port names {} ports; a run asks about {MAX_PORTS} at most",
            ports.len()
        ));
    }
    let server = tokio::net::lookup_host((location.host.as_str(), location.port))
        .await
        .ok()
        .and_then(|mut a| a.next())
        .map(|a| SocketAddr::new(a.ip().to_canonical(), a.port()))
        .ok_or_else(|| format!("{}: {}", location.shown(), Code::Unresolved.name()))?;
    let socket = UdpSocket::bind(unspecified(server))
        .await
        .map_err(|e| format!("a UDP socket: {e}"))?;
    let mut report = Report {
        location: location.shown(),
        server: Some(server),
        capabilities: None,
        other_address: None,
        udp: Check::not(Code::UdpBlocked),
        mapping: Check::not(Code::UdpBlocked),
        filtering: Check::not(Code::UdpBlocked),
        tcp: None,
        cross: Vec::new(),
    };

    let udp = udp_binding(&socket, server).await;
    if let Ok((binding, attrs)) = &udp {
        report.capabilities = capabilities(attrs);
        report.other_address = decode_rfc5780(attrs).ok().and_then(|a| a.other_address);
        report.udp = Check::ok(binding.clone());
    }
    if location.scheme == Scheme::Reflect {
        let tcp = tcp_binding(server).await;
        report.tcp = Some(match tcp {
            Ok((binding, attrs)) => {
                if report.capabilities.is_none() {
                    report.capabilities = capabilities(&attrs);
                }
                Check::ok(binding)
            }
            Err(code) => Check::not(code),
        });
    }
    if let Ok((first, _)) = &udp {
        report.mapping = match report.other_address {
            Some(other) => mapping(&socket, first, other).await,
            None => Check::not(report.without_peer()),
        };
        let rfc5780 = report.capabilities.is_none() && report.other_address.is_some();
        report.filtering = if report.offers(Capability::Filtering) || rfc5780 {
            filtering(&socket, server).await
        } else if report.capabilities.is_some() {
            Check::not(if report.offers(Capability::Peer) {
                Code::NotOffered
            } else {
                Code::NoPeer
            })
        } else {
            Check::not(Code::NotOffered)
        };
    }
    if location.scheme == Scheme::Reflect {
        let tcp_ok = report.tcp.as_ref().is_some_and(|t| t.code == Code::Ok);
        let asks: Vec<Option<u16>> = if ports.is_empty() {
            vec![None]
        } else {
            ports.iter().copied().map(Some).collect()
        };
        for port in asks {
            report.cross.push(if !tcp_ok {
                Check::not(Code::TcpBlocked)
            } else if report.offers(Capability::Cross) {
                cross(server, port).await
            } else {
                Check::not(report.without_peer())
            });
        }
    }
    Ok(report)
}

// depth: the checks

fn unspecified(server: SocketAddr) -> SocketAddr {
    let ip = if server.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    SocketAddr::new(ip, 0)
}

/// The address this machine sends from toward `server`: a routing lookup,
/// with nothing sent. A wildcard socket's own address says `0.0.0.0`.
fn source_toward(server: SocketAddr) -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind(unspecified(server)).ok()?;
    s.connect(server).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

fn capabilities(message: &[u8]) -> Option<Vec<String>> {
    let value = wire::find(message, wire::CAPABILITIES)?;
    let text = std::str::from_utf8(value).ok()?;
    Some(
        text.split(',')
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn result_code(message: &[u8]) -> Option<Code> {
    std::str::from_utf8(wire::find(message, wire::RESULT)?)
        .ok()
        .and_then(Code::parse)
}

/// One UDP transaction from `socket`. A change request's answer may come
/// from anywhere, so any source is taken for it; the transaction id is
/// what authenticates it. `Err(Code::Timeout)` when nothing came.
async fn transact(
    socket: &UdpSocket,
    to: SocketAddr,
    change: ChangeRequest,
) -> Result<(Binding, Vec<u8>), Code> {
    let txid = TransactionId::random();
    let request = encode_binding_request_with(&txid, change);
    let local_ip = source_toward(to);
    let mut wait = FIRST_WAIT;
    let mut buf = [0u8; 1500];
    for _ in 0..ATTEMPTS {
        socket
            .send_to(&request, to)
            .await
            .map_err(|_| Code::UdpBlocked)?;
        let sent = Instant::now();
        let deadline = sent + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(Ok((n, from))) = timeout(left, socket.recv_from(&mut buf)).await else {
                break;
            };
            let from = SocketAddr::new(from.ip().to_canonical(), from.port());
            if change.is_none() && from != to {
                continue;
            }
            match decode(&buf[..n]) {
                Ok(StunMessage::BindingSuccess { txid: got, mapped }) if got == txid => {
                    let local = socket.local_addr().map_err(|_| Code::UdpBlocked)?;
                    let local = SocketAddr::new(local_ip.unwrap_or(local.ip()), local.port());
                    let binding = Binding {
                        mapped,
                        local,
                        from,
                        rtt_ms: sent.elapsed().as_millis() as u64,
                    };
                    return Ok((binding, buf[..n].to_vec()));
                }
                Ok(StunMessage::BindingError {
                    txid: got, code, ..
                }) if got == txid => {
                    return Err(result_code(&buf[..n]).unwrap_or(if code == 420 {
                        Code::NotOffered
                    } else {
                        Code::BadRequest
                    }));
                }
                _ => continue,
            }
        }
        wait *= 2;
    }
    Err(Code::Timeout)
}

/// The mapped address over UDP, with the server's capabilities and
/// OTHER-ADDRESS.
pub async fn udp_binding(
    socket: &UdpSocket,
    server: SocketAddr,
) -> Result<(Binding, Vec<u8>), Code> {
    transact(socket, server, ChangeRequest::NONE)
        .await
        .map_err(|code| {
            if code == Code::Timeout {
                Code::UdpBlocked
            } else {
                code
            }
        })
}

/// The observed address of a TCP connection to the server.
pub async fn tcp_binding(server: SocketAddr) -> Result<(Binding, Vec<u8>), Code> {
    let txid = TransactionId::random();
    let exchange = async {
        let mut stream = TcpStream::connect(server)
            .await
            .map_err(|_| Code::TcpBlocked)?;
        let local = stream.local_addr().map_err(|_| Code::TcpBlocked)?;
        let sent = Instant::now();
        stream
            .write_all(&encode_binding_request_with(&txid, ChangeRequest::NONE))
            .await
            .map_err(|_| Code::TcpBlocked)?;
        let answer = read_message(&mut stream).await.ok_or(Code::TcpBlocked)?;
        match decode(&answer) {
            Ok(StunMessage::BindingSuccess { txid: got, mapped }) if got == txid => Ok((
                Binding {
                    mapped,
                    local,
                    from: server,
                    rtt_ms: sent.elapsed().as_millis() as u64,
                },
                answer,
            )),
            _ => Err(result_code(&answer).unwrap_or(Code::BadRequest)),
        }
    };
    timeout(TCP_WAIT, exchange)
        .await
        .unwrap_or(Err(Code::TcpBlocked))
}

/// The same socket asks the other gate: one mapped port for both
/// destinations is an endpoint-independent mapping.
pub async fn mapping(
    socket: &UdpSocket,
    first: &Binding,
    other: SocketAddr,
) -> Check<MappingAnswer> {
    match transact(socket, other, ChangeRequest::NONE).await {
        Ok((second, _)) => {
            let kind = if second.mapped != first.mapped {
                "endpoint_dependent"
            } else if first.mapped == first.local {
                "none"
            } else {
                "endpoint_independent"
            };
            Check::ok(MappingAnswer {
                kind,
                other: second,
            })
        }
        Err(Code::Timeout) => Check::not(Code::PeerUnreachable),
        Err(code) => Check::not(code),
    }
}

/// RFC 5780 §4.4 from the same socket: an answer from the other address
/// means anyone gets in; failing that, one from the other port tells the
/// two stricter kinds apart, and a server with no alternate port leaves
/// them together.
pub async fn filtering(socket: &UdpSocket, server: SocketAddr) -> Check<FilteringAnswer> {
    let mut answer = FilteringAnswer {
        kind: "endpoint_independent",
        change_ip_reply: None,
        change_port_reply: None,
    };
    match transact(socket, server, ChangeRequest::IP_AND_PORT).await {
        Ok((reply, _)) if reply.from.ip() != server.ip() => {
            answer.change_ip_reply = Some(reply.from);
            return Check::ok(answer);
        }
        // An answer from the address asked is a server that ignored the
        // change, or a pair on one address: no evidence either way.
        Ok(_) => return Check::not(Code::SameAddress),
        Err(Code::Timeout) => {}
        Err(code) => return Check::not(code),
    }
    answer.kind = match transact(socket, server, ChangeRequest::PORT).await {
        Ok((reply, _)) if reply.from != server => {
            answer.change_port_reply = Some(reply.from);
            "address_dependent"
        }
        Ok(_) => return Check::not(Code::SameAddress),
        Err(Code::Timeout) => "address_and_port_dependent",
        Err(Code::NotOffered) => "address_dependent_or_stricter",
        Err(code) => return Check::not(code),
    };
    Check::ok(answer)
}

/// Listen on the port (one the system picks when none is named), and ask
/// the server to have the other gate connect to it.
pub async fn cross(server: SocketAddr, port: Option<u16>) -> Check<CrossAnswer> {
    let listener =
        TcpListener::bind(SocketAddr::new(unspecified(server).ip(), port.unwrap_or(0))).await;
    let port = match (&listener, port) {
        (Ok(l), _) => l.local_addr().map(|a| a.port()).unwrap_or(0),
        (Err(_), Some(p)) => p,
        (Err(_), None) => return Check::not(Code::BadRequest),
    };
    let token = wire::random16();
    let heard = listener.ok().map(|l| {
        tokio::spawn(async move {
            let wait = PEER_TIMEOUT + TCP_WAIT + TOKEN_WAIT;
            let read = async {
                loop {
                    let Ok((mut s, _)) = l.accept().await else {
                        return false;
                    };
                    let mut got = [0u8; 16];
                    if timeout(TOKEN_WAIT, s.read_exact(&mut got))
                        .await
                        .is_ok_and(|r| r.is_ok())
                        && got == token
                    {
                        return true;
                    }
                }
            };
            timeout(wait, read).await.unwrap_or(false)
        })
    });

    let [hi, lo] = port.to_be_bytes();
    let request = wire::append(
        encode_binding_request_with(&TransactionId::random(), ChangeRequest::NONE),
        &[
            (wire::CROSS_PORT, &[hi, lo, 0, 0]),
            (wire::CROSS_TOKEN, &token),
        ],
    );
    let exchange = async {
        let mut stream = TcpStream::connect(server).await.ok()?;
        stream.write_all(&request).await.ok()?;
        read_message(&mut stream).await
    };
    let code = match timeout(PEER_TIMEOUT + TCP_WAIT, exchange).await {
        Ok(Some(answer)) => result_code(&answer).unwrap_or(Code::BadRequest),
        _ => Code::TcpBlocked,
    };
    let token = match heard {
        None => "not_listening",
        Some(task) if code == Code::Connected => match timeout(TOKEN_WAIT, task).await {
            Ok(Ok(true)) => "received",
            _ => "not_received",
        },
        Some(task) => {
            task.abort();
            "not_received"
        }
    };
    Check {
        code,
        answer: Some(CrossAnswer { port, token }),
    }
}

// depth: rendering

fn addr(a: Option<SocketAddr>) -> String {
    a.map(|a| a.to_string()).unwrap_or_else(|| "-".into())
}

/// One line per check: what was asked, the code, and the answer beside it.
pub fn render_text(r: &Report) -> String {
    let mut out = String::new();
    let mut line = |label: &str, text: String| out.push_str(&format!("{label:<10} {text}\n"));
    line("location", r.location.clone());
    line("server", addr(r.server));
    line(
        "offers",
        match &r.capabilities {
            Some(c) => c.join(","),
            None => "- (not a reflect server, or no answer)".into(),
        },
    );
    if let Some(other) = r.other_address {
        line("other", other.to_string());
    }
    let binding = |c: &Check<Binding>| match &c.answer {
        Some(b) => format!(
            "{} mapped {} local {} {} ms",
            c.code.name(),
            b.mapped,
            b.local,
            b.rtt_ms
        ),
        None => c.code.name().to_string(),
    };
    line("udp", binding(&r.udp));
    line(
        "mapping",
        match &r.mapping.answer {
            Some(m) => format!(
                "{} {} (other gate saw {})",
                r.mapping.code.name(),
                m.kind,
                m.other.mapped
            ),
            None => r.mapping.code.name().to_string(),
        },
    );
    line(
        "filtering",
        match &r.filtering.answer {
            Some(f) => format!("{} {}", r.filtering.code.name(), f.kind),
            None => r.filtering.code.name().to_string(),
        },
    );
    if let Some(tcp) = &r.tcp {
        line("tcp", binding(tcp));
    }
    for c in &r.cross {
        line(
            "cross",
            match &c.answer {
                Some(a) => format!("port {} {} token {}", a.port, c.code.name(), a.token),
                None => c.code.name().to_string(),
            },
        );
    }
    out
}

/// The same, as one JSON object: every check has a `code`, and its answer
/// beside it when there is one.
pub fn render_json(r: &Report) -> String {
    let binding = |c: &Check<Binding>| match &c.answer {
        Some(b) => json!({"code": c.code.name(), "mapped": b.mapped.to_string(),
            "local": b.local.to_string(), "from": b.from.to_string(), "rtt_ms": b.rtt_ms}),
        None => json!({"code": c.code.name()}),
    };
    let doc = json!({
        "location": r.location,
        "server": r.server.map(|a| a.to_string()),
        "capabilities": r.capabilities,
        "other_address": r.other_address.map(|a| a.to_string()),
        "udp": binding(&r.udp),
        "mapping": match &r.mapping.answer {
            Some(m) => json!({"code": r.mapping.code.name(), "mapping": m.kind,
                "other": binding(&Check::ok(m.other.clone()))}),
            None => json!({"code": r.mapping.code.name()}),
        },
        "filtering": match &r.filtering.answer {
            Some(f) => json!({"code": r.filtering.code.name(), "filtering": f.kind,
                "change_ip_reply": f.change_ip_reply.map(|a| a.to_string()),
                "change_port_reply": f.change_port_reply.map(|a| a.to_string())}),
            None => json!({"code": r.filtering.code.name()}),
        },
        "tcp": r.tcp.as_ref().map(binding),
        "cross": r.cross.iter().map(|c| match &c.answer {
            Some(a) => json!({"code": c.code.name(), "port": a.port, "token": a.token}),
            None => json!({"code": c.code.name()}),
        }).collect::<Vec<_>>(),
    });
    doc.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::peer::HostSpec;
    use crate::p2p::reflect::serve::Gate;
    use crate::p2p::reflect::ReflectRole;

    #[test]
    fn a_location_is_either_scheme_with_the_stun_port_by_default() {
        let l = Location::parse("drt+reflect://reflect.example")
            .unwrap()
            .unwrap();
        assert_eq!(
            (l.scheme, l.host.as_str(), l.port),
            (Scheme::Reflect, "reflect.example", 3478)
        );
        let l = Location::parse("drt+stun://[2001:db8::1]:4000/")
            .unwrap()
            .unwrap();
        assert_eq!(
            (l.scheme, l.host.as_str(), l.port),
            (Scheme::Stun, "2001:db8::1", 4000)
        );
        assert_eq!(l.shown(), "drt+stun://[2001:db8::1]:4000");
        assert!(Location::parse("drt+ssh://x").is_none());
        assert!(Location::parse("drt+stun://x/v1/a").unwrap().is_err());
        assert!(Location::parse("drt+stun://x:0").unwrap().is_err());
    }

    async fn gate(hosts: &str, peers: Vec<String>, key: Option<&str>) -> SocketAddr {
        let g = Gate::bind(&ReflectRole {
            port: 0,
            hosts: vec![HostSpec::parse(hosts).unwrap()],
            peers,
            key: key.map(|k| k.as_bytes().to_vec()),
            rate: 30,
        })
        .await
        .unwrap();
        let at = g.addrs()[0];
        tokio::spawn(g.serve());
        at
    }

    fn at(a: SocketAddr, scheme: Scheme) -> Location {
        Location {
            scheme,
            host: a.ip().to_string(),
            port: a.port(),
        }
    }

    #[tokio::test]
    async fn a_single_gate_answers_the_single_gate_checks_and_says_why_not_the_rest() {
        let a = gate("127.0.0.1", vec![], None).await;
        let r = ask(&at(a, Scheme::Reflect), &[]).await.unwrap();
        assert_eq!(r.capabilities, Some(vec!["udp".into(), "tcp".into()]));
        assert_eq!(r.udp.code, Code::Ok);
        assert_eq!(r.tcp.as_ref().unwrap().code, Code::Ok);
        assert_eq!(r.mapping.code, Code::NoPeer);
        assert_eq!(r.filtering.code, Code::NoPeer);
        assert_eq!(r.cross.len(), 1);
        assert_eq!(r.cross[0].code, Code::NoPeer);

        let r = ask(&at(a, Scheme::Stun), &[]).await.unwrap();
        assert!(r.tcp.is_none() && r.cross.is_empty());
    }

    #[tokio::test]
    async fn a_pair_reaches_a_port_this_side_listens_on() {
        let b = gate("127.0.0.1", vec![], Some("k")).await;
        let a = gate("127.0.0.1", vec![b.to_string()], Some("k")).await;
        let r = ask(&at(a, Scheme::Reflect), &[]).await.unwrap();
        assert_eq!(r.other_address, Some(b));
        assert_eq!(r.mapping.code, Code::Ok);
        let cross = &r.cross[0];
        assert_eq!(cross.code, Code::Connected);
        assert_eq!(cross.answer.as_ref().unwrap().token, "received");
        let json: serde_json::Value = serde_json::from_str(&render_json(&r)).unwrap();
        assert_eq!(json["cross"][0]["code"], "connected");
        assert_eq!(json["udp"]["code"], "ok");
        assert!(render_text(&r).contains("cross      port "));
    }

    /// Filtering through the other gate's own address, which needs a
    /// second loopback address: Linux has one, macOS does not.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn filtering_is_read_through_the_other_gate() {
        let b = gate("127.0.0.2", vec![], Some("k")).await;
        let a = gate("127.0.0.1", vec![b.to_string()], Some("k")).await;
        let r = ask(&at(a, Scheme::Stun), &[]).await.unwrap();
        assert_eq!(r.filtering.code, Code::Ok);
        let f = r.filtering.answer.unwrap();
        assert_eq!(f.kind, "endpoint_independent");
        assert_eq!(f.change_ip_reply.map(|a| a.ip()), Some(b.ip()));
    }

    #[tokio::test]
    async fn no_answer_is_named_per_protocol() {
        // A refusing low port over TCP, and nothing answers UDP there.
        let r = ask(&at("127.0.0.1:1".parse().unwrap(), Scheme::Reflect), &[])
            .await
            .unwrap();
        assert!(!r.answered());
        assert_eq!(r.udp.code, Code::UdpBlocked);
        assert_eq!(r.tcp.as_ref().unwrap().code, Code::TcpBlocked);
        assert_eq!(r.cross[0].code, Code::TcpBlocked);
    }
}

//! The reflect server: one UDP socket and one TCP listener per `--host`,
//! on one port, and the link to the other gate (`doc/Reflect.md`).
//!
//! ## surface block
//!
//! - Entry points: [`run`], bind and serve; [`Gate::bind`],
//!   [`Gate::addrs`] and [`Gate::serve`], the same in steps, for a test
//!   that needs the port it bound.
//! - Configurable: [`CROSS_CONNECT_TIMEOUT`], [`PEER_TIMEOUT`],
//!   [`IDLE_TIMEOUT`], [`MAX_CONNECTIONS`], [`MAX_MESSAGE`],
//!   [`RESOLVE_EVERY`], [`NONCES_KEPT`].
//! - Fan-out: [`Request`], what a message asks, and the two matches on
//!   it: [`Inner::answer_udp`] and [`Inner::answer_tcp`]. A request between
//!   gates is [`Inner::answer_peer`].

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use drt_rtc::Cidr;
use ego_transport::stun::{
    decode, decode_rfc5780, encode_binding_error, encode_binding_request,
    encode_binding_success_with, message_len, normalize_server, ChangeRequest, ResponseAttributes,
    StunMessage, TransactionId, HEADER_LEN,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Semaphore;
use tokio::time::timeout;

use super::wire;
use super::{Capability, Code, ReflectRole};
use crate::p2p::peer::HostSpec;

/// How long the other gate waits for its connection to the client's port.
pub const CROSS_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a gate waits for the other gate's answer, connect included.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(8);
/// A TCP connection with no request for this long is closed.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// TCP connections served at once, per gate.
pub const MAX_CONNECTIONS: usize = 256;
/// The longest message taken over TCP; a request is well under it.
pub const MAX_MESSAGE: usize = 1024;
/// How often `--reflect-peer` names are resolved again.
pub const RESOLVE_EVERY: Duration = Duration::from_secs(60);
/// Nonces of signed requests remembered, so none is taken twice.
pub const NONCES_KEPT: usize = 10_000;

/// RFC 5780's CHANGE-REQUEST, named in a 420's UNKNOWN-ATTRIBUTES.
const ATTR_CHANGE_REQUEST: u16 = 0x0003;

/// Bind every `--host`, say what was bound and offered, and serve until a
/// socket fails.
pub async fn run(role: &ReflectRole) -> Result<(), String> {
    let gate = Gate::bind(role).await?;
    for line in gate.describe() {
        eprintln!("drt p2p: {line}");
    }
    gate.serve().await
}

/// What a binding request asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// The mapped address: every server answers this.
    Binding,
    /// RFC 5780 CHANGE-REQUEST, over UDP.
    Change(ChangeRequest),
    /// Connect to my observed address on this port, over TCP.
    Cross {
        port: u16,
        token: [u8; 16],
    },
    /// From the other gate: connect here and write the token.
    PeerCross {
        target: SocketAddr,
        token: [u8; 16],
    },
    /// From the other gate: answer this client's binding request.
    PeerChange {
        client: SocketAddr,
        change: ChangeRequest,
    },
    Malformed,
}

impl Request {
    pub fn read(message: &[u8]) -> Request {
        let token =
            |m: &[u8]| -> Option<[u8; 16]> { wire::find(m, wire::CROSS_TOKEN)?.try_into().ok() };
        if let Some(value) = wire::find(message, wire::PEER_CROSS) {
            return match (wire::decode_address(value), token(message)) {
                (Some(target), Some(token)) => Request::PeerCross { target, token },
                _ => Request::Malformed,
            };
        }
        if let Some(value) = wire::find(message, wire::PEER_CHANGE) {
            let flags = value.len().checked_sub(4).map(|at| value[at + 3]);
            return match (
                value
                    .len()
                    .checked_sub(4)
                    .and_then(|at| wire::decode_address(&value[..at])),
                flags,
            ) {
                (Some(client), Some(flags)) => Request::PeerChange {
                    client,
                    change: ChangeRequest {
                        change_ip: flags & 0x04 != 0,
                        change_port: flags & 0x02 != 0,
                    },
                },
                _ => Request::Malformed,
            };
        }
        if let Some(value) = wire::find(message, wire::CROSS_PORT) {
            return match (value, token(message)) {
                ([hi, lo, 0, 0], Some(token)) => Request::Cross {
                    port: u16::from_be_bytes([*hi, *lo]),
                    token,
                },
                _ => Request::Malformed,
            };
        }
        match decode_rfc5780(message) {
            Ok(attrs) => match attrs.change_request {
                Some(change) if !change.is_none() => Request::Change(change),
                _ => Request::Binding,
            },
            Err(_) => Request::Malformed,
        }
    }
}

/// The flags of a CHANGE-REQUEST, as its four bytes.
pub fn change_flags(change: ChangeRequest) -> [u8; 4] {
    let mut flags = 0u8;
    if change.change_ip {
        flags |= 0x04;
    }
    if change.change_port {
        flags |= 0x02;
    }
    [0, 0, 0, flags]
}

/// A bound server, not yet serving.
pub struct Gate {
    inner: Arc<Inner>,
    endpoints: Vec<Endpoint>,
}

struct Endpoint {
    udp: Arc<UdpSocket>,
    tcp: TcpListener,
    local: SocketAddr,
    accept: Vec<Cidr>,
}

struct Inner {
    peers: Vec<String>,
    resolved: RwLock<Vec<SocketAddr>>,
    key: Option<Vec<u8>>,
    capabilities: String,
    rate: Limiter,
    /// What the other gate is asked to connect to, per target, on this
    /// gate's side of the link.
    target_rate: Limiter,
    nonces: Mutex<HashMap<[u8; 16], Instant>>,
    /// Every endpoint's UDP socket, for answering a change request the
    /// other gate handed over.
    udp: Vec<Arc<UdpSocket>>,
    slots: Arc<Semaphore>,
}

impl Gate {
    pub async fn bind(role: &ReflectRole) -> Result<Gate, String> {
        if !role.peers.is_empty() && role.key.is_none() {
            return Err(
                "--reflect-peer needs --reflect-key: every request between the gates is signed"
                    .into(),
            );
        }
        // Several hosts are several binds, and an everywhere bind takes the
        // port from the rest.
        if role.hosts.len() > 1 {
            for host in &role.hosts {
                if !host.bind_ip().is_unspecified() {
                    continue;
                }
                return Err(match host {
                    HostSpec::Addr(ip) => {
                        format!("--host {ip} already admits anyone; give it alone")
                    }
                    HostSpec::Range(range) => format!(
                        "--host {range}: this machine has no address inside it, so it would bind \
                         every address; give it alone"
                    ),
                });
            }
        }
        let mut endpoints = Vec::new();
        for host in &role.hosts {
            let ip = host.bind_ip();
            let (udp, tcp) = bind_pair(ip, role.port).await.map_err(|e| {
                format!(
                    "--reflect cannot bind {}: {e}",
                    SocketAddr::new(ip, role.port)
                )
            })?;
            let local = udp.local_addr().map_err(|e| e.to_string())?;
            endpoints.push(Endpoint {
                udp: Arc::new(udp),
                tcp,
                local,
                accept: host.accept(),
            });
        }
        let mut capabilities = vec![Capability::Udp, Capability::Tcp];
        if !role.peers.is_empty() {
            capabilities.extend([Capability::Peer, Capability::Filtering, Capability::Cross]);
        }
        let inner = Arc::new(Inner {
            peers: role.peers.clone(),
            resolved: RwLock::new(Vec::new()),
            key: role.key.clone(),
            capabilities: capabilities
                .iter()
                .map(|c| c.name())
                .collect::<Vec<_>>()
                .join(","),
            rate: Limiter::new(role.rate),
            target_rate: Limiter::new(role.rate),
            nonces: Mutex::new(HashMap::new()),
            udp: endpoints.iter().map(|e| e.udp.clone()).collect(),
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        });
        inner.resolve().await;
        Ok(Gate { inner, endpoints })
    }

    /// Where each `--host` was bound, UDP and TCP alike.
    pub fn addrs(&self) -> Vec<SocketAddr> {
        self.endpoints.iter().map(|e| e.local).collect()
    }

    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for e in &self.endpoints {
            let who = if e.accept.is_empty() {
                "whoever reaches it".to_string()
            } else {
                e.accept
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            lines.push(format!(
                "reflect on udp and tcp {}, admitting {who}",
                e.local
            ));
        }
        lines.push(format!("reflect offers {}", self.inner.capabilities));
        let resolved = self.inner.resolved.read().expect("not poisoned").clone();
        for peer in &self.inner.peers {
            lines.push(format!("reflect peer {peer}"));
        }
        if !self.inner.peers.is_empty() && resolved.is_empty() {
            lines.push(
                "reflect peer: no address yet; OTHER-ADDRESS is left out until one resolves".into(),
            );
        }
        lines
    }

    pub async fn serve(self) -> Result<(), String> {
        let mut tasks = tokio::task::JoinSet::new();
        if !self.inner.peers.is_empty() {
            let inner = self.inner.clone();
            tasks.spawn(async move {
                loop {
                    tokio::time::sleep(RESOLVE_EVERY).await;
                    inner.resolve().await;
                }
            });
        }
        for e in self.endpoints {
            let (inner, udp, accept) = (self.inner.clone(), e.udp.clone(), e.accept.clone());
            tasks.spawn(async move { serve_udp(inner, udp, accept).await });
            let inner = self.inner.clone();
            tasks.spawn(async move { serve_tcp(inner, e.tcp, e.accept).await });
        }
        match tasks.join_next().await {
            Some(Ok(Err(e))) => Err(e),
            Some(Err(e)) => Err(format!("reflect: {e}")),
            _ => Ok(()),
        }
    }
}

// depth: binding

/// A UDP socket and a TCP listener on one port. Port 0 picks one free for
/// both.
async fn bind_pair(ip: IpAddr, port: u16) -> std::io::Result<(UdpSocket, TcpListener)> {
    for _ in 0..16 {
        let udp = UdpSocket::bind(SocketAddr::new(ip, port)).await?;
        match TcpListener::bind(udp.local_addr()?).await {
            Ok(tcp) => return Ok((udp, tcp)),
            Err(e) if port == 0 && e.kind() == ErrorKind::AddrInUse => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        ErrorKind::AddrInUse,
        "no port free for both UDP and TCP",
    ))
}

fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

fn admitted(accept: &[Cidr], ip: IpAddr) -> bool {
    accept.is_empty() || accept.iter().any(|c| c.contains(ip))
}

// depth: serving

async fn serve_udp(
    inner: Arc<Inner>,
    udp: Arc<UdpSocket>,
    accept: Vec<Cidr>,
) -> Result<(), String> {
    let mut buf = [0u8; 1500];
    loop {
        let (n, from) = udp
            .recv_from(&mut buf)
            .await
            .map_err(|e| format!("reflect udp: {e}"))?;
        // Junk, a response, or a stranger outside --host: silence, so the
        // server reflects nothing it was not asked by someone admitted.
        if !admitted(&accept, from.ip().to_canonical()) {
            continue;
        }
        let Ok(StunMessage::BindingRequest { txid }) = decode(&buf[..n]) else {
            continue;
        };
        if let Some(answer) = inner.clone().answer_udp(&udp, txid, from, &buf[..n]) {
            let _ = udp.send_to(&answer, from).await;
        }
    }
}

async fn serve_tcp(
    inner: Arc<Inner>,
    listener: TcpListener,
    accept: Vec<Cidr>,
) -> Result<(), String> {
    loop {
        let (stream, from) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(permit) = inner.slots.clone().try_acquire_owned() else {
            continue;
        };
        let admitted = admitted(&accept, from.ip().to_canonical());
        let inner = inner.clone();
        tokio::spawn(async move {
            inner.connection(stream, canonical(from), admitted).await;
            drop(permit);
        });
    }
}

/// One STUN message off a stream, framed by its own header (RFC 5389
/// §7.2.2). `None` when the stream ends, idles out or sends something
/// that is not one.
pub async fn read_message(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; HEADER_LEN];
    stream.read_exact(&mut buf).await.ok()?;
    let len = message_len(&buf).ok()?;
    if len > MAX_MESSAGE {
        return None;
    }
    buf.resize(len, 0);
    stream.read_exact(&mut buf[HEADER_LEN..]).await.ok()?;
    Some(buf)
}

impl Inner {
    /// The UDP answer, or `None` when the other gate answers instead.
    fn answer_udp(
        self: Arc<Self>,
        udp: &Arc<UdpSocket>,
        txid: TransactionId,
        from: SocketAddr,
        message: &[u8],
    ) -> Option<Vec<u8>> {
        let client = canonical(from);
        let local = udp.local_addr().ok()?;
        match Request::read(message) {
            Request::Binding => Some(self.success(txid, client, local, &[])),
            Request::Change(change) if change.change_ip => {
                if self.peers.is_empty() {
                    return Some(self.refusal(txid, Code::NoPeer));
                }
                if !self.rate.allow(client.ip()) {
                    return Some(self.refusal(txid, Code::RateLimited));
                }
                let udp = udp.clone();
                tokio::spawn(async move {
                    if let Err(code) = self.hand_over_change(txid, client, change).await {
                        let _ = udp.send_to(&self.refusal(txid, code), from).await;
                    }
                });
                None
            }
            // No alternate port: a change-port alone is not offered.
            Request::Change(_) => Some(self.refusal(txid, Code::NotOffered)),
            // A cross request is TCP's, and a request between gates is too.
            Request::Cross { .. } | Request::PeerCross { .. } | Request::PeerChange { .. } => {
                Some(self.refusal(txid, Code::NotOffered))
            }
            Request::Malformed => Some(self.refusal(txid, Code::BadRequest)),
        }
    }

    async fn connection(
        self: Arc<Self>,
        mut stream: TcpStream,
        client: SocketAddr,
        admitted: bool,
    ) {
        let Ok(local) = stream.local_addr() else {
            return;
        };
        loop {
            let Ok(Some(message)) = timeout(IDLE_TIMEOUT, read_message(&mut stream)).await else {
                return;
            };
            let Ok(StunMessage::BindingRequest { txid }) = decode(&message) else {
                return;
            };
            // The other gate need not be inside --host: its signature
            // admits it. Everyone else is admitted by address or not at all.
            let answer = if wire::find(&message, wire::PEER_AUTH).is_some() {
                self.answer_peer(txid, client, local, &message).await
            } else if admitted {
                self.answer_tcp(txid, client, local, &message).await
            } else {
                return;
            };
            if stream.write_all(&answer).await.is_err() {
                return;
            }
        }
    }

    async fn answer_tcp(
        &self,
        txid: TransactionId,
        client: SocketAddr,
        local: SocketAddr,
        message: &[u8],
    ) -> Vec<u8> {
        match Request::read(message) {
            Request::Binding => self.success(txid, client, local, &[]),
            Request::Cross { port, token } => {
                let code = self.cross(client, port, token).await;
                match code {
                    Code::Connected | Code::Refused | Code::Timeout | Code::Unreachable => self
                        .success(
                            txid,
                            client,
                            local,
                            &[(wire::RESULT, code.name().as_bytes())],
                        ),
                    _ => self.refusal(txid, code),
                }
            }
            // CHANGE-REQUEST is a UDP test, and the gate requests are
            // signed.
            Request::Change(_) | Request::PeerCross { .. } | Request::PeerChange { .. } => {
                self.refusal(txid, Code::NotOffered)
            }
            Request::Malformed => self.refusal(txid, Code::BadRequest),
        }
    }

    /// A request from the other gate, checked before anything is done.
    async fn answer_peer(
        &self,
        txid: TransactionId,
        from: SocketAddr,
        local: SocketAddr,
        message: &[u8],
    ) -> Vec<u8> {
        let Some(key) = &self.key else {
            return self.refusal(txid, Code::NoKey);
        };
        let Ok(nonce) = wire::verify(message, key) else {
            return self.refusal(txid, Code::PeerRefused);
        };
        if !self.fresh(nonce) {
            return self.refusal(txid, Code::PeerRefused);
        }
        let code = match Request::read(message) {
            Request::PeerCross { target, token } => {
                if !self.target_rate.allow(target.ip()) {
                    Code::RateLimited
                } else if target.port() == 0 {
                    Code::BadRequest
                } else {
                    connect_to(target, token).await
                }
            }
            Request::PeerChange { client, change } => {
                self.answer_change(txid, client, change).await
            }
            _ => Code::BadRequest,
        };
        match code {
            Code::RateLimited | Code::BadRequest => self.refusal(txid, code),
            _ => self.success(txid, from, local, &[(wire::RESULT, code.name().as_bytes())]),
        }
    }

    // depth: the requests that need the other gate

    /// The client's cross request, carried by the other gate to the
    /// address this gate observed.
    async fn cross(&self, client: SocketAddr, port: u16, token: [u8; 16]) -> Code {
        if self.peers.is_empty() {
            return Code::NoPeer;
        }
        if port == 0 {
            return Code::BadRequest;
        }
        if !self.rate.allow(client.ip()) {
            return Code::RateLimited;
        }
        let target = SocketAddr::new(client.ip(), port);
        let request = wire::append(
            encode_binding_request(&TransactionId::random()).to_vec(),
            &[
                (wire::PEER_CROSS, &wire::encode_address(target)),
                (wire::CROSS_TOKEN, &token),
            ],
        );
        self.ask_peer(request).await.unwrap_or_else(|code| code)
    }

    /// A change-IP request, handed to the other gate under the client's own
    /// transaction id, so its answer is the one the client is waiting for.
    async fn hand_over_change(
        &self,
        txid: TransactionId,
        client: SocketAddr,
        change: ChangeRequest,
    ) -> Result<(), Code> {
        let mut value = wire::encode_address(client);
        value.extend_from_slice(&change_flags(change));
        let request = wire::append(
            encode_binding_request(&txid).to_vec(),
            &[(wire::PEER_CHANGE, &value)],
        );
        match self.ask_peer(request).await? {
            Code::Sent => Ok(()),
            other => Err(other),
        }
    }

    /// Answer a change request the other gate handed over, from a socket
    /// of the client's family.
    async fn answer_change(
        &self,
        txid: TransactionId,
        client: SocketAddr,
        _change: ChangeRequest,
    ) -> Code {
        let socket = self.udp.iter().find(|u| {
            u.local_addr()
                .is_ok_and(|l| l.is_ipv4() == client.is_ipv4())
        });
        let Some(socket) = socket else {
            return Code::Unreachable;
        };
        let Ok(local) = socket.local_addr() else {
            return Code::Unreachable;
        };
        let answer = self.success(txid, client, local, &[]);
        match socket.send_to(&answer, client).await {
            Ok(_) => Code::Sent,
            Err(_) => Code::Unreachable,
        }
    }

    /// Send a request to the other gate, signed, and read its code. The
    /// peers are tried in order; the first to answer decides.
    async fn ask_peer(&self, request: Vec<u8>) -> Result<Code, Code> {
        let key = self.key.as_ref().ok_or(Code::NoKey)?;
        let request = wire::sign(request, key, wire::random16());
        let peers = self.resolved.read().expect("not poisoned").clone();
        for peer in peers {
            let exchange = async {
                let mut stream = TcpStream::connect(peer).await.ok()?;
                stream.write_all(&request).await.ok()?;
                read_message(&mut stream).await
            };
            let Ok(Some(answer)) = timeout(PEER_TIMEOUT, exchange).await else {
                continue;
            };
            return wire::find(&answer, wire::RESULT)
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(Code::parse)
                .ok_or(Code::PeerUnreachable);
        }
        Err(Code::PeerUnreachable)
    }

    async fn resolve(&self) {
        let mut out = Vec::new();
        for peer in &self.peers {
            if let Ok(addrs) = tokio::net::lookup_host(normalize_server(peer)).await {
                out.extend(addrs.map(canonical));
            }
        }
        // A failed lookup keeps the last good answer rather than none.
        if !out.is_empty() || self.peers.is_empty() {
            *self.resolved.write().expect("not poisoned") = out;
        }
    }

    /// A nonce not seen within twice the allowed clock skew.
    fn fresh(&self, nonce: [u8; 16]) -> bool {
        let mut seen = self.nonces.lock().expect("not poisoned");
        let now = Instant::now();
        let keep = wire::CLOCK_SKEW * 2;
        if seen.len() >= NONCES_KEPT {
            seen.retain(|_, at| now.duration_since(*at) < keep);
        }
        if seen.len() >= NONCES_KEPT {
            return false;
        }
        seen.insert(nonce, now).is_none()
    }

    // depth: answers

    fn other_address(&self, client: SocketAddr) -> Option<SocketAddr> {
        self.resolved
            .read()
            .expect("not poisoned")
            .iter()
            .find(|a| a.is_ipv4() == client.is_ipv4())
            .copied()
    }

    fn success(
        &self,
        txid: TransactionId,
        client: SocketAddr,
        local: SocketAddr,
        extra: &[(u16, &[u8])],
    ) -> Vec<u8> {
        let attrs = ResponseAttributes {
            response_origin: (!local.ip().is_unspecified()).then_some(local),
            other_address: self.other_address(client),
        };
        let mut all: Vec<(u16, &[u8])> = vec![(wire::CAPABILITIES, self.capabilities.as_bytes())];
        all.extend_from_slice(extra);
        wire::append(encode_binding_success_with(&txid, client, &attrs), &all)
    }

    fn refusal(&self, txid: TransactionId, code: Code) -> Vec<u8> {
        let stun = code.stun_error();
        let unknown: &[u16] = if stun == 420 {
            &[ATTR_CHANGE_REQUEST]
        } else {
            &[]
        };
        wire::append(
            encode_binding_error(&txid, stun, code.name(), unknown),
            &[
                (wire::CAPABILITIES, self.capabilities.as_bytes()),
                (wire::RESULT, code.name().as_bytes()),
            ],
        )
    }
}

/// The other gate's half of a cross request: connect, and on success write
/// the token, so the client's listener knows who reached it.
async fn connect_to(target: SocketAddr, token: [u8; 16]) -> Code {
    match timeout(CROSS_CONNECT_TIMEOUT, TcpStream::connect(target)).await {
        Err(_) => Code::Timeout,
        Ok(Ok(mut stream)) => {
            let _ = timeout(Duration::from_secs(1), stream.write_all(&token)).await;
            Code::Connected
        }
        Ok(Err(e)) if e.kind() == ErrorKind::ConnectionRefused => Code::Refused,
        Ok(Err(e)) if e.kind() == ErrorKind::TimedOut => Code::Timeout,
        Ok(Err(_)) => Code::Unreachable,
    }
}

/// Requests a minute per address, in fixed one-minute windows.
struct Limiter {
    per_minute: u32,
    seen: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl Limiter {
    fn new(per_minute: u32) -> Limiter {
        Limiter {
            per_minute,
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn allow(&self, ip: IpAddr) -> bool {
        let window = Duration::from_secs(60);
        let now = Instant::now();
        let mut seen = self.seen.lock().expect("not poisoned");
        if seen.len() > 4096 {
            seen.retain(|_, (start, _)| now.duration_since(*start) < window);
        }
        let entry = seen.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        if entry.1 >= self.per_minute {
            return false;
        }
        entry.1 += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_transport::stun::{probe_tcp, probe_with, ProbeConfig};

    fn role(peers: Vec<String>, key: Option<&str>) -> ReflectRole {
        ReflectRole {
            port: 0,
            hosts: vec![HostSpec::parse("127.0.0.1").unwrap()],
            peers,
            key: key.map(|k| k.as_bytes().to_vec()),
            rate: 30,
        }
    }

    async fn started(role: ReflectRole) -> SocketAddr {
        let gate = Gate::bind(&role).await.unwrap();
        let at = gate.addrs()[0];
        tokio::spawn(gate.serve());
        at
    }

    /// One request over TCP, the answer as it came.
    async fn over_tcp(at: SocketAddr, request: &[u8]) -> Vec<u8> {
        let mut s = TcpStream::connect(at).await.unwrap();
        s.write_all(request).await.unwrap();
        read_message(&mut s).await.unwrap()
    }

    fn cross_request(port: u16, token: [u8; 16]) -> Vec<u8> {
        let [hi, lo] = port.to_be_bytes();
        wire::append(
            encode_binding_request(&TransactionId::random()).to_vec(),
            &[
                (wire::CROSS_PORT, &[hi, lo, 0, 0]),
                (wire::CROSS_TOKEN, &token),
            ],
        )
    }

    fn result(answer: &[u8]) -> &str {
        std::str::from_utf8(wire::find(answer, wire::RESULT).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_single_gate_answers_udp_and_tcp_and_says_what_it_offers() {
        let at = started(role(vec![], None)).await;
        let udp = probe_with(&at.to_string(), &ProbeConfig::default())
            .await
            .unwrap();
        assert_eq!(udp.reflexive.ip(), at.ip());
        assert_eq!(udp.reflexive.port(), udp.local.port());
        let tcp = probe_tcp(&at.to_string(), &ProbeConfig::default())
            .await
            .unwrap();
        assert_eq!(tcp.reflexive.ip(), at.ip());
        assert_eq!(tcp.reflexive.port(), tcp.local.port());

        let answer = over_tcp(at, &encode_binding_request(&TransactionId::random())).await;
        assert_eq!(
            wire::find(&answer, wire::CAPABILITIES),
            Some(&b"udp,tcp"[..])
        );
    }

    #[tokio::test]
    async fn a_cross_request_without_a_peer_says_no_peer() {
        let at = started(role(vec![], None)).await;
        let answer = over_tcp(at, &cross_request(22, [1; 16])).await;
        assert!(matches!(
            decode(&answer),
            Ok(StunMessage::BindingError { code: 420, .. })
        ));
        assert_eq!(result(&answer), "no_peer");
    }

    /// Gate A asks gate B, which connects back to the address A observed:
    /// `connected` to a listener, and the token arrives; `refused` where
    /// nothing listens.
    #[tokio::test]
    async fn the_peer_gate_connects_to_the_observed_address() {
        let b = started(role(vec![], Some("k"))).await;
        let a = started(role(vec![b.to_string()], Some("k"))).await;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = wire::random16();
        let answer = over_tcp(a, &cross_request(port, token)).await;
        assert_eq!(result(&answer), "connected");
        assert_eq!(
            wire::find(&answer, wire::CAPABILITIES),
            Some(&b"udp,tcp,peer,filtering,cross"[..])
        );
        let (mut got, _) = listener.accept().await.unwrap();
        let mut seen = [0u8; 16];
        got.read_exact(&mut seen).await.unwrap();
        assert_eq!(seen, token);

        drop(listener);
        let answer = over_tcp(a, &cross_request(port, token)).await;
        // Windows retries a refused SYN for longer than the connect may
        // take, so there a refusal can arrive as the timeout.
        if cfg!(windows) {
            assert!(matches!(result(&answer), "refused" | "timeout"));
        } else {
            assert_eq!(result(&answer), "refused");
        }
    }

    #[tokio::test]
    async fn a_peer_with_another_key_is_refused() {
        let b = started(role(vec![], Some("one"))).await;
        let a = started(role(vec![b.to_string()], Some("two"))).await;
        let answer = over_tcp(a, &cross_request(9, [0; 16])).await;
        assert_eq!(result(&answer), "peer_refused");
    }

    #[tokio::test]
    async fn a_signed_request_is_taken_once() {
        let b = started(role(vec![], Some("k"))).await;
        let request = wire::sign(
            wire::append(
                encode_binding_request(&TransactionId::random()).to_vec(),
                &[
                    (
                        wire::PEER_CROSS,
                        &wire::encode_address("127.0.0.1:9".parse().unwrap()),
                    ),
                    (wire::CROSS_TOKEN, &[0; 16]),
                ],
            ),
            b"k",
            [3; 16],
        );
        assert_ne!(result(&over_tcp(b, &request).await), "peer_refused");
        assert_eq!(result(&over_tcp(b, &request).await), "peer_refused");
    }

    #[tokio::test]
    async fn cross_requests_over_the_rate_are_refused() {
        let b = started(role(vec![], Some("k"))).await;
        let mut r = role(vec![b.to_string()], Some("k"));
        r.rate = 1;
        let a = started(r).await;
        let first = over_tcp(a, &cross_request(9, [0; 16])).await;
        assert_ne!(result(&first), "rate_limited");
        let second = over_tcp(a, &cross_request(9, [0; 16])).await;
        assert_eq!(result(&second), "rate_limited");
    }

    #[tokio::test]
    async fn an_unreachable_peer_gate_is_named() {
        // A refusing low port: nothing listens, and nothing will.
        let a = started(role(vec!["127.0.0.1:1".into()], Some("k"))).await;
        let answer = over_tcp(a, &cross_request(9, [0; 16])).await;
        assert_eq!(result(&answer), "peer_unreachable");
    }

    #[test]
    fn a_change_request_reads_back_through_its_flags() {
        for change in [ChangeRequest::PORT, ChangeRequest::IP_AND_PORT] {
            let mut value = wire::encode_address("203.0.113.9:4000".parse().unwrap());
            value.extend_from_slice(&change_flags(change));
            let m = wire::append(
                encode_binding_request(&TransactionId::random()).to_vec(),
                &[(wire::PEER_CHANGE, &value)],
            );
            assert_eq!(
                Request::read(&m),
                Request::PeerChange {
                    client: "203.0.113.9:4000".parse().unwrap(),
                    change
                }
            );
        }
    }

    /// RFC 5780 filtering against a pair: the change-IP answer comes from
    /// the other gate's address. Needs a second loopback address, which
    /// Linux has and macOS does not.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_peer_gate_answers_a_change_request_from_its_own_address() {
        use ego_transport::stun::{detect_filtering, NatFiltering};
        let mut rb = role(vec![], Some("k"));
        rb.hosts = vec![HostSpec::parse("127.0.0.2").unwrap()];
        let b = started(rb).await;
        let a = started(role(vec![b.to_string()], Some("k"))).await;
        let report = detect_filtering(&a.to_string(), &ProbeConfig::default())
            .await
            .unwrap();
        assert_eq!(report.other_address, b);
        assert_eq!(report.filtering, NatFiltering::EndpointIndependent);
        assert_eq!(report.change_ip_reply.map(|r| r.ip()), Some(b.ip()));
    }
}

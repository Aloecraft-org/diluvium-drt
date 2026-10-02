//! The host: one UDP socket, one str0m `Rtc` per browser session, the Wisp
//! server on each session's `wisp` channel, and the TCP splice behind it
//! (`doc/BrowserAccess.md` §3.3, §6).
//!
//! One task owns the socket and every session, so nothing about a session
//! is ever shared or locked. Each TCP stream has a reader task and a writer
//! task that talk to that one task over [`Internal`]; payload bytes go
//! between the socket, the data channel and TCP without passing through
//! anything a program wrote.
//!
//! **Direct mode** (`doc/BrowserAccess.md` §3.4) makes a session with no
//! `open` at all: a binding request no session claims, addressed to this
//! host's ufrag from a browser ufrag of [`DIRECT_UFRAG_LEN`], becomes a
//! session whose remote password is that same ufrag. The request has to pass
//! integrity against the host's password to be kept, so only a caller holding
//! the host's record gets one; the browser's DTLS certificate is not known in
//! advance, so its fingerprint is not checked, and whatever runs over the
//! stream authenticates the ends (SSH does).
//!
//! **The host runs on a thread of its own, with a stack it chose.** str0m's
//! `Rtc::do_poll_output` recurses once per SCTP packet it hands to DTLS
//! (`str0m-0.23.1/src/lib.rs:1734`, `return self.do_poll_output()`), so a
//! burst -- a fast download filling the window -- is as deep as the burst
//! is long. Measured on a debug build: 24.7 KB a frame, and a 4 MiB
//! download reached 40 frames before overflowing a 1 MiB stack. It first
//! showed as CI's 2 MiB test thread overflowing, because the host ran on
//! whatever stack its caller's runtime had. str0m buffers at most 128 KiB
//! across a session's channels, about 120 packets, so the worst case is
//! near 3 MiB in debug and far less in release; [`HOST_STACK`] is five
//! times that, and it is virtual memory until a page is touched.
//!
//! ## surface block
//!
//! - Entry points: [`Host::start`] (bind, then serve on the host's own
//!   thread), [`Host::send`], [`Host::try_event`], [`Host::next_event`].
//! - Configurable: [`WISP_BUFFER`], [`HIGH_WATER`], [`LOW_WATER`],
//!   [`SETUP_DEADLINE`], [`STUN_RETRY`], [`TICK`],
//!   [`HOST_STACK`], [`DIRECT_UFRAG_LEN`], and everything in
//!   [`HostConfig`].
//! - Fan-out: [`Command`] (what the program asks), [`Event`] (what the
//!   host reports), [`Internal`] (what a stream's tasks tell the loop),
//!   [`Session::on_wisp`]'s match over [`wisp::Packet`], and
//!   [`Forward`], where a stream that names no target goes
//!   (`doc/P2P.md` §5.1), with [`Sink`] as the two places any stream can
//!   end: a TCP dial, or a [`Service`] the process supplies.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::config::Fingerprint;
use str0m::net::{Protocol, Receive};
use str0m::{
    Candidate, CandidateKind, Event as RtcEvent, IceConnectionState, IceCreds, Input, Output, Rtc,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::cidr::Cidr;
use crate::identity::Identity;
use crate::record::Record;
use crate::scope::{self, Entry, Scope};
use crate::wisp::{self, reason, Packet};

/// Packets of browser-to-host `DATA` a stream may have queued (§6).
pub const WISP_BUFFER: u32 = 128;
/// Stop reading a session's TCP sockets once its channels hold this much.
/// str0m buffers at most 128 KiB across a session's channels and refuses a
/// write past that, so the mark sits below it.
pub const HIGH_WATER: usize = 96 * 1024;
/// Resume reading below this.
pub const LOW_WATER: usize = 32 * 1024;
/// A session that has not connected by now never will.
pub const SETUP_DEADLINE: Duration = Duration::from_secs(30);
/// How soon to ask the STUN servers again while no answer has come.
pub const STUN_RETRY: Duration = Duration::from_secs(2);
/// The housekeeping interval: idle streams, setup deadlines, the gate.
pub const TICK: Duration = Duration::from_secs(1);
/// The host thread's stack. See the module note: str0m recurses once per
/// SCTP packet in a batch, and this is five times the measured worst case.
pub const HOST_STACK: usize = 16 * 1024 * 1024;
/// A direct-mode browser's ufrag, which is also its ICE password: long
/// enough to be a password (RFC 8839 asks 22 characters, 128 bits) and short
/// enough to be a record's `u`. A browser's own ufrags are 4 or 8
/// characters, so a signaled session never matches.
pub const DIRECT_UFRAG_LEN: std::ops::RangeInclusive<usize> = 22..=32;

/// The largest datagram read off the socket.
const RECV_MTU: usize = 2000;
/// The negotiated channel ids (§4).
const CONTROL_ID: u16 = 0;
const WISP_ID: u16 = 1;

/// Everything the host is told once, at start.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Where the session socket binds. A wildcard address is advertised as
    /// the address this box routes from.
    pub bind: SocketAddr,
    pub identity: Identity,
    /// `host:port` STUN servers the server-reflexive candidates come from.
    pub stun: Vec<String>,
    /// How often to ask them once they have answered: often enough to hold
    /// the NAT mapping the record advertises open between sessions (home
    /// routers drop an idle UDP mapping after 30 to 120 s), and to notice
    /// when it moves, which re-reports the record.
    pub stun_refresh: Duration,
    /// Whether the record carries the host candidate (the LAN address).
    pub publish_host_candidates: bool,
    /// The label `hello` carries.
    pub service: String,
    pub default: Option<Entry>,
    pub scope: Scope,
    /// Named services (§10.3): each name a [`Sink`], reached by a CONNECT
    /// with port 0 and the name. A TCP sink is checked as a scope entry is.
    pub services: Vec<(String, Sink)>,
    pub max_sessions: usize,
    pub max_streams: usize,
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    /// Direct mode: a caller holding the record needs no signaling (see
    /// the module note).
    pub direct: bool,
    /// Whether `hello` carries `scope` and `default`. Off, it names the
    /// services and nothing about the addresses behind them, which are
    /// this side's policy and, for a forward into a private network, a map
    /// of it (`doc/P2P.md` §7.2).
    pub hello_scope: bool,
    /// Where a stream that names no target, or only a port, goes
    /// (`doc/P2P.md` §5.1). [`Forward::None`] is the `webrtc` block's
    /// shape: every stream names its target.
    pub forward: Forward,
    /// Capability names the root holds, for `hello` (`doc/P2P.md` §7.2):
    /// what a program or REPL behind this host may be granted, as
    /// `host:time/*` strings. Empty, `hello` says nothing of them; a host
    /// that forwards to a TCP target holds none.
    pub caps: Vec<String>,
    /// Addresses a session's packets may come from; empty admits every
    /// address. A session whose packets arrive from outside the ranges
    /// ends, whatever its record said (`doc/P2P.md` §6, `--accept`).
    pub accept: Vec<Cidr>,
}

/// Where a stream that names no target goes: the serving side's
/// `--forward` (`doc/P2P.md` §5). Only the serving side routes, and only
/// by this; a caller's request never causes an error by itself.
#[derive(Clone)]
pub enum Forward {
    /// Nothing: a stream names its target, in `scope` or `services`. A
    /// stream that names none is refused as malformed, as it always was.
    None,
    /// One target. A port the stream asks for is ignored.
    One(Sink),
    /// A host and a set of its ports. A stream that asks for a port gets it
    /// if the set holds it; one that asks for none gets the one port of a
    /// one-port set, and is otherwise refused as a closed port is.
    Ports { host: String, ports: PortSet },
    /// A relay (`doc/P2P.md` §4.1): the caller names the destination on
    /// `control`, this side calls it through the [`Relay`] and mirrors the
    /// two sessions' Wisp packets onto each other, reading none. `hello`
    /// says `forwarding`. Streams opened before the destination answers
    /// wait for it; a stream opened with no destination named is refused.
    Relay(Arc<dyn Relay>),
}

/// How a relaying host reaches the destination a caller names: whoever
/// embeds the host knows how a peer address is read and signalled, and the
/// host knows only that the result is a [`crate::caller::Call`].
pub trait Relay: Send + Sync {
    fn call(
        &self,
        to: &str,
    ) -> Pin<Box<dyn Future<Output = Result<crate::caller::Call, String>> + Send>>;
}

impl std::fmt::Debug for Forward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Forward::None => f.write_str("None"),
            Forward::One(sink) => f.debug_tuple("One").field(sink).finish(),
            Forward::Ports { host, ports } => f
                .debug_struct("Ports")
                .field("host", host)
                .field("ports", ports)
                .finish(),
            Forward::Relay(_) => f.write_str("Relay"),
        }
    }
}

/// A set of ports: `80,8080:8090,31200`, or every port (`-A`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSet {
    ranges: Vec<(u16, u16)>,
    any: bool,
}

impl PortSet {
    /// Every port.
    pub fn any() -> PortSet {
        PortSet {
            ranges: Vec::new(),
            any: true,
        }
    }

    /// A comma-separated list of ports and `low:high` ranges. Malformed is
    /// an error, never a guess.
    pub fn parse(s: &str) -> Result<PortSet, String> {
        let mut ranges = Vec::new();
        for item in s.split(',') {
            let item = item.trim();
            let port = |p: &str| {
                p.parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| format!("'{s}': '{p}' is not a port"))
            };
            let range = match item.split_once(':') {
                Some((low, high)) => {
                    let (low, high) = (port(low)?, port(high)?);
                    if low > high {
                        return Err(format!("'{s}': {low}:{high} runs backwards"));
                    }
                    (low, high)
                }
                None => {
                    let p = port(item)?;
                    (p, p)
                }
            };
            ranges.push(range);
        }
        Ok(PortSet { ranges, any: false })
    }

    pub fn contains(&self, port: u16) -> bool {
        self.any
            || self
                .ranges
                .iter()
                .any(|(lo, hi)| (*lo..=*hi).contains(&port))
    }

    /// The one port, when the set holds exactly one.
    pub fn single(&self) -> Option<u16> {
        match self.ranges.as_slice() {
            [(lo, hi)] if lo == hi && !self.any => Some(*lo),
            _ => None,
        }
    }
}

impl std::fmt::Display for PortSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.any {
            return f.write_str("every port");
        }
        let parts: Vec<String> = self
            .ranges
            .iter()
            .map(|(lo, hi)| {
                if lo == hi {
                    lo.to_string()
                } else {
                    format!("{lo}:{hi}")
                }
            })
            .collect();
        f.write_str(&parts.join(","))
    }
}

/// Where a stream the host serves ends.
#[derive(Clone)]
pub enum Sink {
    /// A TCP target, resolved and checked as scope entries are (§6).
    Dial(Entry),
    /// The process itself: the REPL, this process's stdio, another peer.
    Local(Arc<dyn Service>),
}

impl std::fmt::Debug for Sink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Sink::Dial(e) => f.debug_tuple("Dial").field(e).finish(),
            Sink::Local(s) => f.debug_tuple("Local").field(&s.name()).finish(),
        }
    }
}

/// A byte stream either side of a [`Service`] holds an end of.
pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}

/// The future a [`Service`] answers with: the stream, or the Wisp `CLOSE`
/// reason the caller gets instead.
pub type Opening = Pin<Box<dyn Future<Output = Result<Pin<Box<dyn Duplex>>, u8>> + Send>>;

/// Something in this process a stream can be handed to. The host runs the
/// stream's two directions over what `open` returns, exactly as it does
/// over a TCP connection, so a service never sees a Wisp packet.
pub trait Service: Send + Sync {
    /// What this is, for reports and refusals.
    fn name(&self) -> String;
    /// Open a stream for a `CONNECT` the host routed here: the host and
    /// port the stream asked for, which may be empty and 0, and the
    /// window the peer reports for it over `control` (`doc/P2P.md`
    /// §5.2), which a terminal-shaped service reads and the rest ignore.
    /// Runs on the host's own runtime.
    fn open(&self, host: &str, port: u16, window: Window) -> Opening;
}

/// A stream's terminal size as the peer last reported it over `control`
/// (`{"t":"resize","stream":N,"cols":C,"rows":R}`): columns and rows,
/// 0 until the peer says. Read it on every keystroke; that is how a
/// resize reaches the thing drawing the line.
#[derive(Clone, Default, Debug)]
pub struct Window(Arc<std::sync::atomic::AtomicU64>);

impl Window {
    pub fn get(&self) -> (u32, u32) {
        let packed = self.0.load(std::sync::atomic::Ordering::Relaxed);
        ((packed >> 32) as u32, packed as u32)
    }

    pub fn set(&self, cols: u32, rows: u32) {
        self.0.store(
            (u64::from(cols) << 32) | u64::from(rows),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

/// What the program asks of the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Make a session from a browser's presence record.
    Open { peer: String, rtc: String },
    /// End one.
    Close { peer: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Connected,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Open,
    Closed,
}

/// What the host reports. Never payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The host's own record, on start and whenever its candidates change.
    Record { rtc: String },
    Session {
        peer: String,
        state: SessionState,
        reason: Option<String>,
    },
    Stream {
        peer: String,
        stream: u32,
        host: String,
        port: u16,
        state: StreamState,
        reason: Option<&'static str>,
        /// Browser to target.
        bytes_up: u64,
        /// Target to browser.
        bytes_down: u64,
    },
}

/// A cloneable way to queue [`Command`]s, from [`Host::sender`].
#[derive(Clone)]
pub struct Sender(mpsc::UnboundedSender<Command>);

impl Sender {
    /// Queue a command. Never blocks; a host that has stopped drops it.
    pub fn send(&self, command: Command) {
        let _ = self.0.send(command);
    }
}

/// A serving host. Dropping it closes its command channel, which ends the
/// loop, and the host thread's runtime goes with it: every session, every
/// stream task, every socket.
pub struct Host {
    local_addr: SocketAddr,
    commands: mpsc::UnboundedSender<Command>,
    events: mpsc::UnboundedReceiver<Event>,
}

impl Host {
    /// Bind the socket and start serving on a thread of the host's own.
    ///
    /// Binding happens here, before the thread starts, so a port in use is
    /// a refusal at startup, by name. The thread owns a current-thread
    /// runtime: the socket, the sessions and every stream task live on it
    /// and on its [`HOST_STACK`], whatever runtime -- or none -- the caller
    /// has. Commands and reports cross over channels that care about
    /// neither.
    pub fn start(cfg: HostConfig) -> Result<Host, String> {
        let socket = std::net::UdpSocket::bind(cfg.bind)
            .map_err(|e| format!("webrtc cannot bind {}: {e}", cfg.bind))?;
        socket
            .set_nonblocking(true)
            .map_err(|e| format!("webrtc: {e}"))?;
        let bound = socket.local_addr().map_err(|e| format!("webrtc: {e}"))?;
        let base = SocketAddr::new(candidate_ip(bound)?, bound.port());
        let host_candidate = Candidate::host(base, "udp")
            .map_err(|e| format!("webrtc: {base} cannot be a host candidate: {e}"))?;

        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let (ready_tx, ready) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("drt-rtc-host".into())
            .stack_size(HOST_STACK)
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("webrtc: no runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(async move {
                    let socket = match UdpSocket::from_std(socket) {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("webrtc: {e}")));
                            return;
                        }
                    };
                    let (internal_tx, internal_rx) = mpsc::unbounded_channel();
                    // STUN servers resolve off the loop, and keep trying: a
                    // box that boots before its network should still come
                    // up and find its mapping later, not refuse to start.
                    if !cfg.stun.is_empty() {
                        let servers = cfg.stun.clone();
                        let tx = internal_tx.clone();
                        let refresh = cfg.stun_refresh;
                        tokio::spawn(async move {
                            resolve_stun(servers, bound.is_ipv4(), refresh, tx).await
                        });
                    }
                    let mut state = Loop {
                        ctx: Ctx {
                            cfg: Arc::new(cfg),
                            socket: Arc::new(socket),
                            base,
                            events: event_tx,
                            internal: internal_tx,
                        },
                        host_candidate,
                        sessions: Vec::new(),
                        next_key: 0,
                        stun_servers: Vec::new(),
                        stun_pending: HashMap::new(),
                        mapped: BTreeMap::new(),
                        next_stun: Instant::now(),
                        next_tick: Instant::now() + TICK,
                        last_record: None,
                    };
                    state.publish_record();
                    let _ = ready_tx.send(Ok(()));
                    state.run(command_rx, internal_rx).await;
                });
            })
            .map_err(|e| format!("webrtc: cannot start the host thread: {e}"))?;
        ready
            .recv()
            .map_err(|_| "webrtc: the host thread ended before it started".to_string())??;
        Ok(Host {
            local_addr: base,
            commands,
            events,
        })
    }

    /// The address the host candidate advertises: the bound port, on the
    /// address a wildcard bind resolved to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Queue a command. Never blocks; a host that has stopped drops it.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// A handle that queues commands from anywhere, while the `Host` itself
    /// stays with whoever reads its events.
    pub fn sender(&self) -> Sender {
        Sender(self.commands.clone())
    }

    /// The next report, if one is waiting. Never blocks.
    pub fn try_event(&mut self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// The next report, waiting for it.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }
}

/// The address a wildcard bind is advertised as: the one this box would
/// route from. `connect` on a UDP socket sends nothing; it only asks the
/// routing table.
pub(crate) fn candidate_ip(bound: SocketAddr) -> Result<IpAddr, String> {
    if !bound.ip().is_unspecified() {
        return Ok(bound.ip());
    }
    let (any, probe) = if bound.is_ipv4() {
        ("0.0.0.0:0", "192.0.2.1:9")
    } else {
        ("[::]:0", "[2001:db8::1]:9")
    };
    std::net::UdpSocket::bind(any)
        .and_then(|s| s.connect(probe).and_then(|_| s.local_addr()))
        .map(|a| a.ip())
        .map_err(|e| {
            format!(
                "webrtc: bind {bound} is a wildcard and this box has no route to advertise an \
                 address from ({e}); bind a specific address"
            )
        })
}

async fn resolve_stun(
    servers: Vec<String>,
    v4: bool,
    refresh: Duration,
    tx: mpsc::UnboundedSender<Internal>,
) {
    loop {
        let mut found = Vec::new();
        for s in &servers {
            if let Ok(addrs) = tokio::net::lookup_host(s.as_str()).await {
                found.extend(addrs.filter(|a| a.is_ipv4() == v4).take(1));
            }
        }
        let done = found.len() == servers.len();
        if tx.send(Internal::StunServers(found)).is_err() || done {
            return;
        }
        tokio::time::sleep(refresh).await;
    }
}

/// What a stream's tasks, and the STUN resolver, tell the loop.
enum Internal {
    StunServers(Vec<SocketAddr>),
    Connected {
        key: u64,
        stream: u32,
        to_tcp: mpsc::UnboundedSender<Vec<u8>>,
        tasks: [AbortHandle; 2],
    },
    Failed {
        key: u64,
        stream: u32,
        code: u8,
    },
    /// Bytes read from the target.
    TcpData {
        key: u64,
        stream: u32,
        bytes: Vec<u8>,
    },
    /// One queued packet written to the target.
    Written {
        key: u64,
        stream: u32,
    },
    /// The target is gone: end of stream, or an error.
    TcpDone {
        key: u64,
        stream: u32,
        code: u8,
    },
    /// A relay's call to its destination is up: where to send the caller's
    /// packets, and what the destination said of itself.
    RelayUp {
        key: u64,
        to: mpsc::UnboundedSender<Vec<u8>>,
        hello: String,
    },
    /// The destination refused, or could not be reached.
    RelayFailed {
        key: u64,
        why: String,
    },
    /// A packet from the destination, for the caller as it is.
    RelayPacket {
        key: u64,
        packet: Vec<u8>,
    },
    /// The destination's session ended.
    RelayDone {
        key: u64,
    },
}

/// What every session needs from the loop.
struct Ctx {
    cfg: Arc<HostConfig>,
    socket: Arc<UdpSocket>,
    /// The host candidate's address, which is also every received
    /// datagram's `destination`.
    base: SocketAddr,
    events: mpsc::UnboundedSender<Event>,
    internal: mpsc::UnboundedSender<Internal>,
}

impl Ctx {
    fn report(&self, e: Event) {
        let _ = self.events.send(e);
    }
}

struct Loop {
    ctx: Ctx,
    host_candidate: Candidate,
    sessions: Vec<Session>,
    next_key: u64,
    stun_servers: Vec<SocketAddr>,
    stun_pending: HashMap<[u8; 12], (SocketAddr, Instant)>,
    /// What each STUN server says this socket's public address is.
    mapped: BTreeMap<SocketAddr, SocketAddr>,
    next_stun: Instant,
    next_tick: Instant,
    last_record: Option<String>,
}

impl Loop {
    async fn run(
        &mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut internal: mpsc::UnboundedReceiver<Internal>,
    ) {
        let socket = self.ctx.socket.clone();
        let mut buf = vec![0u8; RECV_MTU];
        loop {
            for s in &mut self.sessions {
                s.drive(&self.ctx);
            }
            self.reap();
            let deadline = self
                .sessions
                .iter()
                .map(|s| s.timeout)
                .chain([self.next_tick, self.next_stun_due()])
                .min()
                .expect("the tick is always a deadline");
            tokio::select! {
                r = socket.recv_from(&mut buf) => {
                    if let Ok((n, source)) = r {
                        self.on_datagram(&buf[..n], source);
                    }
                }
                c = commands.recv() => match c {
                    Some(c) => self.on_command(c),
                    // The `Host` is gone, and with it anyone to report to.
                    None => return,
                },
                Some(i) = internal.recv() => self.on_internal(i),
                _ = tokio::time::sleep_until(deadline) => {}
            }
            self.on_time(Instant::now());
        }
    }

    fn next_stun_due(&self) -> Instant {
        if self.stun_servers.is_empty() {
            // Nothing to ask; the resolver wakes the loop when there is.
            self.next_tick + Duration::from_secs(3600)
        } else {
            self.next_stun
        }
    }

    /// Advance every clock that has run out.
    fn on_time(&mut self, now: Instant) {
        for s in &mut self.sessions {
            if s.timeout <= now {
                s.input_timeout(now);
            }
        }
        if now >= self.next_tick {
            self.next_tick = now + TICK;
            let idle = self.ctx.cfg.idle_timeout;
            for s in &mut self.sessions {
                s.housekeep(&self.ctx, now, idle);
            }
        }
        if !self.stun_servers.is_empty() && now >= self.next_stun {
            self.ask_stun(now);
        }
    }

    fn on_command(&mut self, c: Command) {
        match c {
            Command::Open { peer, rtc } => {
                let refuse = |reason: String| Event::Session {
                    peer: peer.clone(),
                    state: SessionState::Closed,
                    reason: Some(reason),
                };
                let record = match Record::decode(&rtc) {
                    Ok(r) => r,
                    Err(e) => return self.ctx.report(refuse(e.to_string())),
                };
                if self.sessions.len() >= self.ctx.cfg.max_sessions {
                    return self
                        .ctx
                        .report(refuse("busy: the session cap is reached".into()));
                }
                if self
                    .sessions
                    .iter()
                    .any(|s| s.peer == peer || s.remote_ufrag == record.ufrag)
                {
                    return self.ctx.report(refuse(
                        "duplicate: that peer or ufrag already has a session".into(),
                    ));
                }
                self.next_key += 1;
                match Session::new(
                    &self.ctx,
                    self.next_key,
                    peer.clone(),
                    &record,
                    &self.host_candidate,
                    true,
                ) {
                    Ok(s) => self.sessions.push(s),
                    Err(e) => self.ctx.report(refuse(e)),
                }
            }
            Command::Close { peer } => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.peer == peer) {
                    s.end("closed by the program");
                }
            }
        }
    }

    fn on_datagram(&mut self, buf: &[u8], source: SocketAddr) {
        if self.stun_answer(buf) {
            return;
        }
        let Ok(contents) = buf.try_into() else {
            return;
        };
        let input = Input::Receive(
            Instant::now().into_std(),
            Receive {
                proto: Protocol::Udp,
                source,
                destination: self.ctx.base,
                contents,
            },
        );
        // The first session that accepts it owns it: a binding request by
        // its (host, browser) ufrag pair, a response by transaction id,
        // DTLS and SCTP by source address.
        if let Some(s) = self.sessions.iter_mut().find(|s| s.rtc.accepts(&input)) {
            // Who may connect is checked on the packets themselves, so the
            // rule holds against a signalling server that ignored it: a
            // caller that signals from one address and connects from
            // another is judged by the one it connects from.
            if !admitted(&self.ctx.cfg.accept, source.ip()) {
                s.end(&format!("address {} is not admitted", source.ip()));
                return;
            }
            if let Err(e) = s.rtc.handle_input(input) {
                s.end(&format!("rtc: {e}"));
            }
        } else if self.ctx.cfg.direct && admitted(&self.ctx.cfg.accept, source.ip()) {
            self.direct_session(buf, input);
        }
    }

    /// Direct mode: make a session from a binding request nobody claimed,
    /// and keep it only when the request passes integrity against this
    /// host's password. Anything else is dropped without an answer, as an
    /// unknown datagram always is.
    fn direct_session(&mut self, buf: &[u8], input: Input) {
        let Some(remote) = direct_ufrag(buf, &self.ctx.cfg.identity.ufrag) else {
            return;
        };
        if self.sessions.len() >= self.ctx.cfg.max_sessions
            || self.sessions.iter().any(|s| s.remote_ufrag == remote)
        {
            return;
        }
        let record = Record {
            ufrag: remote.clone(),
            pwd: remote.clone(),
            fingerprint: [0; 32],
            candidates: Vec::new(),
        };
        let Ok(mut s) = Session::new(
            &self.ctx,
            self.next_key + 1,
            format!("direct:{remote}"),
            &record,
            &self.host_candidate,
            false,
        ) else {
            return;
        };
        if !s.rtc.accepts(&input) {
            return;
        }
        self.next_key += 1;
        if let Err(e) = s.rtc.handle_input(input) {
            s.end(&format!("rtc: {e}"));
        }
        self.sessions.push(s);
    }

    fn on_internal(&mut self, i: Internal) {
        let key = match &i {
            Internal::StunServers(found) => {
                self.stun_servers = found.clone();
                self.next_stun = Instant::now();
                return;
            }
            Internal::Connected { key, .. }
            | Internal::Failed { key, .. }
            | Internal::TcpData { key, .. }
            | Internal::Written { key, .. }
            | Internal::TcpDone { key, .. }
            | Internal::RelayUp { key, .. }
            | Internal::RelayFailed { key, .. }
            | Internal::RelayPacket { key, .. }
            | Internal::RelayDone { key } => *key,
        };
        match self.sessions.iter_mut().find(|s| s.key == key) {
            Some(s) => s.on_internal(&self.ctx, i),
            // The session ended while the message was in flight. A stream
            // that connected just too late is closed here, not leaked.
            None => {
                if let Internal::Connected { tasks, .. } = i {
                    tasks.iter().for_each(AbortHandle::abort);
                }
            }
        }
    }

    fn reap(&mut self) {
        let ctx = &self.ctx;
        self.sessions.retain_mut(|s| {
            let Some(reason) = s.dead.take() else {
                return true;
            };
            s.close_all_streams(ctx, "session_closed");
            ctx.report(Event::Session {
                peer: s.peer.clone(),
                state: SessionState::Closed,
                reason: Some(reason),
            });
            false
        });
    }

    // depth: server-reflexive candidates, gathered on the session socket

    fn ask_stun(&mut self, now: Instant) {
        self.next_stun = now
            + if self.mapped.is_empty() {
                STUN_RETRY
            } else {
                self.ctx.cfg.stun_refresh
            };
        self.stun_pending.retain(|_, (_, sent)| {
            now.duration_since(*sent) < STUN_RETRY.max(self.ctx.cfg.stun_refresh)
        });
        for server in &self.stun_servers {
            let txid = ego_transport::stun::TransactionId::random();
            let request = ego_transport::stun::encode_binding_request(&txid);
            let _ = self.ctx.socket.try_send_to(&request, *server);
            self.stun_pending.insert(*txid.as_bytes(), (*server, now));
        }
    }

    /// Take an answer to one of our own binding requests off the socket
    /// before any session sees it. Anything else is left for the sessions:
    /// the transaction id is ours or it is not.
    fn stun_answer(&mut self, buf: &[u8]) -> bool {
        use ego_transport::stun::StunMessage;
        let Ok(StunMessage::BindingSuccess { txid, mapped }) = ego_transport::stun::decode(buf)
        else {
            return false;
        };
        let Some((server, _)) = self.stun_pending.remove(txid.as_bytes()) else {
            return false;
        };
        self.mapped.insert(server, mapped);
        self.publish_record();
        true
    }

    /// Report the record when it differs from the last one reported.
    fn publish_record(&mut self) {
        let id = &self.ctx.cfg.identity;
        let mut candidates = Vec::new();
        if self.ctx.cfg.publish_host_candidates {
            candidates.push(self.host_candidate.to_sdp_string());
        }
        let mut seen = Vec::new();
        for addr in self.mapped.values() {
            if *addr == self.ctx.base || seen.contains(addr) {
                continue;
            }
            seen.push(*addr);
            if let Ok(c) = Candidate::server_reflexive(*addr, self.ctx.base, "udp") {
                candidates.push(srflx_line(&c, addr));
            }
        }
        candidates.truncate(crate::record::MAX_CANDIDATES);
        let record = Record {
            ufrag: id.ufrag.clone(),
            pwd: id.pwd.clone(),
            fingerprint: id.fingerprint(),
            candidates,
        };
        let Ok(rtc) = record.encode() else { return };
        if self.last_record.as_deref() != Some(rtc.as_str()) {
            self.last_record = Some(rtc.clone());
            self.ctx.report(Event::Record { rtc });
        }
    }
}

/// The browser ufrag of a direct-mode binding request: a STUN Binding
/// request whose USERNAME is `<host ufrag>:<browser ufrag>`, the host half
/// this host's and the browser half [`DIRECT_UFRAG_LEN`] ice-chars. `None`
/// for anything else. Integrity is not checked here; the session is.
fn direct_ufrag(buf: &[u8], host_ufrag: &str) -> Option<String> {
    const BINDING_REQUEST: u16 = 0x0001;
    const MAGIC_COOKIE: u32 = 0x2112_A442;
    const USERNAME: u16 = 0x0006;
    let u16_at = |i: usize| buf.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    if u16_at(0)? != BINDING_REQUEST
        || buf.get(4..8)? != MAGIC_COOKIE.to_be_bytes()
        || 20 + usize::from(u16_at(2)?) != buf.len()
    {
        return None;
    }
    let mut at = 20;
    while at + 4 <= buf.len() {
        let (kind, len) = (u16_at(at)?, usize::from(u16_at(at + 2)?));
        let value = buf.get(at + 4..at + 4 + len)?;
        if kind == USERNAME {
            let (host, browser) = std::str::from_utf8(value).ok()?.split_once(':')?;
            let ok = host == host_ufrag
                && DIRECT_UFRAG_LEN.contains(&browser.len())
                && browser.bytes().all(crate::record::ice_char);
            return ok.then(|| browser.to_string());
        }
        at += 4 + len.div_ceil(4) * 4;
    }
    None
}

/// A server-reflexive line with its related address blanked, as browsers
/// write theirs: the relation is the LAN address, and a host that chose not
/// to publish its host candidate should not publish it here instead.
pub(crate) fn srflx_line(c: &Candidate, addr: &SocketAddr) -> String {
    let line = c.to_sdp_string();
    let head = line.split(" raddr ").next().unwrap_or(&line);
    let blank = if addr.is_ipv4() { "0.0.0.0" } else { "::" };
    format!("{head} raddr {blank} rport 0")
}

// depth: one browser's session

struct Session {
    key: u64,
    peer: String,
    remote_ufrag: String,
    rtc: Rtc,
    control: ChannelId,
    wisp: ChannelId,
    wisp_open: bool,
    connected: bool,
    created: Instant,
    /// When str0m next wants to be woken.
    timeout: Instant,
    /// Packets str0m refused for want of buffer space, oldest first.
    outbox: VecDeque<Vec<u8>>,
    /// Open while the session can take more from its TCP sockets.
    gate: watch::Sender<bool>,
    streams: HashMap<u32, Stream>,
    /// Set when the session should end; `Loop::reap` does the ending.
    dead: Option<String>,
    /// A relaying session's other half (`Forward::Relay`).
    relay: RelayState,
}

/// Where a relaying session's destination stands.
enum RelayState {
    /// Not a relay, or no destination named yet; packets meanwhile are
    /// refused, since no `CONNECT` can be answered without one.
    None,
    /// Calling; the caller's packets wait here, oldest first.
    Calling(Vec<Vec<u8>>),
    /// Up: the caller's packets go here as they are.
    Up(mpsc::UnboundedSender<Vec<u8>>),
}

struct Stream {
    host: String,
    port: u16,
    /// What the peer says this stream's terminal measures, if anything.
    window: Window,
    /// Present once connected. Dropping it ends the writer task.
    to_tcp: Option<mpsc::UnboundedSender<Vec<u8>>>,
    /// `DATA` that arrived before the connection did.
    early: Vec<Vec<u8>>,
    queued: u32,
    written_since_continue: u32,
    bytes_up: u64,
    bytes_down: u64,
    last_activity: Instant,
    /// The connect task, then the reader and the writer.
    tasks: Vec<AbortHandle>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        // Aborting both halves' tasks drops both halves, which closes the
        // socket: a stream never outlives its entry here.
        self.tasks.iter().for_each(AbortHandle::abort);
    }
}

impl Session {
    fn new(
        ctx: &Ctx,
        key: u64,
        peer: String,
        record: &Record,
        host_candidate: &Candidate,
        verify_fingerprint: bool,
    ) -> Result<Session, String> {
        let id = &ctx.cfg.identity;
        let now = Instant::now();
        let mut rtc = Rtc::builder()
            .set_local_ice_credentials(IceCreds {
                ufrag: id.ufrag.clone(),
                pass: id.pwd.clone(),
            })
            .set_dtls_cert(id.cert.clone())
            .set_ice_lite(false)
            .set_fingerprint_verification(verify_fingerprint)
            .build(now.into_std());
        rtc.add_local_candidate(host_candidate.clone());
        let mut api = rtc.direct_api();
        api.set_ice_controlling(false);
        api.set_remote_ice_credentials(IceCreds {
            ufrag: record.ufrag.clone(),
            pass: record.pwd.clone(),
        });
        api.set_remote_fingerprint(Fingerprint {
            hash_func: "sha-256".into(),
            bytes: record.fingerprint.to_vec(),
        });
        api.start_dtls(false).map_err(|e| format!("rtc: {e}"))?;
        api.start_sctp(false);
        let channel = |label: &str, id: u16| ChannelConfig {
            label: label.into(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: Some(id),
            protocol: String::new(),
        };
        let control = api.create_data_channel(channel("control", CONTROL_ID));
        let wisp = api.create_data_channel(channel("wisp", WISP_ID));
        for line in &record.candidates {
            // A line str0m cannot read -- an mDNS name, a TCP or relay
            // candidate -- is skipped, not refused (§2.1). The browser's
            // checks teach the host its address anyway.
            match Candidate::from_sdp_string(line) {
                Ok(c) if c.proto() == Protocol::Udp && c.kind() != CandidateKind::Relayed => {
                    rtc.add_remote_candidate(c)
                }
                _ => {}
            }
        }
        let (gate, _) = watch::channel(true);
        Ok(Session {
            key,
            peer,
            remote_ufrag: record.ufrag.clone(),
            rtc,
            control,
            wisp,
            wisp_open: false,
            connected: false,
            created: now,
            timeout: now,
            outbox: VecDeque::new(),
            gate,
            streams: HashMap::new(),
            dead: None,
            relay: RelayState::None,
        })
    }

    fn end(&mut self, reason: &str) {
        if self.dead.is_none() {
            self.dead = Some(reason.to_string());
        }
        self.rtc.disconnect();
    }

    fn input_timeout(&mut self, now: Instant) {
        if let Err(e) = self.rtc.handle_input(Input::Timeout(now.into_std())) {
            self.end(&format!("rtc: {e}"));
        }
    }

    /// Drain str0m: send what it wants sent, handle what it says, and repeat
    /// until it only wants to be woken later. Handling an event can queue
    /// writes, which produce more output, which is why this loops.
    fn drive(&mut self, ctx: &Ctx) {
        loop {
            let mut events = Vec::new();
            loop {
                match self.rtc.poll_output() {
                    Ok(Output::Timeout(t)) => {
                        self.timeout = Instant::from_std(t);
                        break;
                    }
                    Ok(Output::Transmit(t)) => {
                        // A full send buffer drops the datagram, as the
                        // network might have; ICE, DTLS and SCTP retransmit.
                        let _ = ctx.socket.try_send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Event(e)) => events.push(e),
                    Err(e) => {
                        self.end(&format!("rtc: {e}"));
                        return;
                    }
                }
            }
            if events.is_empty() {
                return;
            }
            for e in events {
                self.on_rtc_event(ctx, e);
            }
            self.flush();
        }
    }

    fn on_rtc_event(&mut self, ctx: &Ctx, e: RtcEvent) {
        match e {
            RtcEvent::Connected => {
                self.connected = true;
                ctx.report(Event::Session {
                    peer: self.peer.clone(),
                    state: SessionState::Connected,
                    reason: None,
                });
            }
            RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                self.end("ice disconnected");
            }
            RtcEvent::ChannelOpen(id, _) if id == self.control => {
                let hello = hello(&ctx.cfg);
                if let Some(mut ch) = self.rtc.channel(self.control) {
                    let _ = ch.write(false, hello.as_bytes());
                }
            }
            RtcEvent::ChannelOpen(id, _) if id == self.wisp => {
                self.wisp_open = true;
                if let Some(mut ch) = self.rtc.channel(self.wisp) {
                    ch.set_buffered_amount_low_threshold(LOW_WATER);
                }
                // Stream 0: the initial credit, and the version marker.
                self.outbox.push_back(wisp::cont(0, WISP_BUFFER));
            }
            RtcEvent::ChannelData(d) if d.id == self.wisp => self.on_wisp(ctx, &d.data),
            RtcEvent::ChannelData(d) if d.id == self.control => self.on_control(ctx, &d.data),
            RtcEvent::ChannelClose(id) if id == self.wisp || id == self.control => {
                self.end("a data channel closed");
            }
            RtcEvent::ChannelBufferedAmountLow(_) => self.flush(),
            _ => {}
        }
    }

    /// Write what the outbox holds, in order, until str0m refuses one; then
    /// open or close the gate on what is left.
    fn flush(&mut self) {
        if self.wisp_open {
            while let Some(pkt) = self.outbox.front() {
                let accepted = match self.rtc.channel(self.wisp) {
                    Some(mut ch) => ch.write(true, pkt).unwrap_or(false),
                    None => false,
                };
                if !accepted {
                    break;
                }
                self.outbox.pop_front();
            }
        }
        let buffered = self
            .rtc
            .channel(self.wisp)
            .map(|mut ch| ch.buffered_amount())
            .unwrap_or(0);
        let open = *self.gate.borrow();
        if open && (buffered > HIGH_WATER || !self.outbox.is_empty()) {
            self.gate.send_replace(false);
        } else if !open && buffered < LOW_WATER && self.outbox.is_empty() {
            self.gate.send_replace(true);
        }
    }

    fn housekeep(&mut self, ctx: &Ctx, now: Instant, idle: Duration) {
        if !self.connected && now.duration_since(self.created) > SETUP_DEADLINE {
            self.end("never connected");
            return;
        }
        let stale: Vec<u32> = self
            .streams
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_activity) > idle)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.close_stream(ctx, id, reason::IDLE, true);
        }
        self.flush();
    }

    /// Drop a stream, tell the browser when it does not already know, and
    /// report it.
    fn close_stream(&mut self, ctx: &Ctx, id: u32, code: u8, tell_browser: bool) {
        let Some(s) = self.streams.remove(&id) else {
            return;
        };
        if tell_browser {
            self.outbox.push_back(wisp::close(id, code));
        }
        ctx.report(Event::Stream {
            peer: self.peer.clone(),
            stream: id,
            host: s.host.clone(),
            port: s.port,
            state: StreamState::Closed,
            reason: Some(reason::name(code)),
            bytes_up: s.bytes_up,
            bytes_down: s.bytes_down,
        });
    }

    fn close_all_streams(&mut self, ctx: &Ctx, why: &'static str) {
        for (id, s) in self.streams.drain() {
            ctx.report(Event::Stream {
                peer: self.peer.clone(),
                stream: id,
                host: s.host.clone(),
                port: s.port,
                state: StreamState::Closed,
                reason: Some(why),
                bytes_up: s.bytes_up,
                bytes_down: s.bytes_down,
            });
        }
    }

    /// Refuse a `CONNECT` without ever having opened a stream, and say so in
    /// the audit trail: a denied attempt is the event worth keeping.
    fn refuse(&mut self, ctx: &Ctx, id: u32, host: &str, port: u16, code: u8) {
        self.outbox.push_back(wisp::close(id, code));
        ctx.report(Event::Stream {
            peer: self.peer.clone(),
            stream: id,
            host: host.to_string(),
            port,
            state: StreamState::Closed,
            reason: Some(reason::name(code)),
            bytes_up: 0,
            bytes_down: 0,
        });
    }

    /// What the peer says on `control`: `resize` for one of its streams
    /// (`doc/P2P.md` §5.2), `call` for the destination of a relay (§4.1).
    /// Any other `t` is ignored, as §5 says.
    fn on_control(&mut self, ctx: &Ctx, msg: &[u8]) {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(msg) else {
            return;
        };
        if v["t"] == "call" {
            return self.on_call(ctx, v["to"].as_str().unwrap_or(""));
        }
        if v["t"] != "resize" {
            return;
        }
        let (Some(id), Some(cols), Some(rows)) =
            (v["stream"].as_u64(), v["cols"].as_u64(), v["rows"].as_u64())
        else {
            return;
        };
        if let Some(s) = u32::try_from(id).ok().and_then(|id| self.streams.get(&id)) {
            s.window.set(
                cols.min(u64::from(u16::MAX)) as u32,
                rows.min(u64::from(u16::MAX)) as u32,
            );
        }
    }

    /// `{"t":"call","to":…}`: call the destination and join the sessions
    /// (`doc/P2P.md` §4.1). Once per session; a second is ignored.
    fn on_call(&mut self, ctx: &Ctx, to: &str) {
        let Forward::Relay(relay) = &ctx.cfg.forward else {
            return;
        };
        if !matches!(self.relay, RelayState::None) || to.is_empty() {
            return;
        }
        self.relay = RelayState::Calling(Vec::new());
        let (relay, internal, key, to) = (
            relay.clone(),
            ctx.internal.clone(),
            self.key,
            to.to_string(),
        );
        tokio::spawn(async move {
            let call = match relay.call(&to).await {
                Ok(call) => call,
                Err(why) => {
                    let _ = internal.send(Internal::RelayFailed { key, why });
                    return;
                }
            };
            let raw = match call.raw().await {
                Ok(raw) => raw,
                Err(why) => {
                    let _ = internal.send(Internal::RelayFailed { key, why });
                    return;
                }
            };
            let hello = call.hello().to_string();
            let crate::caller::Raw {
                mut incoming,
                outgoing,
            } = raw;
            let _ = internal.send(Internal::RelayUp {
                key,
                to: outgoing,
                hello,
            });
            // The destination's packets, to the caller as they are; the
            // call is held here for as long as they flow.
            while let Some(packet) = incoming.recv().await {
                if internal
                    .send(Internal::RelayPacket { key, packet })
                    .is_err()
                {
                    return;
                }
            }
            drop(call);
            let _ = internal.send(Internal::RelayDone { key });
        });
    }

    fn on_wisp(&mut self, ctx: &Ctx, msg: &[u8]) {
        if matches!(ctx.cfg.forward, Forward::Relay(_)) {
            match &mut self.relay {
                RelayState::Up(to) => {
                    let _ = to.send(msg.to_vec());
                }
                RelayState::Calling(waiting) => waiting.push(msg.to_vec()),
                // No destination named: a CONNECT is refused as a closed
                // port is, and nothing else means anything yet.
                RelayState::None => {
                    if let Some(Packet::Connect { stream, .. }) = wisp::parse(msg) {
                        self.refuse(ctx, stream, "", 0, reason::BLOCKED);
                    }
                }
            }
            return;
        }
        let now = Instant::now();
        match wisp::parse(msg) {
            Some(Packet::Connect {
                stream,
                kind,
                port,
                host,
            }) => self.on_connect(ctx, stream, kind, port, host),
            Some(Packet::Malformed {
                kind: wisp::CONNECT,
                stream,
            }) => self.refuse(ctx, stream, "", 0, reason::INVALID),
            Some(Packet::Data { stream, payload }) => {
                let Some(s) = self.streams.get_mut(&stream) else {
                    return;
                };
                s.bytes_up += payload.len() as u64;
                s.last_activity = now;
                s.queued += 1;
                match &s.to_tcp {
                    Some(tx) => {
                        let _ = tx.send(payload.to_vec());
                    }
                    None => s.early.push(payload.to_vec()),
                }
            }
            Some(Packet::Close {
                stream,
                reason: code,
            }) => {
                // The browser ended it: nothing to tell it back, and the
                // audit records the reason it gave.
                self.close_stream(ctx, stream, code, false);
            }
            // CONTINUE is the server's to send; a short message, another
            // malformed packet, or an unknown type is ignored (§6).
            _ => {}
        }
    }

    fn on_connect(&mut self, ctx: &Ctx, id: u32, kind: u8, port: u16, host: &[u8]) {
        let Ok(host) = std::str::from_utf8(host).map(str::to_string) else {
            return self.refuse(ctx, id, "", port, reason::INVALID);
        };
        // An empty host asks for whatever this side forwards to, at the
        // port named or at none (`doc/P2P.md` §5.1); a name with port 0 is
        // a service (§10.3). Anything else with port 0 is malformed.
        if id == 0 || (port == 0 && !host.is_empty() && !scope::is_service_name(&host)) {
            return self.refuse(ctx, id, &host, port, reason::INVALID);
        }
        if host.is_empty() && matches!(ctx.cfg.forward, Forward::None) {
            return self.refuse(ctx, id, &host, port, reason::INVALID);
        }
        if self.streams.contains_key(&id) {
            // A reused id: the browser's view of both is now wrong, so both
            // end.
            self.close_stream(ctx, id, reason::INVALID, true);
            return self.refuse(ctx, id, &host, port, reason::INVALID);
        }
        match kind {
            wisp::STREAM_TCP => {}
            // Wisp v1 makes UDP mandatory; this profile does not carry it.
            wisp::STREAM_UDP => return self.refuse(ctx, id, &host, port, reason::BLOCKED),
            _ => return self.refuse(ctx, id, &host, port, reason::INVALID),
        }
        if self.streams.len() >= ctx.cfg.max_streams {
            return self.refuse(ctx, id, &host, port, reason::THROTTLED);
        }
        let sink = match route(&ctx.cfg, &host, port) {
            Ok(sink) => sink,
            Err(code) => return self.refuse(ctx, id, &host, port, code),
        };
        let internal = ctx.internal.clone();
        let gate = self.gate.subscribe();
        let (key, timeout) = (self.key, ctx.cfg.connect_timeout);
        let (asked_host, asked_port) = (host.clone(), port);
        let window = Window::default();
        let stream_window = window.clone();
        let connect = tokio::spawn(async move {
            let opened: Result<(Reader, Writer), u8> =
                match sink {
                    Sink::Dial(entry) => dial(&entry, timeout).await.map(|tcp| {
                        let (r, w) = tcp.into_split();
                        (Box::new(r) as Reader, Box::new(w) as Writer)
                    }),
                    Sink::Local(service) => service
                        .open(&asked_host, asked_port, window)
                        .await
                        .map(|io| {
                            let (r, w) = tokio::io::split(io);
                            (Box::new(r) as Reader, Box::new(w) as Writer)
                        }),
                };
            let msg = match opened {
                Ok((r, w)) => {
                    let (to_tcp, rx) = mpsc::unbounded_channel();
                    let writer = tokio::spawn(write_loop(w, rx, internal.clone(), key, id));
                    let reader = tokio::spawn(read_loop(r, gate, internal.clone(), key, id));
                    Internal::Connected {
                        key,
                        stream: id,
                        to_tcp,
                        tasks: [reader.abort_handle(), writer.abort_handle()],
                    }
                }
                Err(code) => Internal::Failed {
                    key,
                    stream: id,
                    code,
                },
            };
            let _ = internal.send(msg);
        });
        self.streams.insert(
            id,
            Stream {
                host,
                port,
                window: stream_window,
                to_tcp: None,
                early: Vec::new(),
                queued: 0,
                written_since_continue: 0,
                bytes_up: 0,
                bytes_down: 0,
                last_activity: Instant::now(),
                tasks: vec![connect.abort_handle()],
            },
        );
    }

    fn on_internal(&mut self, ctx: &Ctx, i: Internal) {
        let now = Instant::now();
        match i {
            Internal::StunServers(_) => {}
            Internal::Connected {
                stream,
                to_tcp,
                tasks,
                ..
            } => {
                let Some(s) = self.streams.get_mut(&stream) else {
                    tasks.iter().for_each(AbortHandle::abort);
                    return;
                };
                for early in s.early.drain(..) {
                    let _ = to_tcp.send(early);
                }
                s.to_tcp = Some(to_tcp);
                s.tasks.extend(tasks);
                s.last_activity = now;
                ctx.report(Event::Stream {
                    peer: self.peer.clone(),
                    stream,
                    host: s.host.clone(),
                    port: s.port,
                    state: StreamState::Open,
                    reason: None,
                    bytes_up: 0,
                    bytes_down: 0,
                });
            }
            Internal::Failed { stream, code, .. } | Internal::TcpDone { stream, code, .. } => {
                self.close_stream(ctx, stream, code, true);
            }
            Internal::TcpData { stream, bytes, .. } => {
                let Some(s) = self.streams.get_mut(&stream) else {
                    return;
                };
                s.bytes_down += bytes.len() as u64;
                s.last_activity = now;
                self.outbox.push_back(wisp::data(stream, &bytes));
            }
            Internal::RelayUp { to, hello, .. } => {
                let waiting = match std::mem::replace(&mut self.relay, RelayState::Up(to.clone())) {
                    RelayState::Calling(waiting) => waiting,
                    _ => Vec::new(),
                };
                for pkt in waiting {
                    let _ = to.send(pkt);
                }
                let told = serde_json::json!({"t": "called", "hello": hello}).to_string();
                if let Some(mut ch) = self.rtc.channel(self.control) {
                    let _ = ch.write(false, told.as_bytes());
                }
            }
            Internal::RelayFailed { why, .. } => {
                let told = serde_json::json!({"t": "failed", "why": why}).to_string();
                if let Some(mut ch) = self.rtc.channel(self.control) {
                    let _ = ch.write(false, told.as_bytes());
                }
                self.end(&format!("the destination could not be reached: {why}"));
            }
            Internal::RelayPacket { packet, .. } => self.outbox.push_back(packet),
            Internal::RelayDone { .. } => self.end("the destination's session ended"),
            Internal::Written { stream, .. } => {
                let Some(s) = self.streams.get_mut(&stream) else {
                    return;
                };
                s.queued = s.queued.saturating_sub(1);
                s.written_since_continue += 1;
                // Half a buffer written since the last CONTINUE: grant the
                // space back before the browser's credit can run out.
                if s.written_since_continue >= WISP_BUFFER / 2 {
                    s.written_since_continue = 0;
                    let remaining = WISP_BUFFER.saturating_sub(s.queued);
                    self.outbox.push_back(wisp::cont(stream, remaining));
                }
            }
        }
        self.flush();
    }
}

/// `hello` (§5): the services' names (§10.3), and the scope when the
/// config asks for it (`doc/P2P.md` §7.2), over the data channel and
/// nowhere else.
fn hello(cfg: &HostConfig) -> String {
    let entry = |e: &Entry| serde_json::json!({"scheme": e.scheme, "host": e.host, "port": e.port});
    let mut msg = serde_json::json!({
        "v": 1,
        "t": "hello",
        "service": cfg.service,
        "limits": {"max_streams": cfg.max_streams},
    });
    if cfg.hello_scope {
        msg["scope"] = cfg
            .scope
            .entries
            .iter()
            .map(entry)
            .collect::<Vec<_>>()
            .into();
        if let Some(d) = &cfg.default {
            msg["default"] = entry(d);
        }
    }
    if !cfg.services.is_empty() {
        msg["services"] = cfg.services.iter().map(|(name, _)| name.as_str()).collect();
    }
    if matches!(cfg.forward, Forward::Relay(_)) {
        msg["forwarding"] = true.into();
    }
    if !cfg.caps.is_empty() {
        msg["caps"] = cfg.caps.iter().map(String::as_str).collect();
    }
    msg.to_string()
}

/// Whether `ip` is inside one of `ranges`; an empty list admits everyone.
fn admitted(ranges: &[Cidr], ip: IpAddr) -> bool {
    ranges.is_empty() || ranges.iter().any(|r| r.contains(ip))
}

/// Where a `CONNECT` to `host:port` goes, by `doc/P2P.md` §5.1's table:
/// a named service by its name; an empty host by what this side forwards
/// to; a host and port by the scope, or by the forward's port set when
/// the host is the forward's. The error is the Wisp close reason.
fn route(cfg: &HostConfig, host: &str, port: u16) -> Result<Sink, u8> {
    if host.is_empty() {
        return match (&cfg.forward, port) {
            (Forward::None, _) | (Forward::Relay(_), _) => Err(reason::INVALID),
            (Forward::One(sink), _) => Ok(sink.clone()),
            (Forward::Ports { host, ports }, 0) => match ports.single() {
                Some(p) => Ok(Sink::Dial(tcp_entry(host, p))),
                None => Err(reason::BLOCKED),
            },
            (Forward::Ports { host, ports }, p) if ports.contains(p) => {
                Ok(Sink::Dial(tcp_entry(host, p)))
            }
            (Forward::Ports { .. }, _) => Err(reason::BLOCKED),
        };
    }
    if port == 0 {
        // A named service (§10.3) is its sink under another name.
        return match cfg.services.iter().find(|(name, _)| name == host) {
            Some((_, sink)) => Ok(sink.clone()),
            None => Err(reason::BLOCKED),
        };
    }
    if let Some(entry) = cfg.scope.allows(host, port) {
        return Ok(Sink::Dial(entry.clone()));
    }
    if let Forward::Ports {
        host: forward_host,
        ports,
    } = &cfg.forward
    {
        if forward_host.eq_ignore_ascii_case(host) && ports.contains(port) {
            return Ok(Sink::Dial(tcp_entry(forward_host, port)));
        }
    }
    Err(reason::BLOCKED)
}

/// A forward's host at one of its ports, as the entry the dial checks
/// against. The scheme is advice to a browser and this side gives none.
fn tcp_entry(host: &str, port: u16) -> Entry {
    Entry {
        scheme: "tcp".to_string(),
        host: host.to_string(),
        port,
    }
}

/// The two halves of whatever a stream ends in.
type Reader = Box<dyn AsyncRead + Send + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

// depth: the TCP side of a stream

/// Resolve, check every address against the entry, connect to the first
/// that answers. The error is the Wisp close reason.
async fn dial(entry: &Entry, timeout: Duration) -> Result<TcpStream, u8> {
    let (name, port) = (entry.host.as_str(), entry.port);
    let addrs: Vec<SocketAddr> =
        match tokio::time::timeout(timeout, tokio::net::lookup_host((name, port))).await {
            Err(_) => return Err(reason::TIMEOUT),
            Ok(Err(_)) => return Err(reason::UNREACHABLE),
            Ok(Ok(found)) => found.collect(),
        };
    if addrs.is_empty() {
        return Err(reason::UNREACHABLE);
    }
    let allowed: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| scope::resolved_ok(entry, a.ip()))
        .collect();
    if allowed.is_empty() {
        return Err(reason::BLOCKED);
    }
    let mut last = reason::UNREACHABLE;
    for addr in allowed {
        match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                last = reason::REFUSED
            }
            Ok(Err(_)) => last = reason::NETWORK_ERROR,
            Err(_) => last = reason::TIMEOUT,
        }
    }
    Err(last)
}

async fn write_loop(
    mut w: Writer,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    internal: mpsc::UnboundedSender<Internal>,
    key: u64,
    stream: u32,
) {
    while let Some(buf) = rx.recv().await {
        if w.write_all(&buf).await.is_err() {
            let _ = internal.send(Internal::TcpDone {
                key,
                stream,
                code: reason::NETWORK_ERROR,
            });
            return;
        }
        let _ = internal.send(Internal::Written { key, stream });
    }
    let _ = w.shutdown().await;
}

async fn read_loop(
    mut r: Reader,
    mut gate: watch::Receiver<bool>,
    internal: mpsc::UnboundedSender<Internal>,
    key: u64,
    stream: u32,
) {
    let mut buf = vec![0u8; wisp::MAX_PAYLOAD];
    loop {
        // Wait for room on the data channel before reading more: this is
        // the whole of host-to-browser backpressure.
        if gate.wait_for(|open| *open).await.is_err() {
            return;
        }
        let msg = match r.read(&mut buf).await {
            Ok(0) => Internal::TcpDone {
                key,
                stream,
                code: reason::VOLUNTARY,
            },
            Ok(n) => Internal::TcpData {
                key,
                stream,
                bytes: buf[..n].to_vec(),
            },
            Err(_) => Internal::TcpDone {
                key,
                stream,
                code: reason::NETWORK_ERROR,
            },
        };
        let done = matches!(msg, Internal::TcpDone { .. });
        if internal.send(msg).is_err() || done {
            return;
        }
    }
}

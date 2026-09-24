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
//!   [`SETUP_DEADLINE`], [`STUN_RETRY`], [`STUN_REFRESH`], [`TICK`],
//!   [`HOST_STACK`], and everything in [`HostConfig`].
//! - Fan-out: [`Command`] (what the program asks), [`Event`] (what the
//!   host reports), [`Internal`] (what a stream's tasks tell the loop), and
//!   [`Session::on_wisp`]'s match over [`wisp::Packet`].

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::config::Fingerprint;
use str0m::net::{Protocol, Receive};
use str0m::{
    Candidate, CandidateKind, Event as RtcEvent, IceConnectionState, IceCreds, Input, Output, Rtc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

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
/// How often to ask once they have: often enough to hold the NAT mapping
/// the record advertises open between sessions.
pub const STUN_REFRESH: Duration = Duration::from_secs(25);
/// The housekeeping interval: idle streams, setup deadlines, the gate.
pub const TICK: Duration = Duration::from_secs(1);
/// The host thread's stack. See the module note: str0m recurses once per
/// SCTP packet in a batch, and this is five times the measured worst case.
pub const HOST_STACK: usize = 16 * 1024 * 1024;

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
    /// Whether the record carries the host candidate (the LAN address).
    pub publish_host_candidates: bool,
    /// The label `hello` carries.
    pub service: String,
    pub default: Option<Entry>,
    pub scope: Scope,
    pub max_sessions: usize,
    pub max_streams: usize,
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
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
                        tokio::spawn(
                            async move { resolve_stun(servers, bound.is_ipv4(), tx).await },
                        );
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
fn candidate_ip(bound: SocketAddr) -> Result<IpAddr, String> {
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

async fn resolve_stun(servers: Vec<String>, v4: bool, tx: mpsc::UnboundedSender<Internal>) {
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
        tokio::time::sleep(STUN_REFRESH).await;
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
            if let Err(e) = s.rtc.handle_input(input) {
                s.end(&format!("rtc: {e}"));
            }
        }
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
            | Internal::TcpDone { key, .. } => *key,
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
                STUN_REFRESH
            };
        self.stun_pending
            .retain(|_, (_, sent)| now.duration_since(*sent) < STUN_REFRESH);
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

/// A server-reflexive line with its related address blanked, as browsers
/// write theirs: the relation is the LAN address, and a host that chose not
/// to publish its host candidate should not publish it here instead.
fn srflx_line(c: &Candidate, addr: &SocketAddr) -> String {
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
}

struct Stream {
    host: String,
    port: u16,
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

    fn on_wisp(&mut self, ctx: &Ctx, msg: &[u8]) {
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
        let host = match std::str::from_utf8(host) {
            Ok(h) if !h.is_empty() => h.to_string(),
            _ => return self.refuse(ctx, id, "", port, reason::INVALID),
        };
        if id == 0 || port == 0 {
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
        let Some(entry) = ctx.cfg.scope.allows(&host, port).cloned() else {
            return self.refuse(ctx, id, &host, port, reason::BLOCKED);
        };
        let internal = ctx.internal.clone();
        let gate = self.gate.subscribe();
        let (key, timeout) = (self.key, ctx.cfg.connect_timeout);
        let target = host.clone();
        let connect = tokio::spawn(async move {
            let msg = match dial(&entry, &target, port, timeout).await {
                Ok(tcp) => {
                    let (r, w) = tcp.into_split();
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

/// `hello` (§5): the scope, over the data channel and nowhere else.
fn hello(cfg: &HostConfig) -> String {
    let entry = |e: &Entry| serde_json::json!({"scheme": e.scheme, "host": e.host, "port": e.port});
    let mut msg = serde_json::json!({
        "v": 1,
        "t": "hello",
        "service": cfg.service,
        "scope": cfg.scope.entries.iter().map(entry).collect::<Vec<_>>(),
        "limits": {"max_streams": cfg.max_streams},
    });
    if let Some(d) = &cfg.default {
        msg["default"] = entry(d);
    }
    msg.to_string()
}

// depth: the TCP side of a stream

/// Resolve, check every address against the entry, connect to the first
/// that answers. The error is the Wisp close reason.
async fn dial(entry: &Entry, host: &str, port: u16, timeout: Duration) -> Result<TcpStream, u8> {
    let name = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
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
    mut w: tokio::net::tcp::OwnedWriteHalf,
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
    mut r: tokio::net::tcp::OwnedReadHalf,
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

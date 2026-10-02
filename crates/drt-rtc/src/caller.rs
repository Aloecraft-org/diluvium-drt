//! The caller, natively (`doc/BrowserAccess.md` §10.1): what a browser does
//! with `offer` and `accept`, done by a process, so `drt tunnel rtc:` can
//! carry `ssh` to a DRT host or to a page with no browser anywhere.
//!
//! A [`Caller`] binds one UDP socket and knows its own record before it
//! knows anyone else's, because in one shape of signaling (a `POST` of the
//! caller's record, answered with the answerer's) its record goes first.
//! [`Caller::connect`] takes the answerer's record and returns a [`Call`]
//! once both channels are open and the answerer's `hello` and credit have
//! arrived. [`Call::open`] then opens a Wisp stream, by name or by address,
//! and hands back its end as a [`DuplexStream`]: bytes in, bytes out, EOF
//! when the stream closes.
//!
//! One task owns the connection and every stream, as the host's does. The
//! caller opens odd stream ids (§10.2), spends the answerer's credit one
//! packet at a time, and serves nothing: a stream the answerer opens to it
//! is refused with `0x48`.
//!
//! ## surface block
//!
//! - Entry points: [`Caller::new`] (and [`Caller::direct`] for direct mode,
//!   §3.4), [`Caller::gather`], [`Caller::record`], [`Caller::connect`],
//!   [`Call::open`], [`Call::hello`], [`Call::control`] and
//!   [`Call::next_control`] (the `control` channel after `hello`), and
//!   [`Call::raw`], the Wisp channel as packets, for a relay that mirrors
//!   another session onto this one (`doc/P2P.md` §4.1).
//! - Configurable: [`PIPE`], [`CONNECT_TIMEOUT`], [`GATHER_TIMEOUT`].
//! - Fan-out: [`Target`], what a stream is opened to; [`Order`], what a
//!   [`Call`] asks of the task that owns the connection; the match over
//!   str0m's output in `Driver::drain`, and over Wisp packets in
//!   `Driver::on_wisp`.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;

use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::config::Fingerprint;
use str0m::net::{Protocol, Receive};
use str0m::{
    Candidate, CandidateKind, Event as RtcEvent, IceConnectionState, IceCreds, Input, Output, Rtc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::record::Record;
use crate::wisp::{self, reason, Packet};

/// The in-memory pipe between a stream and whoever holds its end, each way.
pub const PIPE: usize = 256 * 1024;
/// How long [`Caller::connect`] waits for the channels, `hello` and credit.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long [`Caller::gather`] waits for the STUN servers it asked. One
/// answer is usually here in a round trip; a server that is down should
/// not hold the call for long.
pub const GATHER_TIMEOUT: Duration = Duration::from_secs(2);

/// The largest datagram read off the socket.
const RECV_MTU: usize = 2000;
/// The negotiated channel ids (§4).
const CONTROL_ID: u16 = 0;
const WISP_ID: u16 = 1;
/// A DATA packet's payload: 16 KiB less the 5-byte header (§4, §6).
const DATA_MAX: usize = 16384 - 5;

/// What a stream is opened to (§6, §10.3, and `doc/P2P.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A named service the answerer serves: `ssh`.
    Service(String),
    /// An address in the answerer's scope.
    Address { host: String, port: u16 },
    /// Whatever the answerer forwards to: an empty host and port 0.
    Default,
    /// Whatever the answerer forwards to, at this port: an empty host.
    Port(u16),
}

impl Target {
    /// `ssh` is a service; `host:port` is an address; `:port` is a port
    /// of whatever the answerer forwards to, and nothing at all is that
    /// target itself.
    pub fn parse(s: &str) -> Result<Target, String> {
        if s.is_empty() {
            return Ok(Target::Default);
        }
        // Digits alone are a port of what the far side forwards to, as
        // `-p 8080:80` means. A service named by digits alone cannot be
        // asked for here; none is, and `drt+<service>://` can still say it.
        if s.bytes().all(|b| b.is_ascii_digit()) {
            return s
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .map(Target::Port)
                .ok_or_else(|| format!("'{s}': the port is not 1..65535"));
        }
        if crate::scope::is_service_name(s) {
            return Ok(Target::Service(s.to_string()));
        }
        let (host, port) = s
            .rsplit_once(':')
            .ok_or_else(|| format!("'{s}' is neither a service name nor host:port"))?;
        let port: u16 = port
            .parse()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| format!("'{s}': the port is not 1..65535"))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() {
            return Ok(Target::Port(port));
        }
        Ok(Target::Address {
            host: host.to_string(),
            port,
        })
    }

    fn connect_packet(&self, stream: u32) -> Vec<u8> {
        match self {
            Target::Service(name) => wisp::connect(stream, wisp::STREAM_TCP, 0, name),
            Target::Address { host, port } => wisp::connect(stream, wisp::STREAM_TCP, *port, host),
            Target::Default => wisp::connect(stream, wisp::STREAM_TCP, 0, ""),
            Target::Port(port) => wisp::connect(stream, wisp::STREAM_TCP, *port, ""),
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Service(name) => write!(f, "the service {name}"),
            Target::Address { host, port } => write!(f, "{host}:{port}"),
            Target::Default => f.write_str("what the far side forwards to"),
            Target::Port(port) => write!(f, "port {port} of what the far side forwards to"),
        }
    }
}

/// A caller that has bound its socket and made its record, and has not
/// met an answerer yet.
pub struct Caller {
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    record: Record,
}

impl Caller {
    /// Bind `bind` and make a record with str0m's own ICE credentials.
    pub async fn new(bind: SocketAddr) -> Result<Caller, String> {
        Caller::with_creds(bind, IceCreds::new()).await
    }

    /// For direct mode (§3.4): one value as ufrag and password, 32
    /// ice-chars, so a host with `direct` on makes a session from the
    /// first check and nothing goes back through signaling.
    pub async fn direct(bind: SocketAddr) -> Result<Caller, String> {
        let ufrag = random_ice_chars(32);
        Caller::with_creds(
            bind,
            IceCreds {
                ufrag: ufrag.clone(),
                pass: ufrag,
            },
        )
        .await
    }

    async fn with_creds(bind: SocketAddr, creds: IceCreds) -> Result<Caller, String> {
        let socket = UdpSocket::bind(bind)
            .await
            .map_err(|e| format!("rtc: cannot bind {bind}: {e}"))?;
        let bound = socket.local_addr().map_err(|e| format!("rtc: {e}"))?;
        let local = SocketAddr::new(crate::host::candidate_ip(bound)?, bound.port());
        let mut rtc = Rtc::builder()
            .set_local_ice_credentials(creds.clone())
            .set_ice_lite(false)
            .build(Instant::now().into_std());
        let candidate = Candidate::host(local, "udp").map_err(|e| format!("rtc: {local}: {e}"))?;
        rtc.add_local_candidate(candidate.clone());
        let fingerprint: [u8; 32] = rtc
            .direct_api()
            .local_dtls_fingerprint()
            .bytes
            .clone()
            .try_into()
            .map_err(|_| "rtc: the local certificate's fingerprint is not sha-256".to_string())?;
        let record = Record {
            ufrag: creds.ufrag,
            pwd: creds.pass,
            fingerprint,
            candidates: vec![candidate.to_sdp_string()],
        };
        Ok(Caller {
            rtc,
            socket,
            local,
            record,
        })
    }

    /// This caller's record, to send to the answerer through signaling
    /// (or not at all, in direct mode).
    pub fn record(&self) -> &Record {
        &self.record
    }

    /// Ask `servers` (`host:port`) for this socket's public address and
    /// add what they say to the record as server-reflexive candidates, so
    /// a peer behind another NAT has an address to reach (`doc/P2P.md`
    /// §2.5). Waits at most [`GATHER_TIMEOUT`], or until every server has
    /// answered. Returns the addresses found; a server that does not
    /// answer is left out, never an error, since the host candidate still
    /// stands.
    pub async fn gather(&mut self, servers: &[String]) -> Vec<SocketAddr> {
        use ego_transport::stun::{decode, encode_binding_request, StunMessage, TransactionId};
        let v4 = self.local.is_ipv4();
        let mut pending: HashMap<[u8; 12], SocketAddr> = HashMap::new();
        for server in servers {
            let Ok(addrs) = tokio::net::lookup_host(server.as_str()).await else {
                continue;
            };
            for addr in addrs.filter(|a| a.is_ipv4() == v4).take(1) {
                let txid = TransactionId::random();
                if self
                    .socket
                    .send_to(&encode_binding_request(&txid), addr)
                    .await
                    .is_ok()
                {
                    pending.insert(*txid.as_bytes(), addr);
                }
            }
        }
        let mut found: Vec<SocketAddr> = Vec::new();
        let deadline = Instant::now() + GATHER_TIMEOUT;
        let mut buf = vec![0u8; RECV_MTU];
        while !pending.is_empty() {
            let Ok(Ok((n, _))) =
                tokio::time::timeout_at(deadline, self.socket.recv_from(&mut buf)).await
            else {
                break;
            };
            let Ok(StunMessage::BindingSuccess { txid, mapped }) = decode(&buf[..n]) else {
                continue;
            };
            if pending.remove(txid.as_bytes()).is_none() {
                continue;
            }
            if mapped == self.local || found.contains(&mapped) {
                continue;
            }
            if let Ok(c) = Candidate::server_reflexive(mapped, self.local, "udp") {
                self.rtc.add_local_candidate(c.clone());
                self.record
                    .candidates
                    .push(crate::host::srflx_line(&c, &mapped));
                found.push(mapped);
            }
        }
        found
    }

    /// Connect to the answerer whose record this is. Resolves once both
    /// channels are open and its `hello` and initial credit have arrived.
    pub async fn connect(self, answerer: &Record, timeout: Duration) -> Result<Call, String> {
        let Caller {
            mut rtc,
            socket,
            local,
            ..
        } = self;
        let mut api = rtc.direct_api();
        api.set_ice_controlling(true);
        api.set_remote_ice_credentials(IceCreds {
            ufrag: answerer.ufrag.clone(),
            pass: answerer.pwd.clone(),
        });
        api.set_remote_fingerprint(Fingerprint {
            hash_func: "sha-256".into(),
            bytes: answerer.fingerprint.to_vec(),
        });
        api.start_dtls(true).map_err(|e| format!("rtc: {e}"))?;
        api.start_sctp(true);
        let channel = |label: &str, id: u16| ChannelConfig {
            label: label.into(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: Some(id),
            protocol: String::new(),
        };
        let control = api.create_data_channel(channel("control", CONTROL_ID));
        let wisp = api.create_data_channel(channel("wisp", WISP_ID));
        for line in &answerer.candidates {
            if let Ok(c) = Candidate::from_sdp_string(line) {
                if c.proto() == Protocol::Udp && c.kind() != CandidateKind::Relayed {
                    rtc.add_remote_candidate(c);
                }
            }
        }
        let (orders, order_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready) = oneshot::channel();
        let (control_out, control_in) = mpsc::unbounded_channel();
        let driver = Driver {
            rtc,
            socket,
            local,
            control,
            wisp,
            open: [false; 2],
            hello: None,
            credit: None,
            ready: Some(ready_tx),
            streams: HashMap::new(),
            next_id: 1,
            outbox: VecDeque::new(),
            ended: None,
            control_out,
            raw: None,
            peer_half_close: false,
        };
        tokio::spawn(driver.run(order_rx, orders.downgrade()));
        match tokio::time::timeout(timeout, ready).await {
            Ok(Ok(Ok(hello))) => Ok(Call {
                orders,
                hello,
                control: tokio::sync::Mutex::new(control_in),
            }),
            Ok(Ok(Err(why))) => Err(why),
            Ok(Err(_)) => Err("rtc: the connection ended before it was ready".into()),
            Err(_) => Err(format!(
                "rtc: no session within {}s: the answerer did not answer, or no path reached it",
                timeout.as_secs()
            )),
        }
    }
}

/// A connected session, from the caller's side.
pub struct Call {
    orders: mpsc::UnboundedSender<Order>,
    hello: String,
    /// `control` messages after `hello`, oldest first.
    control: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
}

/// The Wisp channel as packets, from [`Call::raw`]: what arrives, and a
/// way to send. Once taken, the [`Call`] opens no streams of its own.
pub struct Raw {
    pub incoming: mpsc::UnboundedReceiver<Vec<u8>>,
    pub outgoing: mpsc::UnboundedSender<Vec<u8>>,
}

impl Call {
    /// The answerer's `hello` (§5), as it sent it.
    pub fn hello(&self) -> &str {
        &self.hello
    }

    /// Whether the answerer understands half-close (`END`): a holder that
    /// shuts down its write side then still reads what the far side has
    /// to say, as with TCP. Without it, closing the write side closes the
    /// stream.
    pub fn half_close(&self) -> bool {
        serde_json::from_str::<serde_json::Value>(&self.hello)
            .map(|h| h["half_close"] == true)
            .unwrap_or(false)
    }

    /// Send one message on `control`: one JSON object, as §5 has them.
    pub fn control(&self, text: &str) -> Result<(), String> {
        self.orders
            .send(Order::Control(text.to_string()))
            .map_err(|_| "rtc: the session has ended".to_string())
    }

    /// The next `control` message after `hello`, or `None` once the
    /// session has ended.
    pub async fn next_control(&self) -> Option<String> {
        self.control.lock().await.recv().await
    }

    /// The Wisp channel as packets. A relay (`doc/P2P.md` §4.1) takes it to
    /// mirror a caller's packets onto this session and this session's back,
    /// reading none of them; credit then belongs to the two ends.
    pub async fn raw(&self) -> Result<Raw, String> {
        let (reply, answer) = oneshot::channel();
        self.orders
            .send(Order::Raw { reply })
            .map_err(|_| "rtc: the session has ended".to_string())?;
        answer
            .await
            .map_err(|_| "rtc: the session has ended".to_string())
    }

    /// Open a stream to `target` and hand back this side's end of it,
    /// with a receiver that gets the CLOSE byte when the stream ends.
    /// Usable at once: Wisp v1 has no "connected" packet, so a refusal is
    /// the stream ending with the answerer's reason (`0x48` for a name it
    /// does not serve).
    pub async fn open(
        &self,
        target: &Target,
    ) -> Result<(DuplexStream, oneshot::Receiver<u8>), String> {
        let (reply, answer) = oneshot::channel();
        self.orders
            .send(Order::Open {
                target: target.clone(),
                reply,
            })
            .map_err(|_| "rtc: the session has ended".to_string())?;
        answer
            .await
            .map_err(|_| "rtc: the session has ended".to_string())?
    }
}

/// What a [`Call`] asks of the task that owns the connection.
enum Order {
    Open {
        target: Target,
        reply: oneshot::Sender<Result<(DuplexStream, oneshot::Receiver<u8>), String>>,
    },
    /// Bytes the stream's holder wrote.
    Data { stream: u32, bytes: Vec<u8> },
    /// The holder closed its end.
    Eof { stream: u32 },
    /// One message for `control`.
    Control(String),
    /// Hand the Wisp channel over as packets.
    Raw { reply: oneshot::Sender<Raw> },
    /// A packet to send as is, from [`Raw::outgoing`].
    Packet(Vec<u8>),
}

// depth: the task that owns the connection

struct Stream {
    /// Credit left: packets of DATA the answerer will still take (§6).
    remaining: u32,
    /// DATA waiting for credit, oldest first.
    queued: VecDeque<Vec<u8>>,
    /// To the task writing into this side's end of the pipe; `None` once
    /// the answerer sent `END`, which ends the holder's reads.
    to_holder: Option<mpsc::UnboundedSender<Vec<u8>>>,
    /// Why it closed, once it has: the CLOSE byte.
    closed: Option<oneshot::Sender<u8>>,
    /// The holder closed its write side and `END` went to the answerer.
    ended_out: bool,
}

struct Driver {
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    control: ChannelId,
    wisp: ChannelId,
    open: [bool; 2],
    hello: Option<String>,
    credit: Option<u32>,
    ready: Option<oneshot::Sender<Result<String, String>>>,
    streams: HashMap<u32, Stream>,
    next_id: u32,
    /// Packets str0m refused for want of buffer space, oldest first.
    outbox: VecDeque<Vec<u8>>,
    ended: Option<String>,
    /// `control` messages after `hello`, to whoever holds the `Call`.
    control_out: mpsc::UnboundedSender<String>,
    /// Set once [`Call::raw`] took the channel: packets go here as they are.
    raw: Option<mpsc::UnboundedSender<Vec<u8>>>,
    /// The answerer's `hello` said `half_close`: a holder's end of writes
    /// is `END`, and the answerer's `END` ends the holder's reads.
    peer_half_close: bool,
}

impl Driver {
    async fn run(
        mut self,
        mut orders: mpsc::UnboundedReceiver<Order>,
        own: mpsc::WeakUnboundedSender<Order>,
    ) {
        let mut buf = vec![0u8; RECV_MTU];
        loop {
            let wake = self.drain();
            if let Some(why) = self.ended.take() {
                if let Some(ready) = self.ready.take() {
                    let _ = ready.send(Err(why));
                }
                for (_, s) in self.streams.drain() {
                    if let Some(c) = s.closed {
                        let _ = c.send(reason::NETWORK_ERROR);
                    }
                }
                return;
            }
            let wait = wake
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            tokio::select! {
                r = self.socket.recv_from(&mut buf) => {
                    if let Ok((n, source)) = r {
                        if let Ok(contents) = buf[..n].try_into() {
                            let input = Input::Receive(
                                Instant::now().into_std(),
                                Receive { proto: Protocol::Udp, source, destination: self.local, contents },
                            );
                            if let Err(e) = self.rtc.handle_input(input) {
                                self.ended = Some(format!("rtc: {e}"));
                            }
                        }
                    }
                }
                o = orders.recv() => match o {
                    Some(o) => self.on_order(o, &own),
                    // Every Call and every stream is gone: nobody is left.
                    None => return,
                },
                _ = tokio::time::sleep(wait) => {
                    if let Err(e) = self.rtc.handle_input(Input::Timeout(Instant::now().into_std())) {
                        self.ended = Some(format!("rtc: {e}"));
                    }
                }
            }
        }
    }

    /// Run str0m until it only wants to be woken later; say when.
    fn drain(&mut self) -> Instant {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => {
                    self.flush();
                    return Instant::from_std(t);
                }
                Ok(Output::Transmit(t)) => {
                    let _ = self.socket.try_send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(e)) => self.on_event(e),
                Err(e) => {
                    self.ended = Some(format!("rtc: {e}"));
                    return Instant::now();
                }
            }
        }
    }

    fn on_event(&mut self, e: RtcEvent) {
        match e {
            RtcEvent::ChannelOpen(id, _) if id == self.control => self.open[0] = true,
            RtcEvent::ChannelOpen(id, _) if id == self.wisp => self.open[1] = true,
            RtcEvent::ChannelData(d) if d.id == self.control => {
                let text = String::from_utf8_lossy(&d.data).into_owned();
                if self.hello.is_none() {
                    self.peer_half_close = serde_json::from_str::<serde_json::Value>(&text)
                        .map(|h| h["half_close"] == true)
                        .unwrap_or(false);
                    self.hello = Some(text);
                    // Say what this side understands, so the answerer
                    // sends END rather than CLOSE for a target's end of
                    // stream (`doc/BrowserAccess.md` §6).
                    if let Some(mut ch) = self.rtc.channel(self.control) {
                        let _ = ch.write(false, br#"{"t":"features","half_close":true}"#);
                    }
                } else {
                    let _ = self.control_out.send(text);
                }
            }
            RtcEvent::ChannelData(d) if d.id == self.wisp => match &self.raw {
                Some(raw) => {
                    let _ = raw.send(d.data.clone());
                }
                None => self.on_wisp(&d.data),
            },
            RtcEvent::ChannelClose(_) => self.ended = Some("rtc: a data channel closed".into()),
            RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                self.ended = Some("rtc: ice disconnected".into())
            }
            RtcEvent::ChannelBufferedAmountLow(_) => self.flush(),
            _ => {}
        }
        if self.open == [true, true] && self.credit.is_some() {
            if let (Some(ready), Some(hello)) =
                (self.ready.take_if(|_| self.hello.is_some()), &self.hello)
            {
                let _ = ready.send(Ok(hello.clone()));
            }
        }
    }

    fn on_wisp(&mut self, msg: &[u8]) {
        match wisp::parse(msg) {
            Some(Packet::Continue {
                stream: 0,
                remaining,
            }) => {
                if self.credit.is_none() {
                    self.credit = Some(remaining);
                }
            }
            Some(Packet::Continue { stream, remaining }) => {
                if let Some(s) = self.streams.get_mut(&stream) {
                    s.remaining = remaining;
                }
                self.pump(stream);
            }
            Some(Packet::Data { stream, payload }) => {
                if let Some(Some(to_holder)) = self.streams.get(&stream).map(|s| &s.to_holder) {
                    let _ = to_holder.send(payload.to_vec());
                }
            }
            Some(Packet::End { stream }) => {
                // The answerer will write no more: the holder reads end of
                // file (dropping the sender ends the writer, which shuts
                // its end down) and may still write. Both halves ended
                // closes the stream.
                let Some(s) = self.streams.get_mut(&stream) else {
                    return;
                };
                s.to_holder = None;
                if s.ended_out {
                    if let Some(mut s) = self.streams.remove(&stream) {
                        if let Some(c) = s.closed.take() {
                            let _ = c.send(reason::VOLUNTARY);
                        }
                    }
                    self.send(wisp::close(stream, reason::VOLUNTARY));
                }
            }
            Some(Packet::Close {
                stream,
                reason: code,
            }) => {
                if let Some(mut s) = self.streams.remove(&stream) {
                    if let Some(c) = s.closed.take() {
                        let _ = c.send(code);
                    }
                }
            }
            // This side serves nothing (§10.2): whatever the answerer opens
            // is refused as a name it does not serve.
            Some(Packet::Connect { stream, .. }) => self.send(wisp::close(stream, reason::BLOCKED)),
            _ => {}
        }
    }

    fn on_order(&mut self, o: Order, own: &mpsc::WeakUnboundedSender<Order>) {
        match o {
            Order::Open { target, reply } => {
                let _ = reply.send(self.open_stream(&target, own));
            }
            Order::Data { stream, bytes } => {
                if let Some(s) = self.streams.get_mut(&stream) {
                    for chunk in bytes.chunks(DATA_MAX) {
                        s.queued.push_back(wisp::data(stream, chunk));
                    }
                }
                self.pump(stream);
            }
            Order::Eof { stream } => {
                // The holder closed its write side. To an answerer that
                // understands it, END, and its answer still arrives; to
                // one that does not, the stream closes here.
                let half = self.peer_half_close
                    && self
                        .streams
                        .get(&stream)
                        .is_some_and(|s| s.to_holder.is_some() && !s.ended_out);
                if half {
                    if let Some(s) = self.streams.get_mut(&stream) {
                        s.ended_out = true;
                    }
                    self.send(wisp::end(stream));
                } else if let Some(mut s) = self.streams.remove(&stream) {
                    if let Some(c) = s.closed.take() {
                        let _ = c.send(reason::VOLUNTARY);
                    }
                    self.send(wisp::close(stream, reason::VOLUNTARY));
                }
            }
            Order::Control(text) => {
                if let Some(mut ch) = self.rtc.channel(self.control) {
                    let _ = ch.write(false, text.as_bytes());
                }
            }
            Order::Raw { reply } => {
                let (incoming_tx, incoming) = mpsc::unbounded_channel();
                let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                self.raw = Some(incoming_tx);
                if let Some(own) = own.upgrade() {
                    tokio::spawn(async move {
                        while let Some(pkt) = outgoing_rx.recv().await {
                            if own.send(Order::Packet(pkt)).is_err() {
                                break;
                            }
                        }
                    });
                }
                let _ = reply.send(Raw { incoming, outgoing });
            }
            Order::Packet(pkt) => self.send(pkt),
        }
    }

    fn open_stream(
        &mut self,
        target: &Target,
        own: &mpsc::WeakUnboundedSender<Order>,
    ) -> Result<(DuplexStream, oneshot::Receiver<u8>), String> {
        let credit = self
            .credit
            .ok_or("rtc: the answerer serves nothing: it sent no credit")?;
        let orders = own.upgrade().ok_or("rtc: the session has ended")?;
        let id = self.next_id;
        self.next_id += 2;
        let (ours, theirs) = tokio::io::duplex(PIPE);
        let (mut from_holder, mut to_holder) = tokio::io::split(ours);
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if to_holder.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            let _ = to_holder.shutdown().await;
        });
        tokio::spawn(async move {
            let mut buf = vec![0u8; DATA_MAX];
            loop {
                match from_holder.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        let _ = orders.send(Order::Eof { stream: id });
                        break;
                    }
                    Ok(n) => {
                        if orders
                            .send(Order::Data {
                                stream: id,
                                bytes: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        let (closed_tx, closed) = oneshot::channel();
        self.streams.insert(
            id,
            Stream {
                remaining: credit,
                queued: VecDeque::new(),
                to_holder: Some(tx),
                closed: Some(closed_tx),
                ended_out: false,
            },
        );
        self.send(target.connect_packet(id));
        Ok((theirs, closed))
    }

    /// Send what a stream has queued, as far as its credit goes.
    fn pump(&mut self, stream: u32) {
        let Some(s) = self.streams.get_mut(&stream) else {
            return;
        };
        let mut out = Vec::new();
        while s.remaining > 0 {
            let Some(pkt) = s.queued.pop_front() else {
                break;
            };
            s.remaining -= 1;
            out.push(pkt);
        }
        for pkt in out {
            self.send(pkt);
        }
    }

    fn send(&mut self, pkt: Vec<u8>) {
        self.outbox.push_back(pkt);
        self.flush();
    }

    /// Write what the outbox holds, in order, until str0m refuses one.
    fn flush(&mut self) {
        if !self.open[1] {
            return;
        }
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
}

/// `n` random ice-chars (RFC 8839: ALPHA, DIGIT, `+`, `/`), from str0m's
/// own credential generator, which draws from the OS.
fn random_ice_chars(n: usize) -> String {
    let mut out = String::new();
    while out.len() < n {
        out.push_str(&IceCreds::new().pass);
    }
    out.retain(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/');
    while out.len() < n {
        out.push_str(
            &IceCreds::new()
                .pass
                .replace(|c: char| !c.is_ascii_alphanumeric(), ""),
        );
    }
    out.truncate(n);
    out
}

//! The http listener: `dhost_http.c`'s contract, as a queue bridge.
//!
//! The listener is a **queue bridge**. A request becomes one msgpack map on
//! a named root queue; the program answers with one map on a reply queue;
//! nothing else crosses. The shapes are the C host's, field for field, so
//! the same fetchpoint Lua runs on either host:
//!
//! ```text
//! request:  {conn, method, path, body, headers?}     -> the `queue`
//! reply:    {conn, status?, body?, content_type?, headers?} <- `reply_queue`
//! ```
//!
//! `path` is the whole request-target — query string included; parsing
//! `?format=` is the program's business. `headers` is present only when the
//! listener's allowlist is non-empty, and carries only allowlisted names
//! that arrived, under the allowlist's own lowercased spelling. **A header
//! the deployment does not name never reaches the program** — that is the
//! design, not an accident of parsing.
//!
//! The C's protocol decisions are kept because each closes a hole:
//!
//! - One request per connection, `Connection: close`, no keep-alive. The
//!   edge (nginx) holds the client connections; this side is the LB's side.
//! - `Transfer-Encoding` is refused outright: a parser that disagrees with
//!   the LB about where a body ends is the request-smuggling primitive, so
//!   chunked bodies are not spoken here — the LB buffers.
//! - `Content-Length` is parsed strictly — digits only, no duplicates —
//!   for the same reason.
//! - Reply headers and `content_type` are dropped whole (never truncated,
//!   never cleaned) on any control byte: a guest is untrusted, and its
//!   bytes are interpolated into a response the client's parser trusts.
//!   Dropping rather than refusing keeps a misbehaving guest's bug from
//!   becoming an outage; absence is the safe default.
//! - A reply naming no waiting connection is consumed all the same: the
//!   connection may have hit its deadline first, and a wedged reply queue
//!   would take every later response with it.
//!
//! ## A request may arrive before the program is ready for it
//!
//! The socket accepts from the moment the process binds it, which is
//! before the program has run a line — so a request can arrive naming a
//! queue the program has not declared yet. Refusing it there is a
//! definitive answer to a question the deployment has not finished
//! hearing, and issue #11 is what that costs: prosody's `mod_rest` probes
//! its component **once** at startup and settles on a wire format for the
//! life of the process, so a refusal in a 60 ms window leaves two healthy
//! services that never exchange a message and neither log saying why.
//!
//! So the request waits, for [`ListenerRt::admit`] — but the waiting is
//! `start`'s, not the acceptor's, because whether a queue exists is a
//! question only the deployment can answer. What lives here is the
//! duration and the connection's own deadline; `start::retry_held` is the
//! wait itself.
//!
//! Two acceptors, one bridge (doc/Wasm.md M6). Natively a thread per
//! connection over blocking sockets, with the ingress channel doubling as
//! the drive loop's idle wait so a request never waits on a tick. On wasi
//! — sockets but no threads — the same bridge over non-blocking sockets
//! the drive loop steps itself, every [`POLL_TICK`] while idle. What a
//! request means and what a reply says is decided once, in the pure
//! functions both share; the two differ only in how bytes arrive and
//! leave, and the polled one is compiled and tested natively too.
//!
//! ## surface block
//!
//! - [`bind`]: every configured listener, bound, as this target's
//!   [`Bound`] — [`threaded::Bound`] natively, [`polled::Bound`] on wasi.
//! - [`Acceptor`]: what the drive loop asks of a bound listener set.
//! - [`Ingress`], [`Outcome`], [`ListenerRt`]: the bridge's types.
//! - [`reply_token`], [`parse_reply`]: the reply side, pure.
//! - [`ListenerRt::deadline`], [`ListenerRt::admit`]: the two waits a
//!   request can spend — for the program, and for its queue to exist.
//! - [`HDR_ROOM`], [`HDR_VALUE_MAX`], [`MAX_HEADERS`]: the C's bounds.
//! - [`POLL_TICK`]: how often the polled acceptor looks while idle.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use drt_config::Listener;

/// The C's bounds, kept: header block room, one header value, headers per
/// request.
pub const HDR_ROOM: usize = 8192;
pub const HDR_VALUE_MAX: usize = 4096;
pub const MAX_HEADERS: usize = 32;
/// The polled acceptor's cadence while it waits: a request arriving
/// between looks waits at most this long, which is the drive loop's own
/// idle tick.
pub const POLL_TICK: Duration = Duration::from_millis(1);

#[cfg(not(target_os = "wasi"))]
pub type Bound = threaded::Bound;
#[cfg(target_os = "wasi")]
pub type Bound = polled::Bound;

/// Bind every configured listener and start accepting. Fails whole: a
/// deployment with three listeners and two ports is not almost-serving.
pub fn bind(configs: &[Listener]) -> Result<Bound, String> {
    Bound::bind(configs)
}

/// What the drive loop asks of a bound listener set: requests out,
/// answers in, and the idle wait — which is the same call as the wait for
/// a request, so a request never waits on a tick.
pub trait Acceptor {
    fn listeners(&self) -> &[Arc<ListenerRt>];
    fn addrs(&self) -> &[SocketAddr];
    /// A request already parsed and waiting, if any — never blocks.
    fn try_next(&mut self) -> Option<Ingress>;
    /// Wait for the next request, up to `timeout`.
    fn next_within(&mut self, timeout: Duration) -> Option<Ingress>;
    /// Answer the connection a token names. A token nobody is waiting on
    /// is not an error — the connection may have hit its deadline first.
    fn answer(&mut self, token: u32, outcome: Outcome);
    /// Which listener a waiting token belongs to, for the reply filter.
    fn owner_of(&self, token: u32) -> Option<usize>;
}

/// One parsed request, on its way to the drive loop.
pub struct Ingress {
    /// Which listener it arrived on — the reply filter is per-listener.
    pub listener: usize,
    /// The `conn` token, host-wide unique.
    pub token: u32,
    /// The encoded request map, ready for the root queue.
    pub message: Vec<u8>,
}

/// What the drive loop sends back to a waiting connection.
pub enum Outcome {
    /// The program answered.
    Reply {
        status: u16,
        content_type: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// The host refused on the program's behalf, with the C's texts.
    Refused { status: u16, text: &'static str },
}

impl Outcome {
    pub fn refused(status: u16, text: &'static str) -> Outcome {
        Outcome::Refused { status, text }
    }
}

/// One listener as configured, validated: what both acceptors serve.
pub struct ListenerRt {
    pub queue: String,
    pub reply_queue: String,
    /// Lowercased request-header allowlist, in config order.
    pub hdr_allow: Vec<String>,
    /// Lowercased response-header allowlist, in config order — the wire
    /// gets this spelling, so guest bytes never name a header.
    pub resp_allow: Vec<String>,
    max_body: usize,
    deadline: Duration,
    max_conns: usize,
    admit: Duration,
}

impl ListenerRt {
    fn new(idx: usize, cfg: &Listener) -> Result<Self, String> {
        if cfg.scheme != "http" {
            return Err(format!(
                "listener {idx} speaks '{}', and only 'http' is served today \
                 ('ssh' lands with the control endpoint)",
                cfg.scheme
            ));
        }
        if cfg.headers.len() > 16 || cfg.resp_headers.len() > 16 {
            return Err(format!(
                "listener {idx} allowlists more than 16 headers in one \
                 direction; the bound is the C host's, and a list that long \
                 is a sign the boundary is dissolving"
            ));
        }
        Ok(ListenerRt {
            queue: cfg.queue.clone(),
            reply_queue: cfg.reply_queue.clone(),
            hdr_allow: cfg.headers.iter().map(|h| h.to_lowercase()).collect(),
            resp_allow: cfg.resp_headers.iter().map(|h| h.to_lowercase()).collect(),
            max_body: cfg.max_body,
            deadline: Duration::from_millis(cfg.conn_deadline_ms),
            max_conns: cfg.max_conns,
            admit: Duration::from_millis(cfg.admit_timeout_ms),
        })
    }

    /// How long a connection may take at each stage — reading, waiting
    /// for the program, being written to.
    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// How long a request waits for [`ListenerRt::queue`] to exist before
    /// the host refuses on the program's behalf. The drive loop owns this
    /// wait, not the acceptor: whether the queue exists is a question only
    /// the deployment can answer.
    pub fn admit(&self) -> Duration {
        self.admit
    }
}

// ---------------------------------------------------------------------------
// depth: the request side, pure — one buffer in, one verdict out
// ---------------------------------------------------------------------------

/// What [`parse_request`] made of the bytes so far.
enum Parsed {
    /// The whole request: its token and the encoded map for the queue.
    Complete { token: u32, message: Vec<u8> },
    /// More bytes are needed. `headers_done` chooses the refusal text if
    /// the connection ends instead of sending them.
    Incomplete { headers_done: bool },
}

/// The refusal for a connection that ended, or ran out its deadline,
/// before its request was whole.
fn cut_short(headers_done: bool) -> (u16, &'static str) {
    if headers_done {
        (400, "the connection went away mid-body\n")
    } else {
        (400, "unparseable request\n")
    }
}

/// Validate what has arrived and, once the request is whole, build the
/// request map. The refusals and their texts are `dhost_http.c`'s. `mint`
/// is called exactly once, when the request is complete.
fn parse_request(
    buf: &[u8],
    rt: &ListenerRt,
    mint: &mut dyn FnMut() -> u32,
) -> Result<Parsed, (u16, &'static str)> {
    // Parsed from scratch each pass — httparse borrows from the buffer,
    // so headers cannot outlive the read that may reallocate it.
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut headers);
    let header_len = match req.parse(buf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => {
            if buf.len() > HDR_ROOM {
                return Err((400, "headers past this host's room\n"));
            }
            return Ok(Parsed::Incomplete {
                headers_done: false,
            });
        }
        Err(_) => return Err((400, "unparseable request\n")),
    };
    let method = req.method.unwrap_or("").to_string();
    let path = req.path.unwrap_or("").to_string();

    // Content-Length, strictly: digits only, at most one. A parser that
    // disagrees with the LB about where the body ends is the
    // request-smuggling primitive.
    let mut clen: usize = 0;
    let mut seen_clen = false;
    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case("content-length") {
            if seen_clen || h.value.is_empty() {
                return Err((400, "a malformed or duplicated Content-Length\n"));
            }
            let mut v: usize = 0;
            for &d in h.value {
                if !d.is_ascii_digit() {
                    return Err((400, "Content-Length must be digits only\n"));
                }
                v = v
                    .checked_mul(10)
                    .and_then(|v| v.checked_add((d - b'0') as usize))
                    .ok_or((413, "Content-Length overflows\n"))?;
            }
            clen = v;
            seen_clen = true;
        }
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            return Err((
                400,
                "chunked bodies are not spoken here; the LB should buffer\n",
            ));
        }
    }
    if clen > rt.max_body {
        return Err((413, "body past the configured cap\n"));
    }

    // The allowlisted headers that arrived, in allowlist order, under the
    // allowlist's spelling. Repeats join with ", ", as the C host joins
    // them: a second x-df-sub arrives concatenated to the first rather
    // than as something the program could mistake for the gateway's own.
    // The gateway's set-header replaces, so a join is only ever visible
    // when something upstream misbehaved — visibly.
    let mut present: Vec<(String, rmpv::Value)> = Vec::new();
    for name in &rt.hdr_allow {
        let mut joined: Vec<u8> = Vec::new();
        for h in req
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name))
        {
            if h.value.is_empty() {
                continue;
            }
            if !joined.is_empty() {
                joined.extend_from_slice(b", ");
            }
            joined.extend_from_slice(h.value);
            if joined.len() > HDR_VALUE_MAX {
                return Err((
                    431,
                    "an allowlisted header is past this host's value bound\n",
                ));
            }
        }
        if !joined.is_empty() {
            present.push((name.clone(), bytes_value(&joined)));
        }
    }

    // The body may still be arriving.
    if buf.len() < header_len + clen {
        return Ok(Parsed::Incomplete { headers_done: true });
    }
    let body = &buf[header_len..header_len + clen];

    let token = mint();
    let mut map = vec![
        ("conn".into(), rmpv::Value::from(token as u64)),
        ("method".into(), rmpv::Value::from(method)),
        ("path".into(), rmpv::Value::from(path)),
        ("body".into(), rmpv::Value::Binary(body.to_vec())),
    ];
    if !rt.hdr_allow.is_empty() {
        map.push((
            "headers".into(),
            rmpv::Value::Map(
                present
                    .into_iter()
                    .map(|(k, v)| (rmpv::Value::from(k), v))
                    .collect(),
            ),
        ));
    }
    let mut message = Vec::new();
    rmpv::encode::write_value(&mut message, &rmpv::Value::Map(map))
        .map_err(|_| (500u16, "encoding failed\n"))?;
    Ok(Parsed::Complete { token, message })
}

/// A guest string: `dmsgpack.c` decodes `bin` and `str` identically into a
/// Lua string, so bytes that happen not to be UTF-8 lose nothing.
fn bytes_value(bytes: &[u8]) -> rmpv::Value {
    match std::str::from_utf8(bytes) {
        Ok(s) => rmpv::Value::from(s),
        Err(_) => rmpv::Value::Binary(bytes.to_vec()),
    }
}

// ---------------------------------------------------------------------------
// depth: the reply side, pure — one guest map -> one Outcome
// ---------------------------------------------------------------------------

fn field<'a>(map: &'a rmpv::Value, name: &str) -> Option<&'a rmpv::Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

fn clean(value: &[u8]) -> bool {
    !value.iter().any(|&b| b < 0x20 || b == 0x7f)
}

fn str_bytes(v: &rmpv::Value) -> Option<&[u8]> {
    match v {
        rmpv::Value::String(s) => Some(s.as_bytes()),
        rmpv::Value::Binary(b) => Some(b),
        _ => None,
    }
}

/// The token a reply names, or 0 for a reply that names none.
pub fn reply_token(raw: &[u8]) -> u32 {
    let value = rmpv::decode::read_value(&mut &raw[..]).unwrap_or(rmpv::Value::Nil);
    field(&value, "conn").and_then(|v| v.as_u64()).unwrap_or(0) as u32
}

/// Parse one reply from the queue into what the connection gets. The
/// permissive reads are the C's: a missing or ill-typed field takes its
/// default, because the reply already left the program and refusing it
/// answers nobody.
pub fn parse_reply(raw: &[u8], owner: &ListenerRt) -> Outcome {
    let value = rmpv::decode::read_value(&mut &raw[..]).unwrap_or(rmpv::Value::Nil);
    let status = field(&value, "status")
        .and_then(|v| v.as_i64())
        .filter(|s| (100..=599).contains(s))
        .unwrap_or(200) as u16;
    let body = field(&value, "body")
        .and_then(str_bytes)
        .map(|b| b.to_vec())
        .unwrap_or_default();
    // The guest is untrusted and this string is interpolated into the
    // response head. A control byte would inject headers; a media type has
    // no control characters, so any is disqualifying — keep the default
    // rather than a sanitized guess at what the guest meant.
    let content_type = field(&value, "content_type")
        .and_then(str_bytes)
        .filter(|b| b.len() < 128 && clean(b))
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    // Reply headers against the owner's allowlist: unknown name, control
    // bytes, or oversize loses the header whole. Names match
    // case-insensitively; the wire gets the allowlist's spelling; output
    // order is the allowlist's, so it is config's. Last repeat wins.
    let mut chosen: Vec<Option<Vec<u8>>> = vec![None; owner.resp_allow.len()];
    if let Some(rmpv::Value::Map(entries)) = field(&value, "headers") {
        for (k, v) in entries {
            let Some(name) = k.as_str() else { continue };
            let Some(slot) = owner
                .resp_allow
                .iter()
                .position(|a| a.eq_ignore_ascii_case(name))
            else {
                continue;
            };
            let Some(bytes) = str_bytes(v) else { continue };
            if bytes.len() > HDR_VALUE_MAX || !clean(bytes) {
                continue;
            }
            chosen[slot] = Some(bytes.to_vec());
        }
    }
    let headers = owner
        .resp_allow
        .iter()
        .zip(chosen)
        .filter_map(|(name, v)| {
            v.and_then(|v| String::from_utf8(v).ok())
                .map(|v| (name.clone(), v))
        })
        .collect();
    Outcome::Reply {
        status,
        content_type,
        headers,
        body,
    }
}

// ---------------------------------------------------------------------------
// depth: the response bytes — the C's head, byte for byte in shape
// ---------------------------------------------------------------------------

fn reason_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

fn response_bytes(
    status: u16,
    content_type: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\n",
        reason_for(status)
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

fn refusal_bytes(status: u16, text: &str) -> Vec<u8> {
    response_bytes(status, "text/plain", &[], text.as_bytes())
}

fn outcome_bytes(outcome: &Outcome) -> Vec<u8> {
    match outcome {
        Outcome::Reply {
            status,
            content_type,
            headers,
            body,
        } => response_bytes(*status, content_type, headers, body),
        Outcome::Refused { status, text } => refusal_bytes(*status, text),
    }
}

// ---------------------------------------------------------------------------
// depth: the threaded acceptor — native
// ---------------------------------------------------------------------------

/// A thread per connection over blocking sockets; requests arrive on one
/// channel, and waiting connections are reachable by token.
#[cfg(not(target_os = "wasi"))]
pub mod threaded {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, Sender, SyncSender};
    use std::sync::Mutex;

    struct Waiting {
        tx: SyncSender<Outcome>,
        listener: usize,
    }

    pub struct Bound {
        listeners: Vec<Arc<ListenerRt>>,
        ingress: Receiver<Ingress>,
        /// Held only so the channel cannot disconnect. Every acceptor
        /// thread owns a clone, so with at least one listener this is
        /// redundant — but a deployment with NO listeners (a relay-only
        /// fetchpoint) has no acceptor at all, and without this the last
        /// sender drops when `bind` returns. `recv_timeout` on a
        /// disconnected channel returns at once rather than waiting, which
        /// turns the drive loop's idle sleep into a spin that burns a core
        /// forever.
        _keepalive: Sender<Ingress>,
        waiting: Arc<Mutex<HashMap<u32, Waiting>>>,
        addrs: Vec<SocketAddr>,
    }

    impl Bound {
        pub fn bind(configs: &[Listener]) -> Result<Bound, String> {
            let (tx, rx) = std::sync::mpsc::channel();
            let waiting = Arc::new(Mutex::new(HashMap::new()));
            let tokens = Arc::new(AtomicU32::new(1));
            let mut listeners = Vec::new();
            let mut addrs = Vec::new();
            for (idx, cfg) in configs.iter().enumerate() {
                let rt = Arc::new(ListenerRt::new(idx, cfg)?);
                let socket = TcpListener::bind(&cfg.address)
                    .map_err(|e| format!("listener {idx} cannot bind {}: {e}", cfg.address))?;
                let addr = socket.local_addr().map_err(|e| e.to_string())?;
                spawn_acceptor(
                    socket,
                    idx,
                    rt.clone(),
                    tx.clone(),
                    waiting.clone(),
                    tokens.clone(),
                );
                listeners.push(rt);
                addrs.push(addr);
            }
            Ok(Bound {
                listeners,
                ingress: rx,
                _keepalive: tx,
                waiting,
                addrs,
            })
        }
    }

    impl Acceptor for Bound {
        fn listeners(&self) -> &[Arc<ListenerRt>] {
            &self.listeners
        }

        fn addrs(&self) -> &[SocketAddr] {
            &self.addrs
        }

        fn try_next(&mut self) -> Option<Ingress> {
            self.ingress.try_recv().ok()
        }

        /// The drive loop's idle sleep and its wakeup are the same call.
        fn next_within(&mut self, timeout: Duration) -> Option<Ingress> {
            self.ingress.recv_timeout(timeout).ok()
        }

        fn answer(&mut self, token: u32, outcome: Outcome) {
            let waiting = {
                let mut map = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
                map.remove(&token)
            };
            if let Some(w) = waiting {
                let _ = w.tx.try_send(outcome);
            }
        }

        fn owner_of(&self, token: u32) -> Option<usize> {
            let map = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
            map.get(&token).map(|w| w.listener)
        }
    }

    fn spawn_acceptor(
        socket: TcpListener,
        idx: usize,
        rt: Arc<ListenerRt>,
        tx: Sender<Ingress>,
        waiting: Arc<Mutex<HashMap<u32, Waiting>>>,
        tokens: Arc<AtomicU32>,
    ) {
        let conns = Arc::new(AtomicUsize::new(0));
        std::thread::spawn(move || {
            for conn in socket.incoming() {
                let Ok(stream) = conn else { continue };
                if conns.fetch_add(1, Ordering::SeqCst) >= rt.max_conns {
                    conns.fetch_sub(1, Ordering::SeqCst);
                    respond(&stream, &refusal_bytes(503, CAP_TEXT));
                    continue;
                }
                let rt = rt.clone();
                let tx = tx.clone();
                let waiting = waiting.clone();
                let tokens = tokens.clone();
                let conns = conns.clone();
                std::thread::spawn(move || {
                    serve_conn(&stream, idx, &rt, &tx, &waiting, &tokens);
                    conns.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
    }

    /// One connection, one request, one response, close.
    fn serve_conn(
        stream: &TcpStream,
        idx: usize,
        rt: &ListenerRt,
        tx: &Sender<Ingress>,
        waiting: &Mutex<HashMap<u32, Waiting>>,
        tokens: &AtomicU32,
    ) {
        let _ = stream.set_read_timeout(Some(rt.deadline));
        let (token, message) = match read_request(stream, rt, tokens) {
            Ok(pair) => pair,
            Err((status, text)) => {
                respond(stream, &refusal_bytes(status, text));
                return;
            }
        };

        // Register before sending: the reply must find someone waiting
        // even if it arrives before this thread reaches recv.
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        {
            let mut map = waiting.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(
                token,
                Waiting {
                    tx: reply_tx,
                    listener: idx,
                },
            );
        }
        if tx
            .send(Ingress {
                listener: idx,
                token,
                message,
            })
            .is_err()
        {
            // The drive loop is gone; the deployment is coming down.
            waiting
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&token);
            respond(stream, &refusal_bytes(503, DOWN_TEXT));
            return;
        }
        match reply_rx.recv_timeout(rt.deadline) {
            Ok(outcome) => respond(stream, &outcome_bytes(&outcome)),
            Err(_) => {
                // Deregister so a late reply is consumed without a reader
                // — the C's exact behavior at its deadline.
                waiting
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&token);
                respond(stream, &refusal_bytes(504, LATE_TEXT));
            }
        }
    }

    /// Read until the request is whole, under the read timeout.
    fn read_request(
        mut stream: &TcpStream,
        rt: &ListenerRt,
        tokens: &AtomicU32,
    ) -> Result<(u32, Vec<u8>), (u16, &'static str)> {
        let mut buf = Vec::with_capacity(2048);
        let mut chunk = [0u8; 2048];
        let mut mint = || tokens.fetch_add(1, Ordering::Relaxed);
        loop {
            let headers_done = match parse_request(&buf, rt, &mut mint)? {
                Parsed::Complete { token, message } => return Ok((token, message)),
                Parsed::Incomplete { headers_done } => headers_done,
            };
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return Err(cut_short(headers_done)),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    fn respond(mut stream: &TcpStream, bytes: &[u8]) {
        let _ = stream.write_all(bytes);
        let _ = stream.flush();
    }
}

const CAP_TEXT: &str = "the listener is at its connection cap\n";
const DOWN_TEXT: &str = "the deployment is shutting down\n";
const LATE_TEXT: &str = "the program did not answer within the deadline\n";

// ---------------------------------------------------------------------------
// depth: the polled acceptor — wasi, and natively under test
// ---------------------------------------------------------------------------

/// No threads: non-blocking sockets, each connection a small state
/// machine, all of them stepped by [`polled::Bound::poll`] from the drive
/// loop's own thread.
pub mod polled {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Instant;

    pub struct Bound {
        listeners: Vec<Arc<ListenerRt>>,
        sockets: Vec<TcpListener>,
        addrs: Vec<SocketAddr>,
        conns: Vec<Conn>,
        ready: VecDeque<Ingress>,
        tokens: u32,
    }

    struct Conn {
        stream: TcpStream,
        listener: usize,
        /// When the current stage began; each stage gets the deadline.
        since: Instant,
        /// Whether it holds one of its listener's `max_conns` slots — a
        /// connection refused at the cap is written to and not counted.
        counted: bool,
        state: State,
    }

    enum State {
        Reading { buf: Vec<u8>, headers_done: bool },
        Waiting { token: u32 },
        Writing { out: Vec<u8>, off: usize },
    }

    impl Bound {
        pub fn bind(configs: &[Listener]) -> Result<Bound, String> {
            let mut listeners = Vec::new();
            let mut sockets = Vec::new();
            let mut addrs = Vec::new();
            for (idx, cfg) in configs.iter().enumerate() {
                let rt = ListenerRt::new(idx, cfg)?;
                let socket = TcpListener::bind(&cfg.address)
                    .map_err(|e| format!("listener {idx} cannot bind {}: {e}", cfg.address))?;
                socket
                    .set_nonblocking(true)
                    .map_err(|e| format!("listener {idx} cannot be non-blocking: {e}"))?;
                addrs.push(socket.local_addr().map_err(|e| e.to_string())?);
                listeners.push(Arc::new(rt));
                sockets.push(socket);
            }
            Ok(Bound {
                listeners,
                sockets,
                addrs,
                conns: Vec::new(),
                ready: VecDeque::new(),
                tokens: 1,
            })
        }

        /// One pass over every socket: accept what is waiting, read what
        /// has arrived, write what is owed, and time out what is late.
        /// Never blocks.
        pub fn poll(&mut self) {
            let Bound {
                listeners,
                sockets,
                conns,
                ready,
                tokens,
                ..
            } = self;
            for (idx, socket) in sockets.iter().enumerate() {
                loop {
                    let stream = match socket.accept() {
                        Ok((stream, _)) => stream,
                        Err(_) => break,
                    };
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let held = conns
                        .iter()
                        .filter(|c| c.counted && c.listener == idx)
                        .count();
                    let (counted, state) = if held >= listeners[idx].max_conns {
                        (false, writing(refusal_bytes(503, CAP_TEXT)))
                    } else {
                        (
                            true,
                            State::Reading {
                                buf: Vec::with_capacity(2048),
                                headers_done: false,
                            },
                        )
                    };
                    conns.push(Conn {
                        stream,
                        listener: idx,
                        since: Instant::now(),
                        counted,
                        state,
                    });
                }
            }
            let now = Instant::now();
            let mut chunk = [0u8; 2048];
            let mut mint = || {
                let t = *tokens;
                *tokens += 1;
                t
            };
            conns.retain_mut(|conn| {
                let rt = &listeners[conn.listener];
                let late = now.duration_since(conn.since) > rt.deadline;
                match &mut conn.state {
                    State::Reading { buf, headers_done } => {
                        if late {
                            let (status, text) = cut_short(*headers_done);
                            conn.turn(writing(refusal_bytes(status, text)));
                            return true;
                        }
                        loop {
                            match conn.stream.read(&mut chunk) {
                                Ok(0) => {
                                    let (status, text) = cut_short(*headers_done);
                                    conn.turn(writing(refusal_bytes(status, text)));
                                    return true;
                                }
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                                Err(_) => return false,
                            }
                            match parse_request(buf, rt, &mut mint) {
                                Ok(Parsed::Complete { token, message }) => {
                                    ready.push_back(Ingress {
                                        listener: conn.listener,
                                        token,
                                        message,
                                    });
                                    conn.turn(State::Waiting { token });
                                    return true;
                                }
                                Ok(Parsed::Incomplete { headers_done: done }) => {
                                    *headers_done = done;
                                }
                                Err((status, text)) => {
                                    conn.turn(writing(refusal_bytes(status, text)));
                                    return true;
                                }
                            }
                        }
                        true
                    }
                    State::Waiting { .. } => {
                        if late {
                            // The token leaves with the state, so a late
                            // reply is consumed without a reader — the
                            // C's exact behavior at its deadline.
                            conn.turn(writing(refusal_bytes(504, LATE_TEXT)));
                        }
                        true
                    }
                    State::Writing { out, off } => {
                        if late {
                            return false;
                        }
                        while *off < out.len() {
                            match conn.stream.write(&out[*off..]) {
                                Ok(0) => return false,
                                Ok(n) => *off += n,
                                Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
                                Err(_) => return false,
                            }
                        }
                        let _ = conn.stream.flush();
                        false
                    }
                }
            });
        }
    }

    impl Conn {
        fn turn(&mut self, state: State) {
            self.state = state;
            self.since = Instant::now();
        }
    }

    fn writing(out: Vec<u8>) -> State {
        State::Writing { out, off: 0 }
    }

    impl Acceptor for Bound {
        fn listeners(&self) -> &[Arc<ListenerRt>] {
            &self.listeners
        }

        fn addrs(&self) -> &[SocketAddr] {
            &self.addrs
        }

        fn try_next(&mut self) -> Option<Ingress> {
            self.poll();
            self.ready.pop_front()
        }

        /// Look every [`POLL_TICK`] until a request is whole or the
        /// timeout is spent; with no listener at all this is a plain
        /// sleep, so an empty listener set idles like a full one.
        fn next_within(&mut self, timeout: Duration) -> Option<Ingress> {
            let deadline = Instant::now() + timeout;
            loop {
                self.poll();
                if let Some(ingress) = self.ready.pop_front() {
                    return Some(ingress);
                }
                let now = Instant::now();
                if now >= deadline {
                    return None;
                }
                std::thread::sleep((deadline - now).min(POLL_TICK));
            }
        }

        fn answer(&mut self, token: u32, outcome: Outcome) {
            let waiting = self
                .conns
                .iter_mut()
                .find(|c| matches!(c.state, State::Waiting { token: t } if t == token));
            if let Some(conn) = waiting {
                conn.turn(writing(outcome_bytes(&outcome)));
            }
        }

        fn owner_of(&self, token: u32) -> Option<usize> {
            self.conns
                .iter()
                .find(|c| matches!(c.state, State::Waiting { token: t } if t == token))
                .map(|c| c.listener)
        }
    }
}

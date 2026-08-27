//! The http listener: `dhost_http.c`'s contract, thread-per-connection.
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

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drt_config::Listener;

/// The C's bounds, kept: header block room, one header value, headers per
/// request.
const HDR_ROOM: usize = 8192;
const HDR_VALUE_MAX: usize = 4096;
const MAX_HEADERS: usize = 32;

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

struct Waiting {
    tx: SyncSender<Outcome>,
    listener: usize,
}

/// The bound listeners: sockets accepted on their own threads, requests
/// arriving on one channel, waiting connections reachable by token.
pub struct Bound {
    pub listeners: Vec<Arc<ListenerRt>>,
    ingress: Receiver<Ingress>,
    waiting: Arc<Mutex<HashMap<u32, Waiting>>>,
    addrs: Vec<SocketAddr>,
}

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
    conns: AtomicUsize,
}

impl Bound {
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// A request already parsed and waiting, if any — non-blocking.
    pub fn try_next(&self) -> Option<Ingress> {
        self.ingress.try_recv().ok()
    }

    /// Wait for the next request, up to `timeout` — the drive loop's idle
    /// sleep and its wakeup are the same call, so a request never waits on
    /// a tick.
    pub fn next_within(&self, timeout: Duration) -> Option<Ingress> {
        self.ingress.recv_timeout(timeout).ok()
    }

    /// Answer the connection a token names. A token nobody is waiting on is
    /// not an error — the connection may have hit its deadline first.
    pub fn answer(&self, token: u32, outcome: Outcome) {
        let waiting = {
            let mut map = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
            map.remove(&token)
        };
        if let Some(w) = waiting {
            let _ = w.tx.try_send(outcome);
        }
    }

    /// Which listener a waiting token belongs to, for the reply filter.
    pub fn owner_of(&self, token: u32) -> Option<usize> {
        let map = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&token).map(|w| w.listener)
    }
}

/// Bind every configured listener and start accepting. Fails whole: a
/// deployment with three listeners and two ports is not almost-serving.
pub fn bind(configs: &[Listener]) -> Result<Bound, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let waiting = Arc::new(Mutex::new(HashMap::new()));
    let tokens = Arc::new(AtomicU32::new(1));
    let mut listeners = Vec::new();
    let mut addrs = Vec::new();
    for (idx, cfg) in configs.iter().enumerate() {
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
        let socket = TcpListener::bind(&cfg.address)
            .map_err(|e| format!("listener {idx} cannot bind {}: {e}", cfg.address))?;
        let addr = socket.local_addr().map_err(|e| e.to_string())?;
        let rt = Arc::new(ListenerRt {
            queue: cfg.queue.clone(),
            reply_queue: cfg.reply_queue.clone(),
            hdr_allow: cfg.headers.iter().map(|h| h.to_lowercase()).collect(),
            resp_allow: cfg.resp_headers.iter().map(|h| h.to_lowercase()).collect(),
            max_body: cfg.max_body,
            deadline: Duration::from_millis(cfg.conn_deadline_ms),
            max_conns: cfg.max_conns,
            conns: AtomicUsize::new(0),
        });
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
        waiting,
        addrs,
    })
}

fn spawn_acceptor(
    socket: TcpListener,
    idx: usize,
    rt: Arc<ListenerRt>,
    tx: Sender<Ingress>,
    waiting: Arc<Mutex<HashMap<u32, Waiting>>>,
    tokens: Arc<AtomicU32>,
) {
    std::thread::spawn(move || {
        for conn in socket.incoming() {
            let Ok(stream) = conn else { continue };
            if rt.conns.fetch_add(1, Ordering::SeqCst) >= rt.max_conns {
                rt.conns.fetch_sub(1, Ordering::SeqCst);
                respond_text(&stream, 503, "the listener is at its connection cap\n");
                continue;
            }
            let rt = rt.clone();
            let tx = tx.clone();
            let waiting = waiting.clone();
            let tokens = tokens.clone();
            std::thread::spawn(move || {
                serve_conn(&stream, idx, &rt, &tx, &waiting, &tokens);
                rt.conns.fetch_sub(1, Ordering::SeqCst);
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
            respond_text(stream, status, text);
            return;
        }
    };

    // Register before sending: the reply must find someone waiting even if
    // it arrives before this thread reaches recv.
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
        respond_text(stream, 503, "the deployment is shutting down\n");
        return;
    }
    match reply_rx.recv_timeout(rt.deadline) {
        Ok(Outcome::Reply {
            status,
            content_type,
            headers,
            body,
        }) => respond(stream, status, &content_type, &headers, &body),
        Ok(Outcome::Refused { status, text }) => respond_text(stream, status, text),
        Err(_) => {
            // Deregister so a late reply is consumed without a reader —
            // the C's exact behavior at its deadline.
            waiting
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&token);
            respond_text(
                stream,
                504,
                "the program did not answer within the deadline\n",
            );
        }
    }
}

/// Read and validate one request; build the request map. The refusals and
/// their texts are `dhost_http.c`'s.
fn read_request(
    stream: &TcpStream,
    rt: &ListenerRt,
    tokens: &AtomicU32,
) -> Result<(u32, Vec<u8>), (u16, &'static str)> {
    let mut stream = stream;
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        // Parse from scratch each pass — httparse borrows from the buffer,
        // so headers cannot outlive a read that may reallocate it.
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut headers);
        let parsed = match req.parse(&buf) {
            Ok(p) => p,
            Err(_) => return Err((400, "unparseable request\n")),
        };
        if let httparse::Status::Complete(header_len) = parsed {
            let method = req.method.unwrap_or("").to_string();
            let path = req.path.unwrap_or("").to_string();

            // Content-Length, strictly: digits only, at most one. A parser
            // that disagrees with the LB about where the body ends is the
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

            // The allowlisted headers that arrived, in allowlist order,
            // under the allowlist's spelling. First match wins. Copied out
            // now, because the parse borrows the buffer the body read is
            // about to grow.
            let mut present: Vec<(String, rmpv::Value)> = Vec::new();
            for name in &rt.hdr_allow {
                let found = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case(name));
                if let Some(h) = found {
                    if h.value.len() > HDR_VALUE_MAX {
                        return Err((
                            431,
                            "an allowlisted header is past this host's value bound\n",
                        ));
                    }
                    if !h.value.is_empty() {
                        present.push((name.clone(), bytes_value(h.value)));
                    }
                }
            }
            let _ = req;

            // The body may still be arriving.
            while buf.len() < header_len + clen {
                let n = stream
                    .read(&mut chunk)
                    .map_err(|_| (400u16, "the connection went away mid-body\n"))?;
                if n == 0 {
                    return Err((400, "the connection went away mid-body\n"));
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = &buf[header_len..header_len + clen];

            let token = tokens.fetch_add(1, Ordering::Relaxed);
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
            let mut out = Vec::new();
            rmpv::encode::write_value(&mut out, &rmpv::Value::Map(map))
                .map_err(|_| (500u16, "encoding failed\n"))?;
            return Ok((token, out));
        }
        // Incomplete: keep reading, within the header room.
        if buf.len() > HDR_ROOM {
            return Err((400, "headers past this host's room\n"));
        }
        let n = stream
            .read(&mut chunk)
            .map_err(|_| (400u16, "unparseable request\n"))?;
        if n == 0 {
            return Err((400, "unparseable request\n"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
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
// The reply side: one guest map -> one Outcome, under the owner's allowlist
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

/// Parse one reply from the queue into what the connection gets. The
/// permissive reads are the C's: a missing or ill-typed field takes its
/// default, because the reply already left the program and refusing it
/// answers nobody.
pub fn reply_token(raw: &[u8]) -> u32 {
    let value = rmpv::decode::read_value(&mut &raw[..]).unwrap_or(rmpv::Value::Nil);
    field(&value, "conn").and_then(|v| v.as_u64()).unwrap_or(0) as u32
}

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
// Response writing: the C's head, byte for byte in shape
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

fn respond(
    mut stream: &TcpStream,
    status: u16,
    content_type: &str,
    headers: &[(String, String)],
    body: &[u8],
) {
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
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn respond_text(stream: &TcpStream, status: u16, text: &'static str) {
    respond(stream, status, "text/plain", &[], text.as_bytes());
}

impl Outcome {
    pub fn refused(status: u16, text: &'static str) -> Outcome {
        Outcome::Refused { status, text }
    }
}

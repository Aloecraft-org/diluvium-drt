//! The WebSocket client connector: an outbound `ws://` or `wss://`
//! connection a node holds by handle, for a program that has to hold a
//! conversation with a service rather than make requests of it -- a
//! signaling socket, first (`doc/BrowserAccess.md` §7).
//!
//! ```text
//!   ws/connect {url, headers?, timeout_ms?, idle_ms?, vital?, wake?}
//!                                 -> {handle, status}
//!   ws/send    {handle, text | data}  -> nil
//!   ws/recv    {handle, wait?}    -> {messages = {...}, closed = {code, reason} | nil}
//!   ws/close   {handle, code?, reason?} -> nil
//! ```
//!
//! ## surface block
//!
//! - Entry points: the four calls above, dispatched in [`Connector::call_as`]
//!   below; [`WsScopeType`] for the wiring.
//! - Configurable: [`MESSAGE_MAX`], [`INBOX_MAX`], [`OUTBOX_MAX`],
//!   [`CONNECT_MS`], [`CONNECT_MS_MAX`], [`CLOSE_MS`], [`HEADERS_MAX`].
//! - Fan-out: [`State`], the four states a connection is in; the
//!   connection task's `select!` in [`pump`], which is every way a
//!   connection moves between them.
//!
//! **`wss://`, and plain `ws://` only to loopback.** The upgrade carries
//! whatever authenticates the connection -- an advertise token in
//! `Authorization`, or a header the scope injects -- so it never crosses a
//! network unencrypted. A plain origin off this machine is refused in the
//! scope at boot, a plain URL whose host is not `localhost` or a loopback
//! literal is refused at connect, and a plain connection dials only
//! loopback addresses whatever its name resolved to. Loopback is for test
//! stubs and a local proxy.
//!
//! **The scope is rest's.** An origin allowlist in exactly `rest`'s shape,
//! parsed by `rest`'s own code, with `wss://` and `ws://` read as `https://`
//! and `http://`: the same `headers` injected where the program cannot see
//! them (an advertise token belongs there when a deployment can put it
//! there), the same `allow_headers`, the same `allow_private` and the same
//! check of the **resolved** address before anything connects, and the same
//! `extra_roots` beside the public ones. An outbound socket is as much an
//! SSRF primitive as an outbound request, and one grant shape for both is
//! one thing an operator has to read.
//!
//! **Node-owned, like `socket`'s handles** (`doc/Plan-0.7.0.md` §2–3): the
//! connection lives while its owner does, closes (1001, going away) when
//! its owner is released, and survives its owner's hibernation. `vital`
//! ties the owner's life to it -- the far end closing, the connection
//! failing, or the owner's own `close` ends the owner (§3.3). `wake` pushes
//! `{handle, ready = "recv"}` to the owner's queue when messages or the
//! close arrive, once, re-armed by the next `recv` (§3.4).
//!
//! **What waits, waits in the host.** `connect` answers once the handshake
//! is done (or failed); `recv` once there is a message or the close;
//! `send` once the message is queued to the connection. Each is one call
//! pending in the hostcall pump, polled on the drive loop's cadence, as
//! `socket`'s are. `recv {wait = false}` answers at once, possibly empty.
//!
//! **The connection runs on the connector's own runtime**, one task per
//! connection, so it answers pings and keeps reading while its owner is
//! parked, and does so the same under `drt start` and `drt run`. Pongs are
//! tungstenite's, automatic. `idle_ms`, when given, closes a connection
//! that has carried no frame at all -- not even a ping -- for that long:
//! the dead-peer check a server that pings on a schedule makes possible.
//!
//! **No reconnect.** A connection that ends is ended; `recv` says how, with
//! the close code and reason, and the program decides whether and when to
//! dial again. When to give up is the protocol's business (a server that
//! says "replaced" means stop), not this connector's.
//!
//! Replay: a reply is a message like any other, logged and replayed. A
//! replay does not re-dial or re-send.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use drt_caps::{Scope, ScopeType};
use drt_connector::{Asker, CallError, CallResult, Caller, Connector, HandleId, Handles, Notice};
use drt_connector_rest::{AllowEntry, RestScope, Url};

/// The largest message either way: 1 MiB, `socket`'s write cap. A larger
/// inbound message fails the connection (tungstenite's own limit); a larger
/// outbound one is refused by name.
pub const MESSAGE_MAX: usize = 1024 * 1024;
/// Messages held for an owner that has not called `recv`. Past it the
/// connection stops reading until the owner drains, so a flood backs up
/// into the far end's TCP window rather than into this process.
pub const INBOX_MAX: usize = 256;
/// Messages queued to go out before `send` waits.
pub const OUTBOX_MAX: usize = 64;
/// How long `connect` may take by default, rest's default; and the most a
/// call may ask for, rest's maximum.
pub const CONNECT_MS: u64 = 15_000;
pub const CONNECT_MS_MAX: u64 = 120_000;
/// How long a close waits for the far end's close frame before dropping
/// the connection anyway.
pub const CLOSE_MS: u64 = 5_000;
/// Request headers a call may set, rest's own bound.
pub const HEADERS_MAX: usize = 64;

/// The connector. One table of connections, each keyed by who holds it.
pub struct WsConnector {
    conns: Handles<Conn>,
    /// Owners whose vital connection they closed themselves, reported on
    /// the next [`Connector::ended`] with the ones that ended on their own.
    ended: Mutex<Vec<(Caller, String)>>,
}

impl Default for WsConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl WsConnector {
    pub fn new() -> Self {
        WsConnector {
            conns: Handles::new("ws"),
            ended: Mutex::new(Vec::new()),
        }
    }
}

/// Where a connection is. The task moves it forward; nothing moves it back.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Dialing, TLS, or the handshake.
    Connecting,
    /// Open, with the handshake's HTTP status (101).
    Open(u16),
    /// Never opened: why, in a sentence.
    Failed(String),
    /// Opened and then ended: the close code when the far end or the owner
    /// sent one (1006 when none arrived, as a browser reports it), and why.
    Closed(u16, String),
}

pub struct WsScopeType;

impl ScopeType for WsScopeType {
    fn describe(&self) -> &str {
        "allowed origins, in rest's shape: \"wss://api.example.com\", a list of them, or {allow: [...], allow_private?: bool, extra_roots?: [...]}"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        let parsed = parse_scope(scope)?;
        // As rest: an allowlist that grants nothing is a mistake to say at
        // boot, not a refusal to discover on the first call.
        if parsed.is_empty() {
            return Err(
                "the ws scope grants no origins, so every connect would be refused; name at least one, e.g. \"wss://api.example.com\"".into(),
            );
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Connector for WsConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(WsScopeType)
    }

    /// The caller-blind path is the root with no grants: what it opens is
    /// the root's and lives to [`Connector::finish`].
    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let asker = Asker {
            caller: Caller::Root,
            grants: &[],
        };
        self.call_as(&asker, call, args, scope).await
    }

    async fn call_as(
        &self,
        asker: &Asker<'_>,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let caller = asker.caller;
        match call {
            "ws/connect" => {
                let scope = parse_scope(scope).map_err(CallError::new)?;
                self.connect(caller, &scope, args).await
            }
            "ws/send" => self.send(caller, args).await,
            "ws/recv" => self.recv(caller, args).await,
            "ws/close" => self.close(caller, args),
            other => Err(CallError::new(format!("ws: unknown call '{other}'"))),
        }
    }

    /// The owner is gone for good: each connection it held closes (1001),
    /// and each one that was open is one line of the report (§2.5).
    fn release(&self, caller: &Caller) -> Vec<String> {
        self.conns
            .release(*caller)
            .into_iter()
            .filter_map(|(_, conn)| conn.lost())
            .collect()
    }

    /// Owners whose vital connection ended since the last ask. Each is
    /// reported once and the entry goes with the report.
    fn ended(&self) -> Vec<(Caller, String)> {
        let mut ended = std::mem::take(&mut *lock(&self.ended));
        for (owner, _, conn) in self.conns.take_where(|_, _, c| c.vital && c.has_ended()) {
            ended.push((
                owner,
                format!("its vital WebSocket to {} {}", conn.origin, conn.how()),
            ));
        }
        ended
    }

    fn notices(&self) -> Vec<Notice> {
        let mut out = Vec::new();
        self.conns.each(|owner, handle, conn| {
            if let Some(queue) = conn.became_ready() {
                out.push(Notice {
                    owner,
                    queue,
                    message: reply(vec![("handle", handle.0.into()), ("ready", "recv".into())]),
                });
            }
        });
        out
    }

    fn finish(&self) -> Vec<String> {
        self.conns
            .drain_root()
            .into_iter()
            .filter_map(|(_, conn)| conn.lost())
            .collect()
    }
}

// depth: what the host holds behind a handle

/// What the connection task and the calls share.
struct Shared {
    state: State,
    inbox: VecDeque<rmpv::Value>,
    /// Told when the owner drains a full inbox, so the task reads again.
    drained: Arc<Notify>,
}

/// One connection, as its owner's handle sees it. Dropping it drops the
/// sender, which is the task's cue to close with 1001.
struct Conn {
    shared: Arc<Mutex<Shared>>,
    out: mpsc::Sender<Out>,
    /// `scheme://host:port`, for sentences. Never the path or query, which
    /// may carry a credential.
    origin: String,
    vital: bool,
    wake: Option<String>,
    notified: bool,
    /// Whether `recv` has handed the owner the close; after that there is
    /// nothing more to be woken for.
    close_seen: bool,
}

/// What the owner asks the task to put on the wire.
enum Out {
    Message(Message),
    Close(u16, String),
}

impl Conn {
    fn state(&self) -> State {
        lock(&self.shared).state.clone()
    }

    fn has_ended(&self) -> bool {
        matches!(self.state(), State::Closed(..) | State::Failed(_))
    }

    /// How it ended, for a sentence that starts with the connection.
    fn how(&self) -> String {
        match self.state() {
            State::Closed(code, reason) if reason.is_empty() => format!("closed ({code})"),
            State::Closed(code, reason) => format!("closed ({code}: {reason})"),
            State::Failed(why) => format!("failed: {why}"),
            _ => "ended".into(),
        }
    }

    fn became_ready(&mut self) -> Option<String> {
        let queue = self.wake.clone()?;
        if self.notified || self.close_seen {
            return None;
        }
        let ready = {
            let s = lock(&self.shared);
            !s.inbox.is_empty() || matches!(s.state, State::Closed(..) | State::Failed(_))
        };
        if !ready {
            return None;
        }
        self.notified = true;
        Some(queue)
    }

    /// What closing this loses: an open connection, cut.
    fn lost(self) -> Option<String> {
        matches!(self.state(), State::Open(_))
            .then(|| format!("a WebSocket to {}, cut", self.origin))
    }
}

// depth: the scope

/// rest's scope, with the WebSocket schemes read as the HTTP ones they
/// upgrade from. Rewritten only where rest reads an origin: a bare string,
/// a list's entries, a table's `origin`, and `allow`. A plain-text origin
/// that is not loopback is refused here, at boot, by name.
fn parse_scope(scope: Option<&Scope>) -> Result<RestScope, String> {
    let Some(Scope(value)) = scope else {
        return RestScope::parse(None);
    };
    let value = http_schemes(value.clone());
    for origin in origins(&value) {
        if let Ok(url) = Url::parse(&origin) {
            if !url.tls && !loopback_name(&url.host) {
                return Err(format!(
                    "the ws scope grants {}, which is plain text off this machine; grant wss:// (ws:// is for loopback only)",
                    ws_origin(&url)
                ));
            }
        }
    }
    RestScope::parse(Some(&Scope(value))).map_err(|e| e.replace("the rest scope", "the ws scope"))
}

/// The origin strings in a scope, from the places `http_schemes` rewrites.
fn origins(value: &rmpv::Value) -> Vec<String> {
    match value {
        rmpv::Value::String(s) => s.as_str().map(String::from).into_iter().collect(),
        rmpv::Value::Array(items) => items.iter().flat_map(origins).collect(),
        rmpv::Value::Map(entries) => entries
            .iter()
            .filter(|(k, _)| matches!(k.as_str(), Some("allow") | Some("origin")))
            .flat_map(|(_, v)| origins(v))
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a URL's host names this machine: `localhost`, or a loopback
/// literal. The only hosts plain `ws://` may reach, because what goes on
/// the upgrade (an `Authorization` header, an injected token) must never
/// cross a network in the clear. The resolved address is checked again in
/// [`open`].
fn loopback_name(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn http_schemes(value: rmpv::Value) -> rmpv::Value {
    match value {
        rmpv::Value::String(s) => match s.as_str() {
            Some(text) => rmpv::Value::from(to_http(text)),
            None => rmpv::Value::String(s),
        },
        rmpv::Value::Array(items) => {
            rmpv::Value::Array(items.into_iter().map(http_schemes).collect())
        }
        rmpv::Value::Map(entries) => rmpv::Value::Map(
            entries
                .into_iter()
                .map(|(k, v)| match k.as_str() {
                    Some("allow") | Some("origin") => (k, http_schemes(v)),
                    _ => (k, v),
                })
                .collect(),
        ),
        other => other,
    }
}

fn to_http(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        url.to_string()
    }
}

// depth: the verbs

impl WsConnector {
    async fn connect(
        &self,
        caller: Caller,
        scope: &RestScope,
        args: Option<rmpv::Value>,
    ) -> CallResult {
        let args = args.ok_or_else(|| CallError::new("args.url must be a ws:// or wss:// URL"))?;
        let raw = string_field(&args, "url")?
            .ok_or_else(|| CallError::new("args.url must be a ws:// or wss:// URL"))?
            .to_string();
        if !raw.starts_with("wss://") && !raw.starts_with("ws://") {
            return Err(CallError::new("the url must begin ws:// or wss://"));
        }
        let url = Url::parse(&to_http(&raw)).map_err(CallError::new)?;
        if !url.tls && !loopback_name(&url.host) {
            return Err(CallError::new(format!(
                "{} is plain text off this machine; use wss:// (ws:// is for loopback only)",
                ws_origin(&url)
            )));
        }
        let entry = scope.matching(&url).cloned().ok_or_else(|| {
            CallError::new(format!(
                "{} is not an origin this instance was granted",
                ws_origin(&url)
            ))
        })?;
        let headers = header_list(&args, &entry)?;
        let timeout = ms_field(&args, "timeout_ms")?.unwrap_or(CONNECT_MS);
        if timeout == 0 || timeout > CONNECT_MS_MAX {
            return Err(CallError::new(format!(
                "args.timeout_ms must be 1..={CONNECT_MS_MAX}"
            )));
        }
        let idle = match ms_field(&args, "idle_ms")? {
            Some(0) => return Err(CallError::new("args.idle_ms must be positive")),
            other => other.map(Duration::from_millis),
        };

        let shared = Arc::new(Mutex::new(Shared {
            state: State::Connecting,
            inbox: VecDeque::new(),
            drained: Arc::new(Notify::new()),
        }));
        let (out, out_rx) = mpsc::channel(OUTBOX_MAX);
        let dial = Dial {
            raw,
            url: url.clone(),
            headers,
            scope: scope.clone(),
        };
        own_runtime().spawn(run(
            dial,
            Duration::from_millis(timeout),
            idle,
            shared.clone(),
            out_rx,
        ));
        let handle = self.conns.insert(
            caller,
            Conn {
                shared,
                out,
                origin: ws_origin(&url),
                vital: vital_arg(&args),
                wake: wake_arg(&args),
                notified: false,
                close_seen: false,
            },
        );

        let opened = self
            .until_ready(caller, handle, |conn| match conn.state() {
                State::Connecting => Ok(None),
                State::Open(status) => Ok(Some(status)),
                State::Failed(why) => Err(CallError::new(why)),
                // Opened and closed before the pump looked: it did open.
                State::Closed(..) => Ok(Some(101)),
            })
            .await;
        match opened {
            Ok(status) => Ok(reply(vec![
                ("handle", handle.0.into()),
                ("status", u64::from(status).into()),
            ])),
            Err(e) => {
                // A connect that failed leaves nothing behind to close.
                let _ = self.conns.remove(caller, handle);
                Err(e)
            }
        }
    }

    async fn send(&self, caller: Caller, args: Option<rmpv::Value>) -> CallResult {
        let handle = handle_arg(args.as_ref())?;
        let args = args.expect("handle_arg read it");
        let message = match (field(&args, "text"), field(&args, "data")) {
            (Some(rmpv::Value::String(s)), None) => {
                let text = s
                    .as_str()
                    .ok_or_else(|| CallError::new("args.text must be UTF-8; send bytes as data"))?;
                Message::Text(text.to_string())
            }
            (None, Some(v)) => Message::Binary(
                string_bytes(v)
                    .ok_or_else(|| CallError::new("args.data must be a string"))?
                    .to_vec(),
            ),
            _ => {
                return Err(CallError::new(
                    "args must carry exactly one of text or data",
                ))
            }
        };
        if message.len() > MESSAGE_MAX {
            return Err(CallError::new(format!(
                "the message is bigger than the host's cap ({MESSAGE_MAX} bytes)"
            )));
        }
        let mut message = Some(message);
        self.until_ready(caller, handle, |conn| {
            match conn.state() {
                State::Open(_) => {}
                State::Connecting => return Ok(None),
                _ => {
                    return Err(CallError::new(format!(
                        "send: the connection {}",
                        conn.how()
                    )))
                }
            }
            let Some(m) = message.take() else {
                return Ok(Some(()));
            };
            match conn.out.try_send(Out::Message(m)) {
                Ok(()) => Ok(Some(())),
                Err(mpsc::error::TrySendError::Full(Out::Message(m))) => {
                    message = Some(m);
                    Ok(None)
                }
                Err(_) => Err(CallError::new(format!(
                    "send: the connection {}",
                    conn.how()
                ))),
            }
        })
        .await?;
        Ok(rmpv::Value::Nil)
    }

    async fn recv(&self, caller: Caller, args: Option<rmpv::Value>) -> CallResult {
        let handle = handle_arg(args.as_ref())?;
        let wait = args
            .as_ref()
            .and_then(|a| field(a, "wait"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        self.until_ready(caller, handle, |conn| {
            // Using the handle re-arms its readiness.
            conn.notified = false;
            let mut s = lock(&conn.shared);
            let was_full = s.inbox.len() >= INBOX_MAX;
            let messages: Vec<rmpv::Value> = s.inbox.drain(..).collect();
            if was_full {
                s.drained.notify_one();
            }
            let closed = match &s.state {
                State::Closed(code, reason) => Some(reply(vec![
                    ("code", u64::from(*code).into()),
                    ("reason", reason.as_str().into()),
                ])),
                State::Failed(why) => Some(reply(vec![
                    ("code", 1006u64.into()),
                    ("reason", why.as_str().into()),
                ])),
                _ => None,
            };
            drop(s);
            if messages.is_empty() && closed.is_none() && wait {
                return Ok(None);
            }
            if closed.is_some() {
                conn.close_seen = true;
            }
            Ok(Some(reply(vec![
                ("messages", rmpv::Value::Array(messages)),
                ("closed", closed.unwrap_or(rmpv::Value::Nil)),
            ])))
        })
        .await
    }

    fn close(&self, caller: Caller, args: Option<rmpv::Value>) -> CallResult {
        let handle = handle_arg(args.as_ref())?;
        let code = match args.as_ref().and_then(|a| field(a, "code")) {
            None | Some(rmpv::Value::Nil) => 1000,
            Some(v) => v
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| *n == 1000 || (3000..=4999).contains(n))
                .ok_or_else(|| {
                    CallError::new("args.code must be 1000 or an application code, 3000..=4999")
                })?,
        };
        let reason = string_field(args.as_ref().expect("handle_arg read it"), "reason")?
            .unwrap_or("")
            .to_string();
        // RFC 6455 §5.5: a control frame's payload is at most 125 bytes, two
        // of them the code.
        if reason.len() > 123 {
            return Err(CallError::new("args.reason must be at most 123 bytes"));
        }
        let conn = self
            .conns
            .remove(caller, handle)
            .map_err(|e| CallError::new(e.to_string()))?;
        if conn.vital {
            lock(&self.ended).push((
                caller,
                format!(
                    "its vital WebSocket to {} was closed by its owner",
                    conn.origin
                ),
            ));
        }
        let _ = conn.out.try_send(Out::Close(code, reason));
        Ok(rmpv::Value::Nil)
    }

    /// Drive one look at the caller's connection per poll, as `socket`
    /// does: `Ok(None)` leaves the call pending for the next step, and a
    /// handle the caller does not hold is "no such handle" at once.
    async fn until_ready<T>(
        &self,
        caller: Caller,
        handle: HandleId,
        mut op: impl FnMut(&mut Conn) -> Result<Option<T>, CallError> + Send,
    ) -> Result<T, CallError> {
        poll_fn(|_| match self.conns.with(caller, handle, &mut op) {
            Ok(Ok(Some(value))) => Poll::Ready(Ok(value)),
            Ok(Ok(None)) => Poll::Pending,
            Ok(Err(e)) => Poll::Ready(Err(e)),
            Err(no_such) => Poll::Ready(Err(CallError::new(no_such.to_string()))),
        })
        .await
    }
}

/// The guest's headers, checked as rest checks them, plus the handshake's
/// own. The scope's injected headers go on last, where nothing can shadow
/// them.
fn header_list(args: &rmpv::Value, entry: &AllowEntry) -> Result<Vec<(String, String)>, CallError> {
    let mut out = Vec::new();
    if let Some(v) = field(args, "headers").filter(|v| !v.is_nil()) {
        let map = v
            .as_map()
            .ok_or_else(|| CallError::new("args.headers must be a table of strings"))?;
        if map.len() > HEADERS_MAX {
            return Err(CallError::new(format!(
                "at most {HEADERS_MAX} request headers, {} given",
                map.len()
            )));
        }
        for (k, v) in map {
            let (Some(name), Some(value)) = (k.as_str(), v.as_str()) else {
                return Err(CallError::new("args.headers must be a table of strings"));
            };
            let lower = name.to_ascii_lowercase();
            if lower.is_empty() || lower.contains(['\r', '\n', ':']) || value.contains(['\r', '\n'])
            {
                return Err(CallError::new(format!(
                    "header '{name}' contains a forbidden character"
                )));
            }
            if drt_connector_rest::is_reserved(&lower)
                || lower == "upgrade"
                || lower.starts_with("sec-websocket-") && lower != "sec-websocket-protocol"
            {
                return Err(CallError::new(format!(
                    "header '{name}' is set by the connector"
                )));
            }
            if !entry.guest_may_set(&lower) {
                return Err(CallError::new(format!(
                    "header '{name}' is not one this instance may set on {}",
                    ws_origin(&entry.origin)
                )));
            }
            out.push((lower, value.to_string()));
        }
    }
    for (name, value) in &entry.headers {
        out.push((name.clone(), value.clone()));
    }
    // As rest, and for rest's reason: no user-agent at all is what a
    // Cloudflare-fronted API blocks, and the Discofetch API is one.
    if !out.iter().any(|(k, _)| k == "user-agent") {
        out.push((
            "user-agent".into(),
            format!("drt/{}", env!("CARGO_PKG_VERSION")),
        ));
    }
    Ok(out)
}

fn ws_origin(url: &Url) -> String {
    format!(
        "{}://{}:{}",
        if url.tls { "wss" } else { "ws" },
        url.host,
        url.port
    )
}

// depth: the connection task

/// Everything the task needs to dial, taken from the call and the scope
/// before the call returns, so the task holds no borrow of either.
struct Dial {
    raw: String,
    url: Url,
    headers: Vec<(String, String)>,
    scope: RestScope,
}

/// The connector's own runtime, for rest's reasons: `drt run` drives calls
/// with no reactor, and a connection outlives the call that opened it.
fn own_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("drt-ws")
            .enable_all()
            .build()
            .expect("a tokio runtime for the ws connector")
    })
}

fn set(shared: &Mutex<Shared>, state: State) {
    lock(shared).state = state;
}

async fn run(
    dial: Dial,
    timeout: Duration,
    idle: Option<Duration>,
    shared: Arc<Mutex<Shared>>,
    out_rx: mpsc::Receiver<Out>,
) {
    let config = WebSocketConfig {
        max_message_size: Some(MESSAGE_MAX),
        max_frame_size: Some(MESSAGE_MAX),
        ..Default::default()
    };
    let request = match request(&dial) {
        Ok(r) => r,
        Err(why) => return set(&shared, State::Failed(why)),
    };
    let stream = match tokio::time::timeout(timeout, open(&dial)).await {
        Ok(Ok(s)) => s,
        Ok(Err(why)) => return set(&shared, State::Failed(why)),
        Err(_) => {
            return set(
                &shared,
                State::Failed(format!("connect: no answer in {} ms", timeout.as_millis())),
            )
        }
    };
    match stream {
        Stream::Plain(s) => handshake(s, request, config, timeout, idle, shared, out_rx).await,
        Stream::Tls(s) => handshake(*s, request, config, timeout, idle, shared, out_rx).await,
    }
}

enum Stream {
    Plain(tokio::net::TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
}

fn request(
    dial: &Dial,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    let mut request = dial
        .raw
        .as_str()
        .into_client_request()
        .map_err(|e| format!("the url does not make a request: {e}"))?;
    for (name, value) in &dial.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("header '{name}' is not a header name"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| format!("header '{name}' has a value that is not a header value"))?;
        request.headers_mut().append(name, value);
    }
    Ok(request)
}

/// Resolve, check every address, connect to one that passed, then TLS:
/// rest's order, and for rest's reason -- resolving again after the check
/// is the rebinding window.
async fn open(dial: &Dial) -> Result<Stream, String> {
    let url = &dial.url;
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((url.host.as_str(), url.port))
        .await
        .map_err(|e| format!("dns: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("dns: '{}' resolves to nothing", url.host));
    }
    // Plain text goes nowhere but this machine, whatever the name said.
    let addrs: Vec<std::net::SocketAddr> = if url.tls {
        addrs
    } else {
        addrs.into_iter().filter(|a| a.ip().is_loopback()).collect()
    };
    if addrs.is_empty() {
        return Err(format!(
            "'{}' does not resolve to loopback, and ws:// goes nowhere else; use wss://",
            url.host
        ));
    }
    let Some(addr) = addrs
        .iter()
        .copied()
        .find(|a| dial.scope.permits_address(a.ip()))
    else {
        return Err(format!(
            "'{}' resolves only into private address space, which this instance was not granted (allow_private)",
            url.host
        ));
    };
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let _ = tcp.set_nodelay(true);
    if !url.tls {
        return Ok(Stream::Plain(tcp));
    }
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots.add_parsable_certificates(dial.scope.extra_roots().iter().cloned());
    // rest's note on `builder()` holds here: ring is the only provider in
    // the graph, so there is no ambiguity for it to panic on.
    let config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from(url.host.clone())
        .map_err(|_| format!("tls: '{}' is not a server name", url.host))?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|e| format!("tls: {e}"))?;
    Ok(Stream::Tls(Box::new(tls)))
}

async fn handshake<S>(
    stream: S,
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
    config: WebSocketConfig,
    timeout: Duration,
    idle: Option<Duration>,
    shared: Arc<Mutex<Shared>>,
    out_rx: mpsc::Receiver<Out>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use tokio_tungstenite::tungstenite::Error;
    let shook = tokio::time::timeout(
        timeout,
        tokio_tungstenite::client_async_with_config(request, stream, Some(config)),
    )
    .await;
    let (ws, response) = match shook {
        Ok(Ok(done)) => done,
        Ok(Err(Error::Http(response))) => {
            return set(
                &shared,
                State::Failed(format!(
                    "handshake: the server answered HTTP {}",
                    response.status().as_u16()
                )),
            )
        }
        Ok(Err(e)) => return set(&shared, State::Failed(format!("handshake: {e}"))),
        Err(_) => {
            return set(
                &shared,
                State::Failed(format!(
                    "handshake: no answer in {} ms",
                    timeout.as_millis()
                )),
            )
        }
    };
    set(&shared, State::Open(response.status().as_u16()));
    pump(ws, idle, shared, out_rx).await;
}

/// The open connection: frames in to the inbox, the owner's messages out,
/// until one side closes, the connection fails, or it goes silent.
async fn pump<S>(
    mut ws: WebSocketStream<S>,
    idle: Option<Duration>,
    shared: Arc<Mutex<Shared>>,
    mut out_rx: mpsc::Receiver<Out>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let drained = lock(&shared).drained.clone();
    let mut last = tokio::time::Instant::now();
    let far = tokio::time::Instant::now() + Duration::from_secs(365 * 24 * 3600);
    let ended: (u16, String) = loop {
        let full = lock(&shared).inbox.len() >= INBOX_MAX;
        let silent_at = idle.map_or(far, |d| last + d);
        tokio::select! {
            frame = ws.next(), if !full => {
                last = tokio::time::Instant::now();
                match frame {
                    Some(Ok(Message::Text(t))) => lock(&shared).inbox.push_back(t.into()),
                    Some(Ok(Message::Binary(b))) => {
                        lock(&shared).inbox.push_back(rmpv::Value::Binary(b))
                    }
                    Some(Ok(Message::Close(frame))) => {
                        // tungstenite has queued the reply; one more read
                        // flushes it and sees the end.
                        let _ = tokio::time::timeout(
                            Duration::from_millis(CLOSE_MS),
                            ws.next(),
                        )
                        .await;
                        break match frame {
                            Some(f) => (u16::from(f.code), f.reason.into_owned()),
                            None => (1005, String::new()),
                        };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break (1006, e.to_string()),
                    None => break (1006, "the connection ended without a close frame".into()),
                }
            }
            _ = drained.notified(), if full => {}
            out = out_rx.recv() => match out {
                Some(Out::Message(m)) => {
                    if let Err(e) = ws.send(m).await {
                        break (1006, e.to_string());
                    }
                }
                Some(Out::Close(code, reason)) => {
                    close(&mut ws, code, &reason).await;
                    break (code, reason);
                }
                // The owner let go without closing: released, or its
                // handle removed. Going away, as a browser tab says it.
                None => {
                    close(&mut ws, 1001, "").await;
                    break (1001, String::new());
                }
            },
            _ = tokio::time::sleep_until(silent_at), if idle.is_some() => {
                let quiet = idle.expect("guarded").as_millis();
                close(&mut ws, 1001, "").await;
                break (1006, format!("no frame for {quiet} ms"));
            }
        }
    };
    set(&shared, State::Closed(ended.0, ended.1));
}

/// Send a close frame and wait, a bounded while, for the far end's.
async fn close<S>(ws: &mut WebSocketStream<S>, code: u16, reason: &str)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_string().into(),
    };
    let _ = tokio::time::timeout(Duration::from_millis(CLOSE_MS), async {
        if ws.close(Some(frame)).await.is_ok() {
            while let Some(Ok(_)) = ws.next().await {}
        }
    })
    .await;
}

// depth: reading the request

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `wake = true` is the owner's `inbox`; `wake = "<queue>"` is that queue;
/// anything else is no wake. `socket`'s rule.
fn wake_arg(args: &rmpv::Value) -> Option<String> {
    match field(args, "wake") {
        Some(rmpv::Value::Boolean(true)) => Some("inbox".into()),
        Some(rmpv::Value::String(s)) => s.as_str().map(String::from),
        _ => None,
    }
}

/// `vital = true`, or nothing: a flag that ties a lifetime is not inferred
/// from a truthy value.
fn vital_arg(args: &rmpv::Value) -> bool {
    field(args, "vital").and_then(|v| v.as_bool()) == Some(true)
}

fn handle_arg(args: Option<&rmpv::Value>) -> Result<HandleId, CallError> {
    args.and_then(|a| field(a, "handle"))
        .and_then(|v| v.as_u64())
        .map(HandleId)
        .ok_or_else(|| CallError::new("args.handle must be a handle number"))
}

fn ms_field(args: &rmpv::Value, name: &str) -> Result<Option<u64>, CallError> {
    match field(args, name) {
        None | Some(rmpv::Value::Nil) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| CallError::new(format!("args.{name} must be a whole number of ms"))),
    }
}

fn string_field<'a>(map: &'a rmpv::Value, name: &str) -> Result<Option<&'a str>, CallError> {
    match field(map, name) {
        None | Some(rmpv::Value::Nil) => Ok(None),
        Some(rmpv::Value::String(s)) => s
            .as_str()
            .map(Some)
            .ok_or_else(|| CallError::new(format!("args.{name} must be a string"))),
        Some(_) => Err(CallError::new(format!("args.{name} must be a string"))),
    }
}

fn field<'a>(map: &'a rmpv::Value, name: &str) -> Option<&'a rmpv::Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

/// The bytes of a msgpack `str` or `bin`: the guest's codec reads both into
/// one token.
fn string_bytes(value: &rmpv::Value) -> Option<&[u8]> {
    match value {
        rmpv::Value::String(s) => Some(s.as_bytes()),
        rmpv::Value::Binary(b) => Some(b),
        _ => None,
    }
}

fn reply(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

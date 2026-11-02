//! The `net` connector's native transport (issue #108): WebSocket both
//! ways, so two `drt start` processes link by address with a node's code
//! unchanged from the page, where the transport is a JavaScript object.
//!
//! ## surface block
//!
//! - Entry points: [`NetWsConnector::new`], which `drt`'s CLI wires as
//!   `"net"` under the `connector-net` feature; [`NetScopeType`] for the
//!   wiring; [`WsTransport`], the `drt_connector_net::Transport` it holds.
//! - Configurable: nothing of its own. The message cap, connect timeout
//!   and close wait are the ws connector's ([`MESSAGE_MAX`],
//!   [`CONNECT_MS`], `CLOSE_MS`); the backlogs are the net connector's.
//! - Fan-out: [`Conn`], a connection or a listener; [`carry`]'s `select!`,
//!   every way an open connection ends; [`serve`], every way a listener
//!   does.
//!
//! **What an address is here.** The net connector leaves that to the
//! transport, and this one says:
//!
//! - `net/listen {address = "127.0.0.1:9400"}`: an `ip:port` literal with
//!   the port named. A plain `ws://` server binds it and accepts an
//!   upgrade on any path; `peer` is the far end's `ip:port`. Port `0` is
//!   refused, because `listen` answers no bound address and a port nobody
//!   can learn cannot be dialled. No TLS: a deployment that serves off
//!   this machine puts a terminator in front (not built here).
//! - `net/open {address = "ws://127.0.0.1:9400/"}`: a `ws://` or `wss://`
//!   URL, dialled by the ws connector's own client under its own rules:
//!   plain `ws://` only to loopback, the resolved address checked against
//!   the scope before anything connects, the scope's injected headers on
//!   the upgrade where the node cannot read them.
//!
//! **The scope** names both verbs, and a verb it does not name is refused:
//!
//! ```text
//!   {listen = {"127.0.0.1:9400", "0.0.0.0:0"},  -- socket's `allow` shape
//!    open   = "wss://hub.example.com"}          -- the ws connector's scope
//! ```
//!
//! `listen` is `ip:port` literals, port `0` meaning any port on that
//! address. `open` is anything the ws connector's scope takes (an origin,
//! a list, or a table with `allow`, `allow_private`, `extra_roots`), parsed
//! by the same code. At least one must be present: a net wiring that
//! grants neither verb is a mistake to say at boot.
//!
//! **Messages, as the connector needs.** One `send` is one binary frame,
//! and one frame in, binary or text, is one `data`. A close frame from the
//! far end is `{event = "closed"}`, with a reason unless it was a plain
//! normal close. The connector closing a connection sends 1000; the
//! connector letting go of one without closing it sends 1001.
//!
//! Connections and listeners run on the ws connector's own runtime, so
//! they read, answer pings and accept while their owner is parked, under
//! `drt start` and `drt run` alike.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use drt_caps::{Scope, ScopeType};
use drt_connector::{Asker, CallResult, Caller, Connector, Notice};
use drt_connector_net::{Acceptor, Link, NetConnector, Transport};
use drt_connector_rest::{RestScope, Url};

use super::{
    close, header_list, loopback_name, own_runtime, parse_scope, request, to_http, ws_origin, Dial,
    Stream, CLOSE_MS, CONNECT_MS, MESSAGE_MAX, OUTBOX_MAX,
};

/// The net connector over [`WsTransport`], declaring [`NetScopeType`] so
/// a wiring is checked at boot. Everything else is the net connector's.
pub struct NetWsConnector(NetConnector<WsTransport>);

impl NetWsConnector {
    /// Built from the wiring's scope, which the transport enforces: the
    /// net connector hands a transport an address and nothing else.
    pub fn new(scope: Option<&Scope>) -> Result<Self, String> {
        Ok(NetWsConnector(NetConnector::new(WsTransport {
            scope: NetScope::parse(scope)?,
        })))
    }
}

pub struct NetScopeType;

impl ScopeType for NetScopeType {
    fn describe(&self) -> &str {
        "{listen?: [\"ip:port\", ...], open?: <a ws scope>}: what net/listen may bind (port 0 for any) and what net/open may reach, at least one"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        NetScope::parse(scope).map(|_| ())
    }
}

#[async_trait::async_trait]
impl Connector for NetWsConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(NetScopeType)
    }

    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        self.0.call(call, args, scope).await
    }

    async fn call_as(
        &self,
        asker: &Asker<'_>,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        self.0.call_as(asker, call, args, scope).await
    }

    fn release(&self, caller: &Caller) -> Vec<String> {
        self.0.release(caller)
    }

    fn finish(&self) -> Vec<String> {
        self.0.finish()
    }

    fn ended(&self) -> Vec<(Caller, String)> {
        self.0.ended()
    }

    fn notices(&self) -> Vec<Notice> {
        self.0.notices()
    }
}

/// One connection or one listener, as the net connector holds it. The
/// task behind it holds the other ends.
#[derive(Clone)]
pub struct Conn(Arc<ConnInner>);

struct ConnInner {
    /// Messages to the far end. `None` for a listener.
    out: Option<mpsc::Sender<Vec<u8>>>,
    /// The connector's `close`: a connection sends 1000, a listener stops
    /// accepting.
    stop: Arc<Stop>,
}

/// The connector's `close`, remembered: a task that finds its channel
/// shut must tell a close (1000) from a holder letting go (1001), and the
/// two arrive together when the connector drops what it closed.
#[derive(Default)]
struct Stop {
    asked: AtomicBool,
    notify: Notify,
}

impl Stop {
    fn ask(&self) {
        self.asked.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn was_asked(&self) -> bool {
        self.asked.load(Ordering::SeqCst)
    }
}

impl Conn {
    fn connection() -> (Conn, mpsc::Receiver<Vec<u8>>, Arc<Stop>) {
        let (out, out_rx) = mpsc::channel(OUTBOX_MAX);
        let stop = Arc::new(Stop::default());
        let conn = Conn(Arc::new(ConnInner {
            out: Some(out),
            stop: stop.clone(),
        }));
        (conn, out_rx, stop)
    }

    fn listener(stop: Arc<Stop>) -> Conn {
        Conn(Arc::new(ConnInner { out: None, stop }))
    }
}

/// The transport: the wiring's scope, and the ws connector's runtime.
pub struct WsTransport {
    scope: NetScope,
}

#[async_trait::async_trait]
impl Transport for WsTransport {
    type Conn = Conn;

    async fn open(&self, address: &str, link: Link<Conn>) -> Result<Conn, String> {
        let dial = self.scope.dial(address)?;
        let (conn, out_rx, stop) = Conn::connection();
        let (opened, answer) = oneshot::channel();
        own_runtime().spawn(dial_and_carry(dial, out_rx, stop, link, opened));
        answer
            .await
            .map_err(|_| "net: the connection task ended before it answered".to_string())??;
        Ok(conn)
    }

    async fn listen(&self, address: &str, accept: Acceptor<Conn>) -> Result<Conn, String> {
        let addr = self.scope.bind(address)?;
        let stop = Arc::new(Stop::default());
        let (bound, answer) = oneshot::channel();
        own_runtime().spawn(serve(addr, stop.clone(), accept, bound));
        answer
            .await
            .map_err(|_| "net: the listener task ended before it answered".to_string())??;
        Ok(Conn::listener(stop))
    }

    async fn send(&self, conn: &Conn, data: Vec<u8>) -> Result<(), String> {
        let out = conn
            .0
            .out
            .as_ref()
            .ok_or_else(|| "send on a listener; it takes a connection".to_string())?;
        out.send(data)
            .await
            .map_err(|_| "the connection has ended".to_string())
    }

    fn close(&self, conn: &Conn) {
        conn.0.stop.ask();
    }
}

// depth: the scope

/// What the wiring allows. `None` for a verb the scope does not name,
/// which refuses that verb.
struct NetScope {
    listen: Option<Vec<String>>,
    open: Option<RestScope>,
}

impl NetScope {
    fn parse(scope: Option<&Scope>) -> Result<NetScope, String> {
        let Some(Scope(value)) = scope else {
            return Err(
                "the net connector takes a scope naming what it may do, e.g. {\"listen\": [\"127.0.0.1:9400\"]} and/or {\"open\": \"wss://hub.example.com\"}".into(),
            );
        };
        let map = value
            .as_map()
            .ok_or("the net scope must be a table with listen and/or open")?;
        let mut listen = None;
        let mut open = None;
        for (k, v) in map {
            match k.as_str() {
                Some("listen") => {
                    let items = v
                        .as_array()
                        .ok_or("the net scope's listen must be a list of ip:port literals")?;
                    let mut entries = Vec::with_capacity(items.len());
                    for item in items {
                        let text = item
                            .as_str()
                            .ok_or("the net scope's listen must be a list of ip:port literals")?;
                        text.parse::<SocketAddr>().map_err(|e| {
                            format!("the net scope's listen names '{text}', which is not an ip:port literal: {e}")
                        })?;
                        entries.push(text.to_string());
                    }
                    if entries.is_empty() {
                        return Err("the net scope's listen is empty, so every listen would be refused; name an address or leave the key out".into());
                    }
                    listen = Some(entries);
                }
                Some("open") => {
                    let parsed = parse_scope(Some(&Scope(v.clone())))
                        .map_err(|e| e.replace("the ws scope", "the net scope's open"))?;
                    if parsed.is_empty() {
                        return Err("the net scope's open grants no origins, so every open would be refused; name one or leave the key out".into());
                    }
                    open = Some(parsed);
                }
                Some(other) => {
                    return Err(format!(
                        "the net scope has no key '{other}'; it takes listen and open"
                    ))
                }
                None => return Err("the net scope's keys must be strings".into()),
            }
        }
        if listen.is_none() && open.is_none() {
            return Err(
                "the net scope grants neither listen nor open, so every call would be refused"
                    .into(),
            );
        }
        Ok(NetScope { listen, open })
    }

    /// The address `listen` binds, once the scope allows it.
    fn bind(&self, address: &str) -> Result<SocketAddr, String> {
        let allow = self.listen.as_ref().ok_or(
            "this deployment's net scope grants no listen (the scope's listen names what may be bound)",
        )?;
        let addr: SocketAddr = address.parse().map_err(|_| {
            format!(
                "net/listen here takes an ip:port literal such as 127.0.0.1:9400, not '{address}'"
            )
        })?;
        if addr.port() == 0 {
            return Err(format!(
                "net/listen on {addr}: name the port; listen answers no bound address, so nothing could dial port 0"
            ));
        }
        let permitted = allow
            .iter()
            .filter_map(|entry| entry.parse::<SocketAddr>().ok())
            .any(|a| a.ip() == addr.ip() && (a.port() == 0 || a.port() == addr.port()));
        if !permitted {
            return Err(format!(
                "listen on {addr}: outside this deployment's net scope ({})",
                allow.join(", ")
            ));
        }
        Ok(addr)
    }

    /// What dialling `address` takes, once the scope allows it: the ws
    /// connector's checks, in its order, with no headers from the node.
    fn dial(&self, address: &str) -> Result<Dial, String> {
        let scope = self.open.as_ref().ok_or(
            "this deployment's net scope grants no open (the scope's open names the origins it may reach)",
        )?;
        if !address.starts_with("wss://") && !address.starts_with("ws://") {
            return Err(format!(
                "net/open here takes a ws:// or wss:// URL, not '{address}'"
            ));
        }
        let url = Url::parse(&to_http(address))?;
        if !url.tls && !loopback_name(&url.host) {
            return Err(format!(
                "{} is plain text off this machine; use wss:// (ws:// is for loopback only)",
                ws_origin(&url)
            ));
        }
        let entry = scope.matching(&url).cloned().ok_or_else(|| {
            format!(
                "{} is not an origin this deployment's net scope grants",
                ws_origin(&url)
            )
        })?;
        let headers = header_list(&rmpv::Value::Nil, &entry).map_err(|e| e.detail)?;
        Ok(Dial {
            raw: address.to_string(),
            url,
            headers,
            scope: scope.clone(),
        })
    }
}

// depth: the connection and listener tasks

fn config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(MESSAGE_MAX),
        max_frame_size: Some(MESSAGE_MAX),
        ..Default::default()
    }
}

/// Dial, shake hands, answer `opened`, then carry the connection. Nothing
/// reaches `link` before the answer: there is nothing to report until the
/// handshake is done.
async fn dial_and_carry(
    dial: Dial,
    out_rx: mpsc::Receiver<Vec<u8>>,
    stop: Arc<Stop>,
    link: Link<Conn>,
    opened: oneshot::Sender<Result<(), String>>,
) {
    let timeout = Duration::from_millis(CONNECT_MS);
    let request = match request(&dial) {
        Ok(r) => r,
        Err(why) => return drop(opened.send(Err(why))),
    };
    let stream = match tokio::time::timeout(timeout, super::open(&dial)).await {
        Ok(Ok(s)) => s,
        Ok(Err(why)) => return drop(opened.send(Err(why))),
        Err(_) => {
            return drop(opened.send(Err(format!(
                "connect: no answer in {} ms",
                timeout.as_millis()
            ))))
        }
    };
    match stream {
        Stream::Plain(s) => shake(s, request, timeout, out_rx, stop, link, opened).await,
        Stream::Tls(s) => shake(*s, request, timeout, out_rx, stop, link, opened).await,
    }
}

async fn shake<S>(
    stream: S,
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
    timeout: Duration,
    out_rx: mpsc::Receiver<Vec<u8>>,
    stop: Arc<Stop>,
    link: Link<Conn>,
    opened: oneshot::Sender<Result<(), String>>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use tokio_tungstenite::tungstenite::Error;
    let shook = tokio::time::timeout(
        timeout,
        tokio_tungstenite::client_async_with_config(request, stream, Some(config())),
    )
    .await;
    let ws = match shook {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(Error::Http(response))) => {
            return drop(opened.send(Err(format!(
                "handshake: the server answered HTTP {}",
                response.status().as_u16()
            ))))
        }
        Ok(Err(e)) => return drop(opened.send(Err(format!("handshake: {e}")))),
        Err(_) => {
            return drop(opened.send(Err(format!(
                "handshake: no answer in {} ms",
                timeout.as_millis()
            ))))
        }
    };
    if opened.send(Ok(())).is_err() {
        // The open was abandoned (its caller is gone): nobody holds this.
        let mut ws = ws;
        close(&mut ws, 1001, "").await;
        return;
    }
    carry(ws, out_rx, stop, link).await;
}

/// Bind, answer `bound`, then accept until the connector closes the
/// listener. Each upgrade is its own task, so a slow handshake holds up
/// nobody, and a connection outlives the listener that took it.
async fn serve(
    addr: SocketAddr,
    stop: Arc<Stop>,
    accept: Acceptor<Conn>,
    bound: oneshot::Sender<Result<(), String>>,
) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => return drop(bound.send(Err(format!("listen on {addr}: {e}")))),
    };
    if bound.send(Ok(())).is_err() {
        return;
    }
    loop {
        tokio::select! {
            arrived = listener.accept() => {
                let Ok((tcp, peer)) = arrived else { continue };
                let _ = tcp.set_nodelay(true);
                tokio::spawn(upgrade(tcp, peer, accept.clone()));
            }
            _ = stop.notify.notified() => return,
        }
    }
}

/// One arriving connection: the upgrade, then the connector hears of it.
/// A connection that never finishes its handshake is never reported.
async fn upgrade(tcp: tokio::net::TcpStream, peer: SocketAddr, accept: Acceptor<Conn>) {
    let shook = tokio::time::timeout(
        Duration::from_millis(CONNECT_MS),
        tokio_tungstenite::accept_async_with_config(tcp, Some(config())),
    )
    .await;
    let Ok(Ok(ws)) = shook else { return };
    let (conn, out_rx, stop) = Conn::connection();
    let link = accept.connection(conn, peer.to_string());
    carry(ws, out_rx, stop, link).await;
}

/// An open connection, either side: frames in to `link`, the connector's
/// messages out, until one side closes or the connection fails. `link`
/// hears `closed` only for an end the connector did not ask for.
async fn carry<S>(
    mut ws: WebSocketStream<S>,
    mut out_rx: mpsc::Receiver<Vec<u8>>,
    stop: Arc<Stop>,
    link: Link<Conn>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            frame = ws.next() => match frame {
                Some(Ok(Message::Binary(b))) => link.receive(b),
                Some(Ok(Message::Text(t))) => link.receive(t.into_bytes()),
                Some(Ok(Message::Close(frame))) => {
                    // tungstenite has queued the reply; one more read
                    // flushes it and sees the end.
                    let _ = tokio::time::timeout(Duration::from_millis(CLOSE_MS), ws.next()).await;
                    let reason = frame.and_then(|f| {
                        let code = u16::from(f.code);
                        match (code, f.reason.is_empty()) {
                            (1000, true) => None,
                            (_, false) => Some(f.reason.into_owned()),
                            (_, true) => Some(format!("closed with code {code}")),
                        }
                    });
                    return link.closed(reason);
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return link.closed(Some(e.to_string())),
                None => {
                    return link.closed(Some("the connection ended without a close frame".into()))
                }
            },
            out = out_rx.recv() => match out {
                Some(data) => {
                    if let Err(e) = ws.send(Message::Binary(data)).await {
                        return link.closed(Some(e.to_string()));
                    }
                }
                // Shut: the connector closed it, or every holder let go
                // without closing, which is going away.
                None => {
                    let code = if stop.was_asked() { 1000 } else { 1001 };
                    return close(&mut ws, code, "").await;
                }
            },
            // A send the node made before its close still goes, in order.
            _ = stop.notify.notified() => {
                while let Ok(data) = out_rx.try_recv() {
                    if ws.send(Message::Binary(data)).await.is_err() {
                        return;
                    }
                }
                return close(&mut ws, 1000, "").await;
            }
        }
    }
}

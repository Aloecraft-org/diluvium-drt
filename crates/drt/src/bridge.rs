//! The `bridge` block: a websockify-style WebSocket to TCP bridge, as
//! `drt start` runs it.
//!
//! A browser has no TCP. A client compiled for one, an Emscripten build
//! such as FreeRDP's, opens a binary WebSocket wherever it would have
//! opened a socket, and something on the far end has to turn each one
//! back into TCP. websockify is that thing; this is drt's:
//!
//! ```text
//! page --ws://bind--> [bridge] --tcp--> to (one fixed host:port)
//!                              --p2p--> to (one fixed drt://peer)
//! ```
//!
//! This is `drt p2p --listen-ws`'s role (`crate::p2p::listen_ws`), as a
//! `drt start` block: the same [`serve`], the same [`Target`]s. A `drt://`
//! target needs the `p2p` feature, which supplies the opener.
//!
//! The target is the block's, never the client's. A page reaches `to` and
//! nothing else, whatever URL or path it opened, so a bridge is scoped by
//! construction. It is not a relay: nothing is parked, nothing is paired,
//! and it runs only when a config names it.
//!
//! `drt tunnel --listen` runs as `drt p2p --listen-ws` where `p2p` is
//! built, and every tunnel mode splices through [`pump`].
//!
//! ## surface block
//!
//! - Entry points: [`block_target`], the block's `to` as a [`Target`];
//!   [`WsBridge::start`], the block bound and served; [`WsBridge::report`],
//!   its events onto the root's queue, once per pass of `drt start`'s
//!   loop; [`serve`], the accept loop over a listener the caller bound;
//!   [`pump`], one splice.
//! - Configurable: [`CHUNK`], how much is moved per read; [`SUBPROTOCOL`],
//!   the one subprotocol answered.
//! - Fan-out: [`Target`], where a connection goes (TCP, or a stream an
//!   opener hands back), matched once in [`connect`]; [`Event`], the two
//!   things a bridge says, matched once in [`event_value`].

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use drt_config::{BridgeConfig, RootConfig};

/// How much is moved per read. A throughput knob and nothing more.
const CHUNK: usize = 64 * 1024;

/// The subprotocol answered when a client offers it: raw bytes in binary
/// frames, websockify's `binary`. Emscripten's socket layer offers it on
/// every connection, and Chromium fails a handshake that offered a
/// subprotocol and got none back. A client that offers nothing gets no
/// header. websockify's older `base64` is not spoken: its frames are text,
/// and this bridge moves binary frames only.
pub const SUBPROTOCOL: &str = "binary";

/// A byte stream a bridge splices a WebSocket to.
pub trait Stream: AsyncRead + AsyncWrite + Send {}
impl<T: AsyncRead + AsyncWrite + Send> Stream for T {}

/// One opened stream, whatever carries it.
pub type Opened = std::pin::Pin<Box<dyn Stream>>;

/// Opens one stream per accepted WebSocket: how a target that is not a TCP
/// address (a `drt://` peer) is reached without this module knowing p2p.
pub type Open = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Opened, String>> + Send>>
        + Send
        + Sync,
>;

/// Who may connect, by address; `None` admits whoever reaches the bind.
pub type Admit = std::sync::Arc<dyn Fn(std::net::IpAddr) -> bool + Send + Sync>;

/// Where every connection a bridge accepts goes. Fixed when the bridge
/// starts; a client never names it.
#[derive(Clone)]
pub enum Target {
    /// One `host:port`, dialled per connection: websockify's shape.
    Tcp(String),
    /// Something else, `to` as it is shown, opened by `open`.
    Open { to: String, open: Open },
}

impl Target {
    /// The target as lines and events name it.
    pub fn shown(&self) -> &str {
        match self {
            Target::Tcp(to) | Target::Open { to, .. } => to,
        }
    }
}

/// The block's `to` as a [`Target`]: `host:port`, or a `drt://` peer when
/// this build has `p2p`, called the way `drt p2p --listen-ws` calls it with
/// the file's `default` STUN servers and extra roots.
pub fn block_target(block: &BridgeConfig, config: &RootConfig) -> Result<Target, String> {
    let to = block.to.trim();
    let scheme = to.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
    match scheme.as_deref() {
        Some(s) if s == "drt" || s.starts_with("drt+") => {
            #[cfg(feature = "p2p")]
            {
                crate::p2p::listen_ws::block_target(to, config)
                    .map_err(|e| format!("bridge.to: {e}"))
            }
            #[cfg(not(feature = "p2p"))]
            {
                let _ = config;
                Err(format!(
                    "bridge.to `{to}` is a DRT peer, and this build has no `p2p` feature (it is \
                     in `full`)"
                ))
            }
        }
        Some(_) => Err(format!(
            "bridge.to `{to}` names a scheme; it is host:port, or drt://… for a peer"
        )),
        None => {
            let _ = config;
            check_target(to)?;
            Ok(Target::Tcp(to.to_string()))
        }
    }
}

/// What a bridge says about a connection, one of each per connection that
/// reached its target. A handshake that failed, or a target that refused,
/// is a line on stderr and no event: no session was opened to close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A WebSocket is spliced to the target.
    Opened { peer: String, to: String },
    /// That splice ended. `sent` went to the target, `received` came from
    /// it, both in bytes: what a meter needs.
    Closed {
        peer: String,
        to: String,
        sent: u64,
        received: u64,
    },
}

/// The deployment's end of a bridge: the block bound, served on its own
/// runtime, and its events waiting for the drive loop.
pub struct WsBridge {
    addr: std::net::SocketAddr,
    queue: String,
    events: std::sync::mpsc::Receiver<Event>,
    /// Kept alive for the process's life: dropping it stops the bridge.
    _runtime: std::thread::JoinHandle<()>,
}

impl WsBridge {
    /// Bind the block and serve it on its own runtime, to `target`
    /// ([`block_target`], which refuses a `to` it cannot reach).
    ///
    /// Everything that can be refused is refused before `drt start` steps
    /// the root: a `bind` that is held or not an address names its key.
    pub fn start(config: &BridgeConfig, target: Target) -> Result<WsBridge, String> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| format!("the bridge needs a runtime: {e}"))?;
        let listener = rt
            .block_on(tokio::net::TcpListener::bind(&config.bind))
            .map_err(|e| format!("bridge.bind `{}`: {e}", config.bind))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let (tx, events) = std::sync::mpsc::channel();
        let runtime = std::thread::spawn(move || {
            if let Err(e) = rt.block_on(serve(listener, target, None, Some(tx))) {
                eprintln!("drt start: bridge stopped: {e}");
            }
            // Leaked for the reason every bridge here leaks one: tokio
            // 1.53.1's runtime teardown races a parked blocking worker, and
            // the target is resolved through `spawn_blocking`. See cli.rs.
            std::mem::forget(rt);
        });
        Ok(WsBridge {
            addr,
            queue: config.queue.clone(),
            events,
            _runtime: runtime,
        })
    }

    /// The address actually bound.
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    /// Push every event since the last pass onto the root's queue.
    /// Non-blocking. An event the queue refuses is dropped with a line on
    /// stderr rather than held: the session it describes has moved on.
    pub fn report(&mut self, push: &mut dyn FnMut(&str, &[u8]) -> bool) {
        while let Ok(event) = self.events.try_recv() {
            let mut msg = Vec::new();
            rmpv::encode::write_value(&mut msg, &event_value(&event))
                .expect("a bridge event encodes");
            if !push(&self.queue, &msg) {
                eprintln!(
                    "drt start: bridge: `{}` refused an event; is the queue declared?",
                    self.queue
                );
            }
        }
    }
}

/// One event as the supervisor sees it. `event` names it the way the
/// relay's and the STUN server's are named, so one `if msg.event == …`
/// chain handles them all.
fn event_value(event: &Event) -> rmpv::Value {
    match event {
        Event::Opened { peer, to } => rmpv::Value::Map(vec![
            ("event".into(), "opened".into()),
            ("peer".into(), peer.as_str().into()),
            ("to".into(), to.as_str().into()),
        ]),
        Event::Closed {
            peer,
            to,
            sent,
            received,
        } => rmpv::Value::Map(vec![
            ("event".into(), "closed".into()),
            ("peer".into(), peer.as_str().into()),
            ("to".into(), to.as_str().into()),
            ("sent".into(), rmpv::Value::from(*sent)),
            ("received".into(), rmpv::Value::from(*received)),
        ]),
    }
}

// depth: checking the target, the accept loop, the handshake, the splice

/// `to` as `host:port`, checked before anything binds: a block naming a
/// target with no port would otherwise start, serve, and refuse every
/// connection a moment later with the reason only on stderr.
fn check_target(to: &str) -> Result<(), String> {
    match to.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => Ok(()),
        _ => Err(format!("bridge.to `{to}` is not host:port")),
    }
}

/// Accept WebSockets on `listener` from whoever `admit` lets in and
/// splice each to `target`, telling `events` when each opens and closes,
/// until the listener fails. Each way a connection ends is said on stderr,
/// by peer.
pub async fn serve(
    listener: tokio::net::TcpListener,
    target: Target,
    admit: Option<Admit>,
    events: Option<std::sync::mpsc::Sender<Event>>,
) -> Result<(), String> {
    loop {
        let Ok((conn, peer)) = listener.accept().await else {
            continue;
        };
        if let Some(admit) = &admit {
            if !admit(peer.ip()) {
                eprintln!("drt bridge: {peer}: outside --host's range, closed");
                continue;
            }
        }
        // Issue #33: interactive on both of this hop's sockets; see
        // `tunnel::connect`.
        if let Err(e) = conn.set_nodelay(true) {
            eprintln!("drt bridge: {peer}: cannot set TCP_NODELAY: {e}");
            continue;
        }
        let target = target.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let ws = match tokio_tungstenite::accept_hdr_async(conn, answer_subprotocol).await {
                Ok(ws) => ws,
                Err(e) => {
                    eprintln!("drt bridge: {peer}: not a websocket handshake: {e}");
                    return;
                }
            };
            let stream = match connect(&target).await {
                Ok(stream) => stream,
                Err(e) => {
                    eprintln!("drt bridge: {peer}: cannot reach {}: {e}", target.shown());
                    return;
                }
            };
            let target = target.shown().to_string();
            let peer = peer.to_string();
            eprintln!("drt bridge: {peer}: bridged to {target}");
            if let Some(events) = &events {
                let _ = events.send(Event::Opened {
                    peer: peer.clone(),
                    to: target.clone(),
                });
            }
            let (sent, received) = pump(stream, ws).await;
            eprintln!("drt bridge: {peer}: session ended");
            if let Some(events) = &events {
                let _ = events.send(Event::Closed {
                    peer,
                    to: target,
                    sent,
                    received,
                });
            }
        });
    }
}

/// One stream to `target`: a TCP connection, interactive (issue #33), or
/// whatever the opener hands back.
async fn connect(target: &Target) -> Result<Opened, String> {
    match target {
        Target::Tcp(to) => {
            let tcp = tokio::net::TcpStream::connect(to)
                .await
                .map_err(|e| e.to_string())?;
            tcp.set_nodelay(true)
                .map_err(|e| format!("TCP_NODELAY: {e}"))?;
            Ok(Box::pin(tcp))
        }
        Target::Open { open, .. } => open().await,
    }
}

/// The handshake callback behind [`serve`]: answer [`SUBPROTOCOL`] when
/// the client's `Sec-WebSocket-Protocol` offers it, and leave the response
/// alone otherwise. Nothing is refused here: a client offering only other
/// subprotocols gets no header, which a browser then refuses on its own
/// side, and the line on stderr says so.
#[allow(clippy::result_large_err)]
fn answer_subprotocol(
    request: &Request,
    mut response: Response,
) -> Result<Response, ErrorResponse> {
    use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    let offered: Vec<&str> = request
        .headers()
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if offered.contains(&SUBPROTOCOL) {
        response.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(SUBPROTOCOL),
        );
    } else if !offered.is_empty() {
        eprintln!(
            "drt bridge: a client offered subprotocols {offered:?}; only `{SUBPROTOCOL}` is \
             spoken, so none is answered"
        );
    }
    Ok(response)
}

/// Pump bytes both ways between a byte stream and a WebSocket until either
/// side closes, and say how many went each way: `(sent, received)`, sent
/// being what the WebSocket delivered into the stream.
///
/// Split, not a `select!` over a `&mut self` transport. The previous
/// version could not split — ego-transport's `Transport` takes `&mut self`
/// for both directions, so a two-task split needed a lock, and a lock held
/// across a parked `recv().await` deadlocked the send direction on the
/// first exchange (SSH's handshake is exactly such an exchange). Its own
/// comment named the fix: "tungstenite underneath splits fine, the trait
/// hides it." This is that fix.
///
/// It also answers pings, which the caller half did not. A gate that
/// pings an idle `ProxyCommand` session — an hour into an ssh session
/// with nothing typed — was previously answered with silence.
pub async fn pump<S, T>(stream: S, ws: WebSocketStream<T>) -> (u64, u64)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut read_half, mut write_half) = tokio::io::split(stream);
    let (mut ws_out, mut ws_in) = ws.split();
    let mut buf = vec![0u8; CHUNK];
    let (mut sent, mut received) = (0u64, 0u64);
    loop {
        tokio::select! {
            read = read_half.read(&mut buf) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if ws_out.send(Message::Binary(buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                        received += n as u64;
                    }
                }
            }
            msg = ws_in.next() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => {
                        if write_half.write_all(&b).await.is_err() {
                            break;
                        }
                        let _ = write_half.flush().await;
                        sent += b.len() as u64;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = ws_out.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    let _ = write_half.shutdown().await;
    (sent, received)
}

#[cfg(test)]
mod tests {
    use super::{block_target, check_target, Target};

    /// A block's `to` is `host:port`, or a `drt://` peer where `p2p` is
    /// built; anything else is refused by its key.
    #[test]
    fn a_block_target_is_tcp_or_a_peer() {
        let block = |to: &str| drt_config::BridgeConfig {
            bind: "127.0.0.1:0".into(),
            to: to.into(),
            queue: "bridge_in".into(),
        };
        let config = drt_config::RootConfig::default();
        assert!(matches!(
            block_target(&block("10.0.0.5:3389"), &config),
            Ok(Target::Tcp(to)) if to == "10.0.0.5:3389"
        ));
        let peer = block_target(&block("drt://s.example/v1/pc"), &config);
        #[cfg(feature = "p2p")]
        assert!(matches!(peer, Ok(Target::Open { .. })));
        #[cfg(not(feature = "p2p"))]
        assert!(peer.err().unwrap().contains("no `p2p` feature"));
        let err = block_target(&block("ws://x:1"), &config).err().unwrap();
        assert!(err.contains("bridge.to"), "{err}");
    }

    /// A target is `host:port`, and a bracketed v6 address is one.
    #[test]
    fn a_target_names_a_host_and_a_port() {
        assert!(check_target("10.0.0.5:3389").is_ok());
        assert!(check_target("rdp.internal:3389").is_ok());
        assert!(check_target("[::1]:3389").is_ok());
        for bad in ["10.0.0.5", ":3389", "host:rdp", "host:70000", ""] {
            let err = check_target(bad).expect_err(bad);
            assert!(err.contains("bridge.to"), "{err}");
        }
    }
}

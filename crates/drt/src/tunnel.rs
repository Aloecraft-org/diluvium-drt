//! `drt tunnel`: SSH over WSS, as a dumb pipe with two halves.
//!
//! Discofetch fetchpoints live behind whatever NAT topology an endpoint
//! happens to have. Where hole-punching works (STUN/WebRTC), a direct path
//! exists; where it does not — CGNAT, address-and-port-dependent filtering —
//! the carrier that always works is an outbound WSS connection to something
//! reachable. SSH does not care what carries it: the protocol is
//! end-to-end over any reliable byte stream, so the honest design is a
//! **carrier bridge that never looks inside**:
//!
//! ```text
//! client half:  stdio       <-> wss://gate/fp     (ProxyCommand shape)
//! client half:  tcp listener <-> wss://gate/fp    (--local: one leg per
//!                                                  accepted connection)
//! server half:  ws listener <-> 127.0.0.1:22      (in front of any sshd)
//! ```
//!
//! The client half is deliberately the OpenSSH `ProxyCommand` contract —
//! bytes on stdio — because that is what buys "works like normal SSH"
//! without reimplementing any of it:
//!
//! ```text
//! ssh   -o ProxyCommand="drt tunnel wss://gate.example/fp" user@fp
//! rsync -e 'ssh -o ProxyCommand="drt tunnel wss://gate.example/fp"' …
//! sftp  -o ProxyCommand="drt tunnel wss://gate.example/fp" user@fp
//! ```
//!
//! rsync, sftp, `-L`/`-R` tunneling, agent forwarding — all of it is the
//! real ssh client's, inherited, because the bridge moves bytes and nothing
//! else. Host-key verification and auth stay end-to-end between the ssh
//! client and the sshd; a compromised gateway relaying the WSS leg can drop
//! the connection but reads only ciphertext. (TLS on the `wss://` leg is
//! then belt over braces — worth having so middleboxes see ordinary HTTPS,
//! not load-bearing for secrecy.)
//!
//! One known edge, and where it goes: the bridge tears down when **either**
//! direction ends, so a local EOF ends the session rather than half-closing
//! it. Under `ProxyCommand` — the case that matters — stdin stays open for
//! the ssh session's whole life, so this is invisible. It shows up only
//! when a script pipes a fixed input (`printf ... | drt tunnel <url>`),
//! where the answer can be lost to the teardown stdin's EOF triggers. A
//! real half-close needs a Close frame the peer can see. The migration to
//! tokio-tungstenite that this was waiting on has since landed — both
//! halves speak it now — so the blocker is gone and only the change
//! itself is outstanding: send Close on local EOF and keep reading until
//! the peer's Close comes back, rather than a timeout guessing when the
//! far side is finished.
//!
//! What this deliberately is not: an in-process SSH-over-WSS *client* for
//! the `host:ssh/exec` connector. That composition wants
//! `SshClientConnection` to accept an already-open stream, which is an
//! ego-transport seam (`russh` has `connect_stream`; ego-transport's
//! `connect` dials TCP itself today). Filed upstream; when it lands, the
//! ssh connector's scope grows a `via` and this file loses no code — the
//! bridge stays useful for the system ssh client forever. Until then the
//! composition is `--local` ([`local_to_ws`]): the connector dials a
//! local port, and each connection there is its own leg through the
//! relay.
//!
//! ## surface block
//!
//! - Entry points: [`resolve`], the file's `tunnel` block and the flags
//!   merged into one [`Mode`]; [`run`], that mode carried out; and the
//!   four modes themselves, [`stdio_to_ws`], [`local_to_ws`],
//!   [`ws_to_tcp`] and [`park`], which `run` dispatches to and a test
//!   drives directly. [`serve_local`] and [`serve_ws_bridge`] are the
//!   accept loops behind two of them, over a listener the caller bound.
//! - Configurable: [`CHUNK`], how much is moved per read.
//! - Fan-out: [`Mode`], the four things a tunnel can be, and the one
//!   match on it in [`run`]. [`Flags`] is the command line as typed and
//!   `drt_config::TunnelConfig` is the file; `resolve` is the only place
//!   the two are judged, so the refusals are one list in one vocabulary.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

/// The WebSocket this module speaks, on both halves.
///
/// One client, not two. The caller half used to go through
/// ego-transport's `WebSocketNative` while the device half used
/// tokio-tungstenite directly, which meant two error vocabularies, two
/// sets of frame handling, and — the reason this changed — only one of
/// them could be handed a trust store. `WebSocketNative::connect` calls
/// `connect_async` with no connector hook, so an operator behind an
/// internal CA had no way in.
pub type Ws<S> = WebSocketStream<S>;

/// A WSS connection as the caller half makes one.
pub type WsClient = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// How much is moved per read. A throughput knob and nothing more.
const CHUNK: usize = 64 * 1024;

/// The four things a tunnel can be. Which one is told by which keys are
/// present -- in the file, on the command line, or merged from both --
/// and never by a mode flag of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// `drt tunnel <url>`: stdio to the URL, OpenSSH's `ProxyCommand` shape.
    Stdio { claim: String },
    /// `drt tunnel <url> --local <bind>`: a local port, one fresh leg per
    /// accepted connection.
    Local { claim: String, bind: String },
    /// `drt tunnel --listen <addr> --to <target>`: WebSockets in, TCP out,
    /// in front of any sshd.
    Listen { listen: String, to: String },
    /// `drt tunnel --park <url> --to <target>`: the device side of the
    /// relay, dialing `to` lazily when a caller claims the leg.
    Park { park: String, to: String },
}

/// `drt tunnel`'s flags, as typed. Each is one key of the `tunnel` block
/// under its command-line name; [`resolve`] merges them over the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flags {
    /// The positional URL: the block's `claim`.
    pub url: Option<String>,
    /// `--local`: the block's `bind`.
    pub local: Option<String>,
    /// `--listen`.
    pub listen: Option<String>,
    /// `--to`.
    pub to: Option<String>,
    /// `--park`.
    pub park: Option<String>,
    /// `--extra-root`, repeatable: the block's `extra_roots`.
    pub extra_root: Vec<PathBuf>,
}

/// What [`resolve`] settles: the mode, and the PEM files to trust beside
/// the public roots, with the name a refusal about one of them should use
/// (`--extra-root` when they came from the flag, `tunnel.extra_roots` when
/// from the file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub mode: Mode,
    pub extra_roots: Vec<PathBuf>,
    pub extra_roots_key: &'static str,
}

/// Where a key came from, so a conflict can say which line and which flag
/// disagree rather than only that two keys do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    File,
    Flag,
}

/// A key's name as the operator wrote it: the block's key in the file,
/// the flag on the command line.
fn spelled(key: &str, source: Source) -> String {
    match source {
        Source::File => format!("`tunnel.{key}` in the config"),
        Source::Flag => match key {
            "claim" => "the URL on the command line".to_string(),
            "bind" => "`--local`".to_string(),
            other => format!("`--{other}`"),
        },
    }
}

/// The file's `tunnel` block and the flags, merged into one [`Mode`].
///
/// **Flags win per key.** A flag naming a key the file also names replaces
/// it; every other key the file names stands. Then the merged keys are
/// judged as one set, so a flag naming a different mode than the file is
/// exactly the conflict two flags naming two modes would be, refused by
/// name and with where each came from -- the same rule `netcheck` shipped
/// for `--reflect` over a `measure` block.
///
/// Refused here, before anything is bound or dialed: two modes in one
/// tunnel (`park` beside `claim`, `listen` beside `claim`, `park` beside
/// `listen`); `park` or `listen` without `to`; `bind` without `claim`; `to`
/// without `park` or `listen`; and nothing at all, which says what to
/// name. A `bind` or `listen` address already held is refused by the
/// mode's own bind, and an unusable PEM by `roots::load_roots_named`, each
/// naming the key it read.
pub fn resolve(file: Option<&drt_config::TunnelConfig>, flags: &Flags) -> Result<Resolved, String> {
    // One key, one source, the flag's when both name it.
    let pick = |from_flag: &Option<String>, from_file: Option<&String>| match (from_flag, from_file)
    {
        (Some(v), _) => Some((v.clone(), Source::Flag)),
        (None, Some(v)) => Some((v.clone(), Source::File)),
        (None, None) => None,
    };
    let claim = pick(&flags.url, file.and_then(|f| f.claim.as_ref()));
    let bind = pick(&flags.local, file.and_then(|f| f.bind.as_ref()));
    let park = pick(&flags.park, file.and_then(|f| f.park.as_ref()));
    let listen = pick(&flags.listen, file.and_then(|f| f.listen.as_ref()));
    let to = pick(&flags.to, file.and_then(|f| f.to.as_ref()));
    let (extra_roots, extra_roots_key) = if !flags.extra_root.is_empty() {
        (flags.extra_root.clone(), "--extra-root")
    } else {
        (
            file.map(|f| f.extra_roots.clone()).unwrap_or_default(),
            "tunnel.extra_roots",
        )
    };

    // Two modes in one tunnel, named by the keys that chose them.
    let modes: Vec<(&str, Source)> = [("claim", &claim), ("park", &park), ("listen", &listen)]
        .into_iter()
        .filter_map(|(name, key)| key.as_ref().map(|(_, source)| (name, *source)))
        .collect();
    if let [(a, sa), (b, sb), ..] = modes[..] {
        return Err(format!(
            "tunnel: {} and {} name two modes; a tunnel is one claim, one park, or one listen",
            spelled(a, sa),
            spelled(b, sb)
        ));
    }

    // A key that belongs to a mode this tunnel is not in, named as such
    // rather than ignored: in a file, a silently ignored key is exactly
    // the failure this loader exists to catch.
    let belongs = |key: &str, owner: &str, present: &Option<(String, Source)>| match present {
        Some((_, source)) => Err(format!(
            "tunnel: {} belongs with {owner}, and this tunnel has none",
            spelled(key, *source)
        )),
        None => Ok(()),
    };
    let needs = |key: &str, mode: &str, present: &Option<(String, Source)>| match present {
        Some((value, _)) => Ok(value.clone()),
        None => Err(format!(
            "tunnel: a {mode} needs `{key}`, the host:port it delivers to (`--{key}`, or \
             `tunnel.{key}` in the config)"
        )),
    };
    let mode = match (claim, park, listen) {
        (Some((claim, _)), None, None) => {
            belongs("to", "`park` or `listen`", &to)?;
            match bind {
                Some((bind, _)) => Mode::Local { claim, bind },
                None => Mode::Stdio { claim },
            }
        }
        (None, Some((park, _)), None) => {
            belongs("bind", "`claim`", &bind)?;
            let to = needs("to", "park", &to)?;
            Mode::Park { park, to }
        }
        (None, None, Some((listen, _))) => {
            belongs("bind", "`claim`", &bind)?;
            let to = needs("to", "listen", &to)?;
            Mode::Listen { listen, to }
        }
        _ => {
            belongs("bind", "`claim`", &bind)?;
            belongs("to", "`park` or `listen`", &to)?;
            return Err(
                "name a URL to bridge stdio to (with --local to serve a local port \
                        instead), --listen with --to, or --park with --to; or `tunnel` in \
                        the --config file, which takes the same keys"
                    .into(),
            );
        }
    };
    Ok(Resolved {
        mode,
        extra_roots,
        extra_roots_key,
    })
}

/// Carry out a [`Mode`]: the one match on it, and what `drt tunnel` runs
/// once [`resolve`] has spoken. Returns when the tunnel ends, which for
/// `Park` is never.
pub async fn run(mode: Mode, extra_roots: &[CertificateDer<'static>]) -> Result<(), String> {
    match mode {
        Mode::Stdio { claim } => stdio_to_ws(&claim, extra_roots).await,
        Mode::Local { claim, bind } => local_to_ws(&bind, &claim, extra_roots).await,
        Mode::Listen { listen, to } => ws_to_tcp(&listen, &to).await,
        Mode::Park { park: url, to } => park(&url, &to, extra_roots).await,
    }
}

/// Dial a `ws://` or `wss://` URL, trusting `extra_roots` beside the
/// public ones.
///
/// With no extra roots this is plain `connect_async`, which is exactly
/// what it was before — webpki's bundled roots, by way of
/// tokio-tungstenite's `rustls-tls-webpki-roots`. Supplying roots swaps
/// in a connector built from the same public set *plus* what was named:
/// **added, never substituted**, which is the rule the `rest` connector's
/// `extra_roots` already states and for its reason — a client that could
/// narrow its trust to one certificate is a footgun, and the case that
/// exists in the field is a CA that must be trusted beside the public
/// ones rather than instead of them.
pub async fn connect(
    url: &str,
    extra_roots: &[CertificateDer<'static>],
) -> Result<WsClient, String> {
    if extra_roots.is_empty() {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| format!("cannot reach {url}: {e}"))?;
        return Ok(ws);
    }
    let config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(crate::roots::store(extra_roots))
        .with_no_client_auth();
    let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(
        url,
        None,
        false,
        Some(Connector::Rustls(Arc::new(config))),
    )
    .await
    .map_err(|e| format!("cannot reach {url}: {e}"))?;
    Ok(ws)
}

/// Pump bytes both ways between a byte stream and a WebSocket until either
/// side closes.
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
async fn pump<S, T>(stream: S, ws: Ws<T>) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use futures_util::{SinkExt, StreamExt};

    let (mut read_half, mut write_half) = tokio::io::split(stream);
    let (mut ws_out, mut ws_in) = ws.split();
    let mut buf = vec![0u8; CHUNK];
    loop {
        tokio::select! {
            read = read_half.read(&mut buf) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if ws_out.send(Message::Binary(buf[..n].to_vec())).await.is_err() {
                            break;
                        }
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
    Ok(())
}

/// The client half: dial the WSS url and pump this process's stdio through
/// it — the OpenSSH `ProxyCommand` contract. Runs until either side closes.
pub async fn stdio_to_ws(url: &str, extra_roots: &[CertificateDer<'static>]) -> Result<(), String> {
    let ws = connect(url, extra_roots).await?;
    let stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    pump(stdio, ws).await
}

/// The server half: accept WebSocket connections and bridge each to a TCP
/// connection to `target` — `drt tunnel --listen 127.0.0.1:8022 --to
/// 127.0.0.1:22` in front of any sshd. One task per connection; a target
/// that refuses closes that WS and nothing else.
pub async fn ws_to_tcp(listen: &str, target: &str) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| format!("cannot bind {listen}: {e}"))?;
    eprintln!(
        "drt tunnel: ws on {} bridging to {target}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    serve_ws_bridge(listener, target).await
}

/// The accept loop behind [`ws_to_tcp`], over a listener the caller bound —
/// which is also how a test gets the port back.
pub async fn serve_ws_bridge(
    listener: tokio::net::TcpListener,
    target: &str,
) -> Result<(), String> {
    loop {
        let Ok((conn, _)) = listener.accept().await else {
            continue;
        };
        let target = target.to_string();
        tokio::spawn(async move {
            let Ok(ws) = tokio_tungstenite::accept_async(conn).await else {
                return;
            };
            let Ok(tcp) = tokio::net::TcpStream::connect(&target).await else {
                return;
            };
            let _ = pump(tcp, ws).await;
        });
    }
}

/// The program-shaped caller half (issue #13): a local TCP listener where
/// each accepted connection claims one fresh leg — its own WSS connection
/// to `url`, spliced until either side closes. `drt tunnel <url> --local
/// 127.0.0.1:2222`, and then `ssh/exec` scoped to that address, `rest`
/// dialing it, or a desktop client with no ProxyCommand support, all
/// reach a parked device through the relay from inside a program, which
/// the stdio half could not give them.
///
/// N concurrent local connections are N legs, and nothing is multiplexed
/// over one: a claim is one splice and the device replenishes on claim,
/// so the relay's accounting per leg stays true. A claim the far side
/// refuses — a wrong key or an unknown label is a 403 at upgrade time, a
/// relay that is down is a connect error — closes the accepted socket at
/// once, so a client sees a refused connection and never a half-open one
/// it sits on.
pub async fn local_to_ws(
    local: &str,
    url: &str,
    extra_roots: &[CertificateDer<'static>],
) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(local)
        .await
        .map_err(|e| format!("cannot bind {local}: {e}"))?;
    eprintln!(
        "drt tunnel: local {} claiming a leg per connection at {url}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    serve_local(listener, url, extra_roots).await
}

/// The accept loop behind [`local_to_ws`], over a listener the caller
/// bound — which is also how a test gets the port back.
pub async fn serve_local(
    listener: tokio::net::TcpListener,
    url: &str,
    extra_roots: &[CertificateDer<'static>],
) -> Result<(), String> {
    loop {
        let Ok((conn, peer)) = listener.accept().await else {
            continue;
        };
        let url = url.to_string();
        let roots = extra_roots.to_vec();
        tokio::spawn(async move {
            // Claim first, splice second. `stream_to_ws` dials before it
            // pumps, so a refused claim returns here with `conn` unread
            // and drops it -- that drop is the local close the caller
            // sees, at once, in place of a leg that never came.
            if let Err(e) = stream_to_ws(conn, &url, &roots).await {
                eprintln!("drt tunnel: {peer}: {e}");
            }
        });
    }
}

/// Bridge one already-open byte stream to the WSS url — `stdio_to_ws` with
/// the stream supplied, which is what a test (or a later in-process caller)
/// uses in place of a terminal.
pub async fn stream_to_ws<S>(
    stream: S,
    url: &str,
    extra_roots: &[CertificateDer<'static>],
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let ws = connect(url, extra_roots).await?;
    pump(stream, ws).await
}

// ---------------------------------------------------------------------------
// Park mode: the device side of the rendezvous relay
// ---------------------------------------------------------------------------

/// `drt tunnel --park <url> --to <host:port>`: hold a parked leg on the
/// relay and become a session when a caller claims it.
///
/// The state machine, and the two rules that are load-bearing:
///
/// - **The local dial is lazy.** A parked leg can sit for hours, and sshd
///   drops an idle connection at `LoginGraceTime` (120 s default) — so
///   `<host:port>` is dialed only when the first claimed bytes arrive, and
///   those bytes are replayed into it. Dial-at-park would make the first
///   session die confusingly hours later.
/// - **Replenish on claim, not on close.** The moment a parked leg sees its
///   first byte it has become a session; a fresh leg parks immediately, so
///   a second caller never waits for the first to hang up. Concurrency is
///   the pool's depth over time, and no control protocol exists to need.
///
/// The outer loop reconnects with capped exponential backoff forever — a
/// relay restart or a network blip re-parks on its own. This side uses
/// tokio-tungstenite directly, like the relay and for the relay's reasons
/// (split, whole messages, headers); the ego-transport modes above migrate
/// in their own change.
pub async fn park(
    url: &str,
    target: &str,
    extra_roots: &[CertificateDer<'static>],
) -> Result<(), String> {
    let mut backoff = Duration::from_secs(1);
    loop {
        match park_once(url, target, extra_roots).await {
            // A claim happened: the session runs detached; park again now.
            Ok(Parked::Claimed) => {
                backoff = Duration::from_secs(1);
            }
            // Idle-timeout close from the relay, or a clean drop: re-park
            // promptly — an unparked label is a device that is not home.
            Ok(Parked::Dropped) => {
                backoff = Duration::from_secs(1);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => {
                eprintln!("drt tunnel --park: {e}; retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

enum Parked {
    Claimed,
    Dropped,
}

async fn park_once(
    url: &str,
    target: &str,
    extra_roots: &[CertificateDer<'static>],
) -> Result<Parked, String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let mut ws = connect(url, extra_roots)
        .await
        .map_err(|e| format!("cannot park at {url}: {e}"))?;

    // Hold, answering pings, until the first claimed bytes arrive.
    let first = loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) if !b.is_empty() => break b,
            Some(Ok(Message::Ping(p))) => {
                let _ = ws.send(Message::Pong(p)).await;
            }
            Some(Ok(Message::Close(_))) | None => return Ok(Parked::Dropped),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(format!("parked leg: {e}")),
        }
    };

    // Claimed. The session runs detached so the caller of park() can
    // re-park immediately — replenish-on-claim is this line.
    let target = target.to_string();
    let url = url.to_string();
    tokio::spawn(async move {
        if let Err(e) = run_session(ws, &target, first).await {
            eprintln!("drt tunnel --park [{url}]: session ended: {e}");
        }
    });
    Ok(Parked::Claimed)
}

/// One claimed session: dial the local target now (lazily, on purpose),
/// replay the first bytes, splice until either side closes.
async fn run_session(
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    target: &str,
    first: impl Into<Vec<u8>>,
) -> Result<(), String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let mut tcp = tokio::net::TcpStream::connect(target)
        .await
        .map_err(|e| format!("cannot reach {target}: {e}"))?;
    let (mut tcp_read, mut tcp_write) = tcp.split();
    tcp_write
        .write_all(&first.into())
        .await
        .map_err(|e| e.to_string())?;

    let (mut ws_out, mut ws_in) = ws.split();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        tokio::select! {
            read = tcp_read.read(&mut buf) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if ws_out.send(Message::Binary(buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            msg = ws_in.next() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => {
                        if tcp_write.write_all(&b).await.is_err() { break; }
                    }
                    Some(Ok(Message::Ping(p))) => { let _ = ws_out.send(Message::Pong(p)).await; }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    let _ = tcp_write.shutdown().await;
    Ok(())
}

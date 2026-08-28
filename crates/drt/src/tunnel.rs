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
//! real half-close needs a Close frame the peer can see, and
//! ego-transport's `Transport` exposes only `send`/`recv` — so this is
//! fixed by the same migration to tokio-tungstenite the relay already
//! made, not by a timeout guessing when the far side is finished.
//!
//! What this deliberately is not: an in-process SSH-over-WSS *client* for
//! the `host:ssh/exec` connector. That composition wants
//! `SshClientConnection` to accept an already-open stream, which is an
//! ego-transport seam (`russh` has `connect_stream`; ego-transport's
//! `connect` dials TCP itself today). Filed upstream; when it lands, the
//! ssh connector's scope grows a `via` and this file loses no code — the
//! bridge stays useful for the system ssh client forever.

use std::time::Duration;

use ego_transport::transport::{Transport, TransportError};
use ego_transport::WebSocketNative;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// How much is moved per read. Formerly load-bearing for correctness:
/// ego-transport's `recv` used to copy a message into the caller's buffer
/// and silently drop the tail, so this had to be at least as large as any
/// message a peer sent, and the bridge was safe only because both halves
/// used the same size. That is fixed upstream as of ego-transport 0.1.3 —
/// a `MessageBuffer` retains the tail and returns it on the next `recv` —
/// so this is now a throughput knob and nothing more, and a peer sending
/// larger frames is no longer a corruption hazard.
const CHUNK: usize = 64 * 1024;

/// Pump bytes both ways between a byte stream and a WS transport until
/// either side closes.
///
/// One task, one `select!` loop — deliberately. ego-transport's `Transport`
/// takes `&mut self` for both directions, so a two-task split needs a lock,
/// and a lock held across a parked `recv().await` deadlocks the send
/// direction on the first exchange (SSH's handshake is exactly such an
/// exchange; this bridge's first draft proved it the hard way). The select
/// loop polls both directions and acts on whichever fires; while one
/// side's write is in flight the other side buffers in its socket, which
/// is ordinary backpressure, not a stall. The upstream ask that removes
/// the constraint is a split-capable WS — tungstenite underneath splits
/// fine, the trait hides it.
async fn pump<S>(stream: S, mut ws: WebSocketNative) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut read_half, mut write_half) = tokio::io::split(stream);
    let mut sbuf = vec![0u8; CHUNK];
    let mut wbuf = vec![0u8; CHUNK];
    loop {
        tokio::select! {
            read = read_half.read(&mut sbuf) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if ws.send(&sbuf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
            received = ws.recv(&mut wbuf) => {
                match received {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if write_half.write_all(&wbuf[..n]).await.is_err() {
                            break;
                        }
                        let _ = write_half.flush().await;
                    }
                }
            }
        }
    }
    let _ = write_half.shutdown().await;
    Ok(())
}

/// The client half: dial the WSS url and pump this process's stdio through
/// it — the OpenSSH `ProxyCommand` contract. Runs until either side closes.
pub async fn stdio_to_ws(url: &str) -> Result<(), String> {
    let ws = WebSocketNative::connect(url)
        .await
        .map_err(|e| describe(url, e))?;
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
            let Ok(ws) = WebSocketNative::accept(conn).await else {
                return;
            };
            let Ok(tcp) = tokio::net::TcpStream::connect(&target).await else {
                return;
            };
            let _ = pump(tcp, ws).await;
        });
    }
}

/// Bridge one already-open byte stream to the WSS url — `stdio_to_ws` with
/// the stream supplied, which is what a test (or a later in-process caller)
/// uses in place of a terminal.
pub async fn stream_to_ws<S>(stream: S, url: &str) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let ws = WebSocketNative::connect(url)
        .await
        .map_err(|e| describe(url, e))?;
    pump(stream, ws).await
}

fn describe(url: &str, e: TransportError) -> String {
    format!("cannot reach {url}: {e:?}")
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
pub async fn park(url: &str, target: &str) -> Result<(), String> {
    let mut backoff = Duration::from_secs(1);
    loop {
        match park_once(url, target).await {
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

async fn park_once(url: &str, target: &str) -> Result<Parked, String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws, _) = tokio_tungstenite::connect_async(url)
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

//! The call role (`doc/P2P.md` §2.1): this process is the caller in a
//! browser access session. Its end is stdio, or the local ports `-p`
//! binds, each connection to one its own stream to the far side.
//!
//! ## surface block
//!
//! - Entry points: [`connect`], a peer reached and the session up, for this
//!   role and for anything else that wants a [`Call`] (`drt ssh`, the
//!   `drt://` forward); [`run`], the role carried out.
//! - Configurable: [`Dial`], what every call is made with.
//! - Fan-out: the match on [`How`] in `connect`: a record in hand, a
//!   signalling request, or a WebSocket relay, which carries bytes and not
//!   a session and so is its own path in `run`.

use drt_rtc::caller::{Call, Caller, Target, CONNECT_TIMEOUT};
use drt_rtc::Record;
use tokio::io::AsyncWriteExt;
use tokio_rustls::rustls::pki_types::CertificateDer;

use super::http;
use super::peer::{fingerprint_text, How, Peer, PortMap};
use super::CallRole;

/// What every call is made with.
#[derive(Debug, Clone, Default)]
pub struct Dial {
    /// STUN servers asked for this side's public address (§2.5).
    pub stun: Vec<String>,
    /// Headers for the signalling request (`--H`).
    pub headers: Vec<(String, String)>,
    /// The answerer's DTLS fingerprint, when it must match (`--fingerprint`).
    pub fingerprint: Option<[u8; 32]>,
}

/// A session, and what the answerer said of itself.
pub struct Connected {
    pub call: Call,
    /// The answerer is a relay (§4.4): a caller prints `via relay`.
    pub forwarding: bool,
    pub answerer: Record,
}

/// Reach `peer` and bring the session up: the record in hand, or the
/// caller's request of `doc/DRT-Signalling.md` §3 and its reply.
pub async fn connect(
    peer: &Peer,
    dial: &Dial,
    roots: &[CertificateDer<'static>],
) -> Result<Connected, String> {
    let any = "0.0.0.0:0".parse().expect("a literal");
    let (caller, answerer) = match &peer.how {
        How::Record(record) => (Caller::direct(any).await?, record.clone()),
        How::Signal { .. } => {
            let url = peer.calls_url().expect("a signalling peer has a URL");
            let mut caller = Caller::new(any).await?;
            if !dial.stun.is_empty() {
                let found = caller.gather(&dial.stun).await;
                if found.is_empty() {
                    eprintln!(
                        "drt p2p: no STUN server answered; the record carries this side's local \
                         address only"
                    );
                }
            }
            let mine = caller
                .record()
                .encode()
                .map_err(|e| format!("this caller's record: {e}"))?;
            let reply = http::request("POST", &url, &dial.headers, Some(&mine), roots).await?;
            if let Some(why) = reply.refusal(&url) {
                return Err(why);
            }
            let record = Record::decode(reply.text().trim()).map_err(|e| {
                format!(
                    "{} answered something that is not a record: {e}",
                    crate::tunnel::shown(&url)
                )
            })?;
            (caller, record)
        }
        How::Ws(_) => return Err("a WebSocket relay carries bytes, not a session".into()),
    };
    if let Some(want) = dial.fingerprint {
        if want != answerer.fingerprint {
            return Err(format!(
                "the answerer's fingerprint is {}, not the {} given; a signalling server that \
                 answered with another peer's record would look exactly like this",
                fingerprint_text(&answerer.fingerprint),
                fingerprint_text(&want)
            ));
        }
    }
    let call = caller.connect(&answerer, CONNECT_TIMEOUT).await?;
    let forwarding = serde_json::from_str::<serde_json::Value>(call.hello())
        .map(|h| h["forwarding"] == true)
        .unwrap_or(false);
    Ok(Connected {
        call,
        forwarding,
        answerer,
    })
}

/// The role: connect, then stdio and the mapped ports until the session
/// ends. Returns when stdio ends; with ports alone, never.
pub async fn run(role: &CallRole, roots: &[CertificateDer<'static>]) -> Result<(), String> {
    if role
        .relay
        .as_ref()
        .or(role.fallback.as_ref())
        .is_some_and(|p| !matches!(p.how, How::Ws(_)))
    {
        return Err(
            "--relay and --fallback through a DRT peer are not built yet; a wss:// relay is".into(),
        );
    }
    if let Some(Peer {
        how: How::Ws(url), ..
    }) = &role.relay
    {
        return over_ws(url, &role.maps, &role.dial.headers, roots).await;
    }
    let connected = connect(&role.peer, &role.dial, roots).await?;
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if connected.forwarding {
        eprintln!("drt p2p: via relay");
    }
    let (unbound, bound): (Vec<&PortMap>, Vec<&PortMap>) =
        role.maps.iter().partition(|m| m.local.is_none());
    let stdio: Option<Target> = if role.maps.is_empty() {
        Some(match &role.peer.service {
            Some(s) => Target::Service(s.clone()),
            None => Target::Default,
        })
    } else {
        unbound.first().map(|m| m.target.clone())
    };
    let call = std::sync::Arc::new(connected.call);
    let mut acceptors = Vec::new();
    for map in bound {
        let local = map.local.expect("partitioned");
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", local))
            .await
            .map_err(|e| format!("-p {local}: cannot bind 127.0.0.1:{local}: {e}"))?;
        eprintln!("drt p2p: 127.0.0.1:{local} reaches {}", map.target);
        let (call, target) = (call.clone(), map.target.clone());
        acceptors.push(tokio::spawn(async move {
            loop {
                let Ok((conn, from)) = listener.accept().await else {
                    continue;
                };
                let _ = conn.set_nodelay(true);
                let (call, target) = (call.clone(), target.clone());
                tokio::spawn(async move {
                    match call.open(&target).await {
                        Ok((stream, closed)) => {
                            let mut conn = conn;
                            let mut stream = stream;
                            let moved = tokio::io::copy_bidirectional(&mut conn, &mut stream).await;
                            if let (Err(_), Ok(code)) = (moved, closed.await) {
                                if code != drt_rtc::wisp::reason::VOLUNTARY {
                                    eprintln!(
                                        "drt p2p: {from}: the far side closed the stream: {}",
                                        drt_rtc::wisp::reason::name(code)
                                    );
                                }
                            }
                        }
                        Err(e) => eprintln!("drt p2p: {from}: {e}"),
                    }
                });
            }
        }));
    }
    let Some(target) = stdio else {
        // Ports alone: serve until the process is stopped.
        return std::future::pending().await;
    };
    let (stream, closed) = call.open(&target).await?;
    if interactive {
        eprintln!(
            "drt p2p: connected to {}; stdin and stdout are the session",
            role.peer.shown()
        );
    }
    let (mut from_peer, mut to_peer) = tokio::io::split(stream);
    let up = async {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut to_peer).await;
    };
    let down = async {
        let mut out = tokio::io::stdout();
        let n = tokio::io::copy(&mut from_peer, &mut out).await.unwrap_or(0);
        let _ = out.flush().await;
        n
    };
    // Either direction ending ends the session, as `drt tunnel` did.
    let received = tokio::select! {
        _ = up => None,
        n = down => Some(n),
    };
    // A stream refused before a byte came back says why, in the words the
    // wire has for it.
    if received == Some(0) {
        if let Ok(code) = closed.await {
            if code != drt_rtc::wisp::reason::VOLUNTARY {
                return Err(format!(
                    "the far side closed the stream to {target}: {} (0x{code:02x})",
                    drt_rtc::wisp::reason::name(code)
                ));
            }
        }
    }
    Ok(())
}

/// `--relay wss://…`: the WebSocket relay as a carrier, bytes only. The
/// label in the URL names the destination (§4.3, §9).
async fn over_ws(
    url: &str,
    maps: &[PortMap],
    headers: &[(String, String)],
    roots: &[CertificateDer<'static>],
) -> Result<(), String> {
    let bound: Vec<u16> = maps.iter().filter_map(|m| m.local).collect();
    if bound.is_empty() {
        return crate::tunnel::stdio_to_ws(url, roots, headers).await;
    }
    let mut legs = Vec::new();
    for local in bound {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", local))
            .await
            .map_err(|e| format!("-p {local}: cannot bind 127.0.0.1:{local}: {e}"))?;
        eprintln!(
            "drt p2p: 127.0.0.1:{local} claims a leg per connection at {}",
            crate::tunnel::shown(url)
        );
        let (url, roots, headers) = (url.to_string(), roots.to_vec(), headers.to_vec());
        legs.push(tokio::spawn(async move {
            let _ = crate::tunnel::serve_local(listener, &url, &roots, &headers).await;
        }));
    }
    std::future::pending().await
}

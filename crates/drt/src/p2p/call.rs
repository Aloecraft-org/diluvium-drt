//! The call role (`doc/P2P.md` §2.1): this process is the caller in a
//! browser access session. Its end is stdio, or the local ports `-p`
//! binds, each connection to one its own stream to the far side.
//!
//! ## surface block
//!
//! - Entry points: [`connect`], a peer reached and the session up, for this
//!   role and for anything else that wants a [`Call`] (the `drt://`
//!   forward); [`session`], the same with the role's carrier (`drt ssh`);
//!   [`run`], the role carried out.
//! - Configurable: [`EOF_GRACE`]; [`Dial`], what every call is made with.
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

/// How long, after stdin ends, the far side may still answer, when it
/// does not understand half-close (an older peer).
pub const EOF_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// What every call is made with.
#[derive(Debug, Clone, Default)]
pub struct Dial {
    /// STUN servers asked for this side's public address (§2.5).
    pub stun: Vec<String>,
    /// Headers for the signalling request (`--H`).
    pub headers: Vec<(String, String)>,
    /// The answerer's DTLS fingerprint, when it must match (`--fingerprint`).
    pub fingerprint: Option<[u8; 32]>,
    /// A TURN server whose allocation is one more candidate (`--turn`).
    pub turn: Option<super::turn::TurnUri>,
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
    reach(peer, dial, roots).await.map_err(|f| f.why)
}

/// [`connect`], saying which kind of failure it was.
async fn reach(
    peer: &Peer,
    dial: &Dial,
    roots: &[CertificateDer<'static>],
) -> Result<Connected, Failed> {
    let any = "0.0.0.0:0".parse().expect("a literal");
    let (caller, answerer) = match &peer.how {
        How::Record(record) => {
            let mut caller = Caller::direct(any).await?;
            relay(&mut caller, dial).await;
            (caller, record.clone())
        }
        How::Signal { .. } => {
            let url = peer.calls_url().expect("a signalling peer has a URL");
            let mut caller = Caller::new(any).await?;
            relay(&mut caller, dial).await;
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
                return Err(why.into());
            }
            let record = Record::decode(reply.text().trim()).map_err(|e| {
                format!(
                    "{} answered something that is not a record: {e}",
                    crate::tunnel::shown(&url)
                )
            })?;
            (caller, record)
        }
        How::Ws(_) => {
            return Err(Failed::from(
                "a WebSocket relay carries bytes, not a session".to_string(),
            ))
        }
    };
    if let Some(want) = dial.fingerprint {
        if want != answerer.fingerprint {
            return Err(format!(
                "the answerer's fingerprint is {}, not the {} given; a signalling server that \
                 answered with another peer's record would look exactly like this",
                fingerprint_text(&answerer.fingerprint),
                fingerprint_text(&want)
            )
            .into());
        }
    }
    let call = caller
        .connect(&answerer, CONNECT_TIMEOUT)
        .await
        .map_err(|why| Failed { why, no_path: true })?;
    let forwarding = serde_json::from_str::<serde_json::Value>(call.hello())
        .map(|h| h["forwarding"] == true)
        .unwrap_or(false);
    Ok(Connected {
        call,
        forwarding,
        answerer,
    })
}

/// `--turn`: add the allocation as a candidate. One that cannot be had is
/// said and gone on without: the direct candidates still stand.
async fn relay(caller: &mut Caller, dial: &Dial) {
    let Some(uri) = &dial.turn else { return };
    match super::turn::allocate(uri)
        .await
        .and_then(|r| caller.relay(r))
    {
        Ok(()) => {}
        Err(e) => eprintln!("drt p2p: {e}; calling without a TURN relay"),
    }
}

/// The role: connect, then stdio and the mapped ports until the session
/// ends. Returns when stdio ends; with ports alone, never.
pub async fn run(role: &CallRole, roots: &[CertificateDer<'static>]) -> Result<(), String> {
    if let Some(Peer {
        how: How::Ws(url), ..
    }) = &role.relay
    {
        return over_ws(url, &role.maps, &role.dial.headers, roots).await;
    }
    if let Some(Peer {
        how: How::Ws(_), ..
    }) = &role.fallback
    {
        return Err("--fallback takes a DRT peer; a wss:// relay is a carrier for --relay".into());
    }
    let connected = session(role, roots).await?;
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if connected.call.via_turn() {
        eprintln!("drt p2p: via TURN");
    }
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
    let half_close = call.half_close();
    let (mut from_peer, mut to_peer) = tokio::io::split(stream);
    let up = async {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut to_peer).await;
        // Stdin ended: say so on the stream. To a peer with half-close
        // that is END, and its answer still arrives below.
        let _ = to_peer.shutdown().await;
    };
    let down = async {
        let mut out = tokio::io::stdout();
        let n = tokio::io::copy(&mut from_peer, &mut out).await.unwrap_or(0);
        let _ = out.flush().await;
        n
    };
    // The far side ending ends the session. Stdin ending does too: with a
    // peer that understands half-close, once the far side has said the
    // rest; with one that does not, after a grace for it, since there is
    // no other way to keep `printf x | drt p2p <peer>` from losing its
    // answer.
    let mut down = std::pin::pin!(down);
    let received = tokio::select! {
        _ = up => {
            if half_close {
                Some((&mut down).await)
            } else {
                tokio::time::timeout(EOF_GRACE, &mut down).await.ok()
            }
        }
        n = &mut down => Some(n),
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

/// The session a call role gets: direct, through its relay, or through its
/// fallback once no direct path was found. What `run` and `drt ssh` share.
pub async fn session(
    role: &CallRole,
    roots: &[CertificateDer<'static>],
) -> Result<Connected, String> {
    match (&role.relay, &role.fallback) {
        (Some(relay), _) => through(relay, role, roots).await,
        (None, Some(fallback)) => match connect(&role.peer, &role.dial, roots).await {
            Ok(c) => Ok(c),
            Err(why) => {
                eprintln!(
                    "drt p2p: no direct path ({why}); through {}",
                    fallback.shown()
                );
                through(fallback, role, roots).await
            }
        },
        (None, None) => match reach(&role.peer, &role.dial, roots).await {
            Ok(c) => Ok(c),
            // Measured only here: with a carrier asked for, the failure is
            // not the end of the attempt and a STUN round trip would only
            // delay the fallback.
            Err(Failed { why, no_path: true }) => {
                Err(format!("{why}; {}", network(&role.dial.stun).await))
            }
            Err(Failed { why, .. }) => Err(why),
        },
    }
}

/// Why [`reach`] failed, and whether it was the session itself (no path,
/// or the handshake) rather than the signalling round trip or a check
/// before it. Only the first is a question about the network.
struct Failed {
    why: String,
    no_path: bool,
}

impl From<String> for Failed {
    fn from(why: String) -> Failed {
        Failed {
            why,
            no_path: false,
        }
    }
}

// depth: this side's network, for a failure (doc/P2P.md §1)

/// `drt netcheck`'s verdict for the network this side runs on, measured
/// against the call's own `--stun` servers, as the clause a failure ends
/// with. Its UDP half only: that is the decisive measurement, and it is
/// the one the call's flags already name the servers for.
#[cfg(feature = "stun")]
async fn network(stun: &[String]) -> String {
    if stun.is_empty() {
        return "this side's network: not measured (no --stun server named; two measure it)"
            .to_string();
    }
    let servers: Vec<&str> = stun.iter().map(String::as_str).collect();
    let mut m = crate::netcheck::Measurements::default();
    crate::netcheck::gather::local_and_udp(&mut m, &servers, None).await;
    crate::netcheck::failure_note(&m)
}

#[cfg(not(feature = "stun"))]
async fn network(_stun: &[String]) -> String {
    "this side's network: not measured (this build has no STUN client)".to_string()
}

/// `--relay <peer>` (§4.1): call the relay as any peer, name the destination
/// on `control`, and wait for the relay to say it is joined. The session
/// then carries the destination's streams; the relay's `hello` is what the
/// caller sees, and it says `forwarding`.
async fn through(
    relay: &Peer,
    role: &CallRole,
    roots: &[CertificateDer<'static>],
) -> Result<Connected, String> {
    // The relay is reached with this side's --H and --stun; --fingerprint
    // is the destination's and nobody on this path can check it, which is
    // why §1 says a relayed session is one the user asked for.
    let dial = Dial {
        fingerprint: None,
        ..role.dial.clone()
    };
    let connected = connect(relay, &dial, roots).await?;
    if !connected.forwarding {
        return Err(format!(
            "{} is not a relay: its hello does not say forwarding (a peer with a bare --forward is)",
            relay.shown()
        ));
    }
    connected
        .call
        .control(&serde_json::json!({"t": "call", "to": role.destination}).to_string())?;
    let told = tokio::time::timeout(http::TIMEOUT, connected.call.next_control())
        .await
        .map_err(|_| format!("{} did not reach the destination in time", relay.shown()))?
        .ok_or_else(|| format!("{}: the session ended", relay.shown()))?;
    let told: serde_json::Value = serde_json::from_str(&told).unwrap_or_default();
    match told["t"].as_str() {
        Some("called") => Ok(connected),
        Some("failed") => Err(format!(
            "{} could not reach {}: {}",
            relay.shown(),
            role.peer.shown(),
            told["why"].as_str().unwrap_or("no reason given")
        )),
        _ => Err(format!(
            "{} answered something unexpected on control: {told}",
            relay.shown()
        )),
    }
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

//! `drt relay`: the rendezvous relay — parked WSS legs, paired by label,
//! spliced (the discofetch tunnel).
//!
//! # The wire protocol (v1 — DRT owns this; the edge and the API consume it)
//!
//! Both legs are dumb byte pipes in P0 (`websocat` on the device, `websocat`
//! in the caller's `ProxyCommand`), so the entire protocol is expressible in
//! **URLs, HTTP status, and raw bytes** — no custom framing a dumb client
//! could not speak. What ossifies is what a user's fingers type, so the URL
//! shapes are the versioned public surface and nothing else is:
//!
//! ```text
//!   device parks a leg:   wss://<label>--tunnel.<zone>/park/<label>?k=<park_key>
//!   caller claims a leg:  wss://<label>--tunnel.<zone>/s/<label>?k=<caller_key>
//! ```
//!
//! A **claim manifests as the first caller byte** — there is no control
//! message, because a websocat leg cannot read one. The relay holds the
//! device leg parked (answering pings to keep the NAT mapping warm) until a
//! caller connects to `/s/<label>`; then it splices the two, full-duplex,
//! counting bytes. The device, seeing traffic, immediately parks a fresh
//! leg — **replenish-on-claim**, which is the whole concurrency story and
//! needs no control channel. A parked leg with no caller is torn down on
//! its idle timeout and re-parked by the device's backoff loop.
//!
//! # What is deliberately NOT here
//!
//! - **Metering** rides out over the supervisor's queue bridge (the listen
//!   contract), so the deployment owns reporting and the relay owns only
//!   the byte counts. That keeps policy in Lua, as the rendezvous kind
//!   wants.
//! - **Tickets.** P0 verifies a static per-label key from config. The API's
//!   HMAC tickets (`{label, leg, expires}`) replace the key *values* later,
//!   not this shape — `verify_key` is the one function that changes.
//! - **Control+data channels.** Parked-pool is the subset a dumb client
//!   speaks forever; a control endpoint is added *beside* `/park`, never in
//!   place of it, if richer presence or session caps ever demand it.
//!
//! # Why tokio-tungstenite and not ego-transport here
//!
//! The same split as the HTTP listener: the C-host-shaped seam is for
//! guests, the right Rust primitive is for host-side machinery. ego
//! -transport's `Transport` cannot split (so a full-duplex splice would
//! deadlock on a lock held across `recv`), its native `recv` truncates a
//! message larger than the caller's buffer (fatal when the legs are foreign
//! websocat frames), and its `accept` hides the handshake path (the relay
//! routes on it). tokio-tungstenite splits, delivers whole messages, and
//! exposes the request. The ego-transport asks stay filed upstream; the
//! relay does not wait on them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

use drt_config::RelayConfig;

/// How long a parked leg waits for a caller before the relay drops it. The
/// device's backoff loop re-parks; a leg held forever is a NAT mapping the
/// relay cannot prove is still live.
const PARK_IDLE: Duration = Duration::from_secs(300);
/// Ping cadence on a parked leg — frequent enough to keep a CGNAT mapping
/// warm, cheap enough to hold thousands.
const PARK_PING: Duration = Duration::from_secs(25);

/// One label's live state: the parked device legs waiting for a caller.
/// A caller claims the oldest; the device replenishes.
struct Label {
    /// Senders into parked legs' outbound side. A claim takes one.
    parked: Vec<ParkedLeg>,
}

struct ParkedLeg {
    /// Bytes from the caller go here; the parked-leg task forwards them to
    /// the device and, on the first one, stops pinging.
    to_device: mpsc::Sender<Vec<u8>>,
    /// The device's bytes come back out here for the caller task to send.
    from_device: mpsc::Receiver<Vec<u8>>,
}

/// The relay's shared routing table and its config.
pub struct Relay {
    config: RelayConfig,
    labels: Mutex<HashMap<String, Label>>,
    bytes_relayed: AtomicU64,
}

impl Relay {
    pub fn new(config: RelayConfig) -> Arc<Relay> {
        Arc::new(Relay {
            config,
            labels: Mutex::new(HashMap::new()),
            bytes_relayed: AtomicU64::new(0),
        })
    }

    /// Total bytes spliced since start — the seed of the metering the
    /// supervisor will drain. A counter, not a report: reporting is the
    /// deployment's, over the queue bridge.
    pub fn bytes_relayed(&self) -> u64 {
        self.bytes_relayed.load(Ordering::Relaxed)
    }

    /// Verify a per-label key from one of the two allowlists. The one
    /// function tickets replace: today a constant-time compare against the
    /// configured static key; later an HMAC check over `{label, leg,
    /// expires}`. Nothing else in the relay knows which.
    fn verify_key(&self, label: &str, leg: Leg, presented: Option<&str>) -> bool {
        let Some(entry) = self.config.labels.get(label) else {
            return false;
        };
        let want = match leg {
            Leg::Park => &entry.park_key,
            Leg::Caller => &entry.caller_key,
        };
        // A configured empty key is a closed door, never an open one: the
        // regret asymmetry is total, so absence fails.
        if want.is_empty() {
            return false;
        }
        match presented {
            Some(k) => constant_time_eq(k.as_bytes(), want.as_bytes()),
            None => false,
        }
    }
}

#[derive(Clone, Copy)]
enum Leg {
    Park,
    Caller,
}

/// Bind and serve until cancelled. One accept loop; each connection is
/// routed by its request path to the park or claim handler.
pub async fn serve(relay: Arc<Relay>) -> Result<(), String> {
    let listener = TcpListener::bind(&relay.config.bind)
        .await
        .map_err(|e| format!("relay cannot bind {}: {e}", relay.config.bind))?;
    eprintln!(
        "drt relay: listening on {}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let relay = relay.clone();
        tokio::spawn(async move {
            let _ = handle(relay, stream).await;
        });
    }
}

/// Route one connection by its handshake path, verifying the key before the
/// WebSocket is even accepted — an unauthorized leg never upgrades.
async fn handle(relay: Arc<Relay>, stream: TcpStream) -> Result<(), String> {
    // Captured out of the handshake callback, since the callback cannot be
    // async and the routing decision is made after it.
    let route: Arc<Mutex<Option<Route>>> = Arc::new(Mutex::new(None));
    let route_cb = route.clone();
    let relay_cb = relay.clone();

    #[allow(clippy::result_large_err)] // tungstenite's ErrorResponse is what it is
    let callback = move |req: &Request, response: Response| {
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();
        let key = query
            .split('&')
            .find_map(|p| p.strip_prefix("k="))
            .map(|k| k.to_string());
        let parsed = Route::parse(&path);
        if let Some(r) = &parsed {
            if relay_cb.verify_key(&r.label, r.leg, key.as_deref()) {
                *route_cb.lock().unwrap() = Some(r.clone());
                return Ok(response);
            }
        }
        // A refusal the client can see, since a dumb pipe reads status.
        Err(Response::builder()
            .status(403)
            .body(Some("relay: unauthorized or unknown route\n".to_string()))
            .unwrap())
    };

    let ws = tokio_tungstenite::accept_hdr_async(stream, callback)
        .await
        .map_err(|e| format!("relay handshake: {e}"))?;
    let route = route.lock().unwrap().take().ok_or("relay: no route")?;

    match route.leg {
        Leg::Park => park_leg(relay, route.label, ws).await,
        Leg::Caller => claim_leg(relay, route.label, ws).await,
    }
}

#[derive(Clone)]
struct Route {
    label: String,
    leg: Leg,
}

impl Route {
    /// `/park/<label>` or `/s/<label>`. The versioned public surface; the
    /// only thing a user's fingers type.
    fn parse(path: &str) -> Option<Route> {
        let rest = path.strip_prefix('/')?;
        let (kind, label) = rest.split_once('/')?;
        if label.is_empty() || label.contains('/') {
            return None;
        }
        let leg = match kind {
            "park" => Leg::Park,
            "s" => Leg::Caller,
            _ => return None,
        };
        Some(Route {
            label: label.to_string(),
            leg,
        })
    }
}

type Ws = tokio_tungstenite::WebSocketStream<TcpStream>;

/// A device leg: register it as parked, keep the NAT mapping warm with
/// pings, and forward in both directions once a caller claims it. On claim,
/// the device sees traffic and parks a fresh leg (replenish-on-claim, the
/// device's job, not ours).
async fn park_leg(relay: Arc<Relay>, label: String, mut ws: Ws) -> Result<(), String> {
    let (to_device_tx, mut to_device_rx) = mpsc::channel::<Vec<u8>>(16);
    let (from_device_tx, from_device_rx) = mpsc::channel::<Vec<u8>>(16);

    {
        let mut labels = relay.labels.lock().unwrap();
        labels
            .entry(label.clone())
            .or_insert_with(|| Label { parked: Vec::new() })
            .parked
            .push(ParkedLeg {
                to_device: to_device_tx,
                from_device: from_device_rx,
            });
    }

    let mut ping = tokio::time::interval(PARK_PING);
    let idle = tokio::time::sleep(PARK_IDLE);
    tokio::pin!(idle);
    let mut claimed = false;

    loop {
        tokio::select! {
            // The device sent us bytes — forward to the caller. First byte
            // means a session is live; stop idling it out.
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => {
                        claimed = true;
                        relay
                            .bytes_relayed
                            .fetch_add(b.len() as u64, Ordering::Relaxed);
                        if from_device_tx.send(b).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(p))) => { let _ = ws.send(Message::Pong(p)).await; }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            // A caller's bytes to forward to the device.
            caller = to_device_rx.recv() => {
                match caller {
                    Some(b) => {
                        claimed = true;
                        relay.bytes_relayed.fetch_add(b.len() as u64, Ordering::Relaxed);
                        if ws.send(Message::Binary(b)).await.is_err() { break; }
                    }
                    None => break, // the caller hung up
                }
            }
            _ = ping.tick(), if !claimed => {
                if ws.send(Message::Ping(Vec::new())).await.is_err() { break; }
            }
            _ = &mut idle, if !claimed => {
                break; // parked too long with no caller; the device re-parks
            }
        }
    }
    // Drop this leg from the parked set if it is still there (unclaimed).
    if let Some(lbl) = relay.labels.lock().unwrap().get_mut(&label) {
        lbl.parked.retain(|p| !p.to_device.is_closed());
    }
    Ok(())
}

/// A caller leg: claim the oldest parked device leg for this label and
/// splice. If none is parked, the device is not home — 1013 (Try Again
/// Later) via a close, which a dumb pipe surfaces as a clean disconnect.
async fn claim_leg(relay: Arc<Relay>, label: String, mut ws: Ws) -> Result<(), String> {
    let leg = {
        let mut labels = relay.labels.lock().unwrap();
        labels.get_mut(&label).and_then(|l| {
            if l.parked.is_empty() {
                None
            } else {
                Some(l.parked.remove(0))
            }
        })
    };
    let Some(leg) = leg else {
        let _ = ws
            .send(Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: 1013.into(),
                    reason: "no parked leg: the device is not home".into(),
                },
            )))
            .await;
        return Ok(());
    };

    let ParkedLeg {
        to_device,
        mut from_device,
    } = leg;

    loop {
        tokio::select! {
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => {
                        if to_device.send(b).await.is_err() { break; }
                    }
                    Some(Ok(Message::Ping(p))) => { let _ = ws.send(Message::Pong(p)).await; }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            back = from_device.recv() => {
                match back {
                    Some(b) => { if ws.send(Message::Binary(b)).await.is_err() { break; } }
                    None => break, // the device leg closed
                }
            }
        }
    }
    Ok(())
}

/// Constant-time compare, so key verification does not leak length or a
/// prefix through timing. The `subtle` crate is already in the tree behind
/// the crypto connector.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

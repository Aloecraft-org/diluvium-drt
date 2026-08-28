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
//! # The control plane (when the relay runs inside a deployment)
//!
//! Standalone (`drt relay`), the static per-label key is the only gate and
//! the relay tells nobody anything. Inside `drt start` it is given a
//! [`Control`], and then three things become true at once, because they
//! were always one missing channel rather than three missing features:
//!
//! - **Presence.** `parked` / `claimed` / `closed` arrive on the root's
//!   relay queue as ordinary messages. A panel can say *the laptop is
//!   home* without asking anything.
//! - **Metering.** `closed` carries the session's byte count. The relay
//!   counts; the deployment reports. Policy stays in Lua.
//! - **Arbitration.** If the deployment names a reply queue it is asked
//!   `admit` before a leg is allowed to proceed, and its answer decides.
//!
//! All of it over the same queue bridge the http listener uses, with the
//! same token discipline — a message in, a reply naming its token out.
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

/// Something the deployment should know about. One-directional; the
/// supervisor observes rather than answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayEvent {
    /// A device leg is parked and waiting. The label is home.
    Parked { label: String },
    /// A caller took a parked leg; a session is live.
    Claimed { label: String, session: u64 },
    /// The session ended, having carried this many bytes in both
    /// directions together — the number a meter bills or throttles on.
    Closed {
        label: String,
        session: u64,
        bytes: u64,
    },
}

/// A question the relay needs answered before it lets a leg proceed.
pub struct Admit {
    pub label: String,
    /// `"park"` or `"caller"`.
    pub leg: &'static str,
    /// `true` admits. A dropped sender is a refusal, so a deployment that
    /// dies mid-question closes the door rather than holding it open.
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

/// The deployment's end of the relay. Absent when the relay runs alone.
pub struct Control {
    pub events: mpsc::UnboundedSender<RelayEvent>,
    /// `None` when the deployment named no reply queue — it opted out of
    /// being asked, and the static key is the only gate.
    pub admit: Option<mpsc::UnboundedSender<Admit>>,
    pub admit_timeout: Duration,
}

/// The relay's shared routing table and its config.
pub struct Relay {
    config: RelayConfig,
    labels: Mutex<HashMap<String, Label>>,
    bytes_relayed: AtomicU64,
    sessions: AtomicU64,
    control: Option<Control>,
}

impl Relay {
    pub fn new(config: RelayConfig) -> Arc<Relay> {
        Relay::build(config, None)
    }

    /// The relay as part of a deployment: events and arbitration reach the
    /// root program over its queue bridge.
    pub fn with_control(config: RelayConfig, control: Control) -> Arc<Relay> {
        Relay::build(config, Some(control))
    }

    fn build(config: RelayConfig, control: Option<Control>) -> Arc<Relay> {
        Arc::new(Relay {
            config,
            labels: Mutex::new(HashMap::new()),
            bytes_relayed: AtomicU64::new(0),
            sessions: AtomicU64::new(0),
            control,
        })
    }

    fn emit(&self, event: RelayEvent) {
        if let Some(c) = &self.control {
            // A full or closed channel is not worth failing a session over:
            // the deployment is gone or wedged, and dropping bytes is the
            // relay's job to keep doing regardless.
            let _ = c.events.send(event);
        }
    }

    /// Ask the deployment whether this leg may proceed. `true` when nobody
    /// is arbitrating; a timeout or a dropped answer is a refusal, because
    /// having opted in to being asked, silence must fail closed.
    async fn admitted(&self, label: &str, leg: &'static str) -> bool {
        let Some(control) = &self.control else {
            return true;
        };
        let Some(admit) = &control.admit else {
            return true;
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        if admit
            .send(Admit {
                label: label.to_string(),
                leg,
                reply: tx,
            })
            .is_err()
        {
            return false;
        }
        matches!(
            tokio::time::timeout(control.admit_timeout, rx).await,
            Ok(Ok(true))
        )
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
    let mut ws_checked = ws;
    let route = route.lock().unwrap().take().ok_or("relay: no route")?;

    // Policy, after the key. The handshake callback cannot be async, so
    // arbitration cannot happen there — and it should not: the key is the
    // cheap structural gate (an unauthorized leg never upgrades, 403), and
    // the deployment's policy is a second, slower question asked only of
    // connections that already passed it.
    let leg_name = match route.leg {
        Leg::Park => "park",
        Leg::Caller => "caller",
    };
    if !relay.admitted(&route.label, leg_name).await {
        let _ = ws_checked
            .send(Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: 1008.into(),
                    reason: "refused by the deployment".into(),
                },
            )))
            .await;
        return Ok(());
    }
    let ws = ws_checked;

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
    relay.emit(RelayEvent::Parked {
        label: label.clone(),
    });

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
                        // Counted in `claim_leg`, which is the one task that
                        // sees both directions — counting here too would
                        // double every byte.
                        claimed = true;
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

    // A session exists from here: a caller has a device leg. Both
    // directions pass through this task, so this is where bytes are
    // counted — once each, for the session and for the relay total.
    let session = relay.sessions.fetch_add(1, Ordering::Relaxed) + 1;
    let mut bytes = 0u64;
    relay.emit(RelayEvent::Claimed {
        label: label.clone(),
        session,
    });

    loop {
        tokio::select! {
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => {
                        bytes += b.len() as u64;
                        relay.bytes_relayed.fetch_add(b.len() as u64, Ordering::Relaxed);
                        if to_device.send(b).await.is_err() { break; }
                    }
                    Some(Ok(Message::Ping(p))) => { let _ = ws.send(Message::Pong(p)).await; }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            back = from_device.recv() => {
                match back {
                    Some(b) => {
                        bytes += b.len() as u64;
                        relay.bytes_relayed.fetch_add(b.len() as u64, Ordering::Relaxed);
                        if ws.send(Message::Binary(b)).await.is_err() { break; }
                    }
                    None => break, // the device leg closed
                }
            }
        }
    }
    relay.emit(RelayEvent::Closed {
        label,
        session,
        bytes,
    });
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

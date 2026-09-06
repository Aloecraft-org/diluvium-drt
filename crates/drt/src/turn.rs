//! The TURN relay: the last rung of the traversal ladder, for the peers
//! `stun` says cannot punch.
//!
//! `stun` (`stun.rs`) tells a peer what address the world sees and
//! whether that address survives a change of destination. When it does
//! not — a mapping that is new per destination, the CGNAT case — no
//! direct path exists, and the peers reach each other through a relay
//! both can reach outbound. That is this: a TURN server, in ego-transport
//! (`ego_transport::turn`) on the maintained `turn` crate, for the reason
//! that put `ssh` on russh — the protocol carries authentication,
//! allocation lifecycles, permissions and channel framing, and a bug in
//! any of them is an open relay. This module is the DRT-side wiring, as
//! `stun.rs` is for STUN: the config block, the foreground verb, and the
//! bridge `drt start` uses.
//!
//! ## Credentials: coturn's, and `crypto/turn_credential`'s
//!
//! The server verifies coturn's `use-auth-secret` scheme byte for byte:
//! a username `<expiry>:<principal>`, a password that is the base64
//! HMAC-SHA1 of that username under a shared secret, and nothing stored
//! per user. `crypto/turn_credential` mints exactly that under
//! `connectors.crypto.turn`'s secret, so a deployment that puts the same
//! secret in both blocks hands out credentials its own relay accepts —
//! and, the scheme being coturn's, credentials coturn accepts too, which
//! is what lets either stand behind the same `--ice` answer (issue #12).
//! The principal is opaque here: the server checks the HMAC and the
//! expiry and never reads the text after the colon. discofetch puts the
//! fetchpoint's label there, and it comes back out on every closing
//! report, which is what makes relay bytes attributable to a name.
//!
//! ## What reaches the supervisor
//!
//! Two kinds of message on the block's `queue`, both named by `event` so
//! one `if m.event == …` chain reads them beside the relay's and
//! `stun`'s: `turn`, a counter snapshot on the block's timer; and
//! `turn_closed`, one per allocation as it closes, carrying the principal
//! and the bytes it relayed over its whole life. The second is the one a
//! meter needs, and it is delivered as it happens rather than on the
//! timer because an allocation that opens and closes between two
//! snapshots would otherwise take its byte count with it.
//!
//! ## What it deliberately does not do
//!
//! No open relay: the block needs a secret, and a server with nothing to
//! verify refuses to bind rather than starting as a free service. No
//! bitrate cap and no per-principal quota beyond the closing report —
//! both are asked for, and both live behind this.
//!
//! ## surface block
//!
//! - Entry points: [`serve`] (`drt turn`), [`bind`], [`TurnBridge::start`]
//!   and [`TurnBridge::report`] (`drt start`).
//! - Configurable: [`KEY_MIN`], the shortest secret accepted; everything
//!   else is the `turn` block's (`drt_config::TurnConfig`).
//! - Fan-out: none. One server, one credential scheme, two messages.

use std::net::{IpAddr, SocketAddr};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use drt_config::TurnConfig;
use ego_transport::turn::{AllocationCloseHook, TurnCredentials, TurnMetrics, TurnServerConfig};
pub use ego_transport::turn::{ClosedAllocation, TurnServer, TurnSnapshot};

/// The shortest secret worth verifying against: `crypto`'s own floor.
const KEY_MIN: usize = 16;

/// Bind and serve for the life of the process. `drt turn`'s whole body.
pub async fn serve(config: &TurnConfig) -> Result<(), String> {
    let server = bind(config, None).await?;
    eprintln!(
        "drt turn: listening on {}, relaying via {}",
        server.local_addr(),
        server.relay_address()
    );
    // The server lives in tasks on this runtime; holding it here holds
    // them, and nothing ends this but the process.
    let _server = server;
    std::future::pending::<()>().await;
    Ok(())
}

/// Bind the server the config names, reporting the address actually
/// taken (a `:0` port is resolved here, which is what makes the tests
/// honest). `on_closed` is called once per allocation as it closes, from
/// the server's own task, with its final byte count.
pub async fn bind(
    config: &TurnConfig,
    on_closed: Option<AllocationCloseHook>,
) -> Result<TurnServer, String> {
    let shared_secret = secret(config)?;
    let relay_address = relay_address(config)?;
    let mut server =
        TurnServerConfig::new(relay_address, TurnCredentials::Ephemeral { shared_secret });
    server.listen_addr = config.bind.clone();
    server.relay_bind_ip = config.relay_bind.clone();
    server.realm = config.realm.clone();
    server.max_allocations = config.max_allocations;
    server.on_allocation_closed = on_closed;
    TurnServer::bind(server)
        .await
        .map_err(|e| format!("turn cannot bind {}: {e}", config.bind))
}

/// The shared secret, read once at startup and refused by name when it
/// is missing: `connectors.crypto.turn`'s three knobs, resolved in the
/// same order, because it is the same secret.
fn secret(config: &TurnConfig) -> Result<String, String> {
    let bytes = if let Some(path) = &config.key_file {
        let mut b = std::fs::read(path)
            .map_err(|e| format!("turn: cannot read key_file '{}': {e}", path.display()))?;
        // Trim one trailing newline, the common shape of a key file.
        if b.last() == Some(&b'\n') {
            b.pop();
        }
        b
    } else if let Some(var) = &config.key_env {
        std::env::var(var)
            .map_err(|_| format!("turn: env var '{var}' (key_env) is not set"))?
            .into_bytes()
    } else if let Some(inline) = &config.key {
        inline.clone().into_bytes()
    } else {
        Vec::new()
    };
    if bytes.len() < KEY_MIN {
        return Err(format!(
            "turn: the key is missing or shorter than {KEY_MIN} bytes \
             (set one of key_file, key_env, key); a relay with nothing to \
             verify against is an open relay, and is refused"
        ));
    }
    // coturn's `static-auth-secret` is text, and the HMAC is over its
    // bytes; a key that is not text could not be written in coturn's
    // config either.
    String::from_utf8(bytes)
        .map_err(|_| "turn: the key must be text (coturn's static-auth-secret is)".to_string())
}

/// The address peers are told to send to: the block's, or `bind`'s when
/// that names one and not a wildcard. A relay handing out an address
/// nobody can reach fails after authentication, which is the worst place
/// to fail, so a wildcard bind with nothing said is refused here.
fn relay_address(config: &TurnConfig) -> Result<IpAddr, String> {
    if !config.relay_address.is_empty() {
        return config.relay_address.parse().map_err(|e| {
            format!(
                "turn.relay_address '{}' is not an IP address: {e}",
                config.relay_address
            )
        });
    }
    let bound: SocketAddr = config.bind.parse().map_err(|_| {
        format!(
            "turn needs relay_address: bind '{}' is not a literal address to derive it from",
            config.bind
        )
    })?;
    if bound.ip().is_unspecified() {
        return Err(format!(
            "turn needs relay_address: bind '{}' is a wildcard, and peers must be told \
             an address they can reach",
            config.bind
        ));
    }
    Ok(bound.ip())
}

/// The deployment's end of an in-process TURN server.
///
/// `StunBridge`'s shape — the server is tokio, `drt start`'s drive loop
/// is not, so it runs on its own runtime on its own thread and the loop
/// reads from it without blocking — plus a channel the server's closing
/// reports arrive on, so `report` carries each one to the supervisor the
/// pass after it happens.
pub struct TurnBridge {
    metrics: Arc<TurnMetrics>,
    addr: SocketAddr,
    relay_address: IpAddr,
    queue: String,
    every: Duration,
    next_report: Instant,
    last: Option<TurnSnapshot>,
    closed: mpsc::Receiver<ClosedAllocation>,
    /// Kept alive for the process's life: dropping it stops the server.
    _runtime: std::thread::JoinHandle<()>,
}

impl TurnBridge {
    /// Bind the server and start serving it on its own runtime.
    ///
    /// Binding happens here, synchronously, so a port already in use, a
    /// missing secret or a wildcard bind with no relay address fails
    /// `drt start` at startup, by name, rather than becoming a thread
    /// that dies quietly a moment later.
    pub fn start(config: &TurnConfig) -> Result<TurnBridge, String> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| format!("the turn server needs a runtime: {e}"))?;
        let (tx, closed) = mpsc::channel();
        let hook: AllocationCloseHook = Arc::new(move |report| {
            // A receiver that is gone is a bridge that is gone, and a
            // bridge is never dropped before the process is.
            let _ = tx.send(report);
        });
        let server = rt.block_on(bind(config, Some(hook)))?;
        let addr = server.local_addr();
        let relay_address = server.relay_address();
        let metrics = server.metrics();
        let runtime = std::thread::spawn(move || {
            // The server lives in tasks on this runtime; holding it here
            // holds them. Nothing returns from this but process exit.
            let _server = server;
            rt.block_on(std::future::pending::<()>());
        });
        Ok(TurnBridge {
            metrics,
            addr,
            relay_address,
            queue: config.queue.clone(),
            every: Duration::from_millis(config.report_ms),
            // Report once on the first pass, so a panel has a reading
            // before the first interval has elapsed.
            next_report: Instant::now(),
            last: None,
            closed,
            _runtime: runtime,
        })
    }

    /// The address actually bound.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The address peers are told to send relayed traffic to.
    pub fn relay_address(&self) -> IpAddr {
        self.relay_address
    }

    /// Push every closing report that has arrived since the last pass,
    /// then a counter snapshot when the interval has elapsed and
    /// something changed. Non-blocking.
    ///
    /// Closes first and unconditionally: each is one allocation's final
    /// byte count, and a meter that misses one under-bills. The snapshot
    /// can afford to be dropped — the counters are cumulative, so the
    /// next one carries the running totals.
    pub fn report(&mut self, push: &mut dyn FnMut(&str, &[u8]) -> bool) {
        while let Ok(report) = self.closed.try_recv() {
            push(&self.queue, &encode(&closed_value(&report)));
        }
        if Instant::now() < self.next_report {
            return;
        }
        self.next_report = Instant::now() + self.every;
        let snap = self.metrics.snapshot();
        if self.last == Some(snap) {
            return;
        }
        self.last = Some(snap);
        push(
            &self.queue,
            &encode(&snapshot_value(&self.addr, &self.relay_address, &snap)),
        );
    }
}

fn encode(value: &rmpv::Value) -> Vec<u8> {
    let mut msg = Vec::new();
    rmpv::encode::write_value(&mut msg, value).expect("a turn report encodes");
    msg
}

/// One allocation's final report, as the supervisor sees it: who held
/// it, where it relayed, and what it cost. `principal` is nil for a
/// credential minted in the bare `<expiry>` form, which names nobody.
fn closed_value(c: &ClosedAllocation) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("event".into(), "turn_closed".into()),
        ("username".into(), c.username.as_str().into()),
        (
            "principal".into(),
            match &c.principal {
                Some(p) => p.as_str().into(),
                None => rmpv::Value::Nil,
            },
        ),
        (
            "relay_addr".into(),
            c.relay_addr.to_string().as_str().into(),
        ),
        (
            "client_addr".into(),
            c.client_addr.to_string().as_str().into(),
        ),
        ("relayed_bytes".into(), rmpv::Value::from(c.relayed_bytes)),
    ])
}

/// One snapshot as the supervisor sees it, named the way `stun`'s and the
/// relay's are so one `if msg.event == …` chain handles all three.
fn snapshot_value(addr: &SocketAddr, relay_address: &IpAddr, s: &TurnSnapshot) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("event".into(), "turn".into()),
        ("addr".into(), addr.to_string().as_str().into()),
        (
            "relay_address".into(),
            relay_address.to_string().as_str().into(),
        ),
        (
            "live_allocations".into(),
            rmpv::Value::from(s.live_allocations as u64),
        ),
        (
            "max_allocations".into(),
            rmpv::Value::from(s.max_allocations as u64),
        ),
        (
            "allocations_granted".into(),
            rmpv::Value::from(s.allocations_granted),
        ),
        // Refused for the cap, not for a bad credential: a number that
        // climbs is a relay that is full.
        (
            "allocations_refused".into(),
            rmpv::Value::from(s.allocations_refused),
        ),
        (
            "allocations_closed".into(),
            rmpv::Value::from(s.allocations_closed),
        ),
        // Bytes relayed by allocations that have since closed; the live
        // ones report theirs when they close.
        (
            "relayed_bytes_closed".into(),
            rmpv::Value::from(s.relayed_bytes_closed),
        ),
        // What the credential handler answered. `auth_refused` is a
        // username it could not accept: malformed, or expired. A forged
        // password is not that -- its username is fine, so the handler
        // answers with the key it computes (`auth_ok`) and the TURN
        // layer's integrity check refuses the message after it; the
        // allocation never happens, and `allocations_granted` says so.
        // A refused count that climbs is expired credentials or a
        // scanner; forgeries show as `auth_ok` without a grant.
        ("auth_ok".into(), rmpv::Value::from(s.auth_ok)),
        ("auth_refused".into(), rmpv::Value::from(s.auth_refused)),
        (
            "last_activity_ms".into(),
            rmpv::Value::from(s.last_activity_ms),
        ),
    ])
}

//! The STUN binding server: "what address did this datagram come from?"
//!
//! A fetchpoint behind CGNAT has no inbound address, and the relay
//! (`relay.rs`) is the answer that always works — a rendezvous point both
//! sides can reach outbound. STUN is the cheaper answer that works on most
//! networks: a peer learns its own reflexive address and the two punch a
//! hole directly, and the relay carries only the sessions where that
//! failed.
//!
//! The protocol lives in ego-transport (`ego_transport::stun`), which
//! carries the RFC 5389 codec, the client probe, and the server. This
//! module is the DRT-side wiring: the config block, the foreground verb,
//! and the reporting bridge `drt start` uses. Nothing here re-implements
//! STUN — that would be a second copy of a thing that already has one.
//!
//! ## Why a pair, not a server
//!
//! One binding server tells a peer what one vantage point saw. Two tell it
//! whether the mapping *changed* between vantage points, which is the fact
//! that decides whether hole punching can work at all
//! (`ego_transport::stun::NatMapping`). `detect_mapping` refuses below two
//! servers rather than guessing, so `stun1` and `stun2` on separate
//! addresses is the deployment that makes classification available.
//!
//! ## What it deliberately does not do
//!
//! A binding server answers strangers by design — that is the service —
//! so it drops anything that is not a well-formed binding request rather
//! than replying with an error, which would make it a reflector for
//! spoofed traffic. Rate limiting is the deployment's policy, and a
//! supervisor holding the counters this module reports is where it goes.

use std::time::{Duration, Instant};

use drt_config::StunConfig;
pub use ego_transport::stun::{StunServer, StunServerSnapshot};

/// Bind and serve until the socket fails. `drt stun`'s whole body.
pub async fn serve(config: &StunConfig) -> Result<(), String> {
    let server = bind(config).await?;
    eprintln!("drt stun: listening on {}", server.local_addr());
    server.run().await.map_err(|e| e.to_string())
}

/// Bind the server the config names, reporting the address actually taken
/// (a `:0` port is resolved here, which is what makes the tests honest).
pub async fn bind(config: &StunConfig) -> Result<StunServer, String> {
    StunServer::bind(&config.bind)
        .await
        .map_err(|e| format!("stun cannot bind {}: {e}", config.bind))
}

/// The deployment's end of an in-process STUN server.
///
/// The same shape as `RelayBridge`: the server is tokio, `drt start`'s
/// drive loop is not, so it runs on its own runtime on its own thread and
/// the loop reads its counters without blocking. Unlike the relay there is
/// no channel — a binding server is stateless and has nothing to say per
/// datagram — so the bridge holds the shared metrics handle and samples it
/// on a timer.
pub struct StunBridge {
    metrics: std::sync::Arc<ego_transport::stun::StunServerMetrics>,
    addr: std::net::SocketAddr,
    queue: String,
    every: Duration,
    next_report: Instant,
    last: Option<StunServerSnapshot>,
    /// Kept alive for the process's life: dropping it stops the server.
    _runtime: std::thread::JoinHandle<()>,
}

impl StunBridge {
    /// Bind the server and start serving it on its own runtime.
    ///
    /// Binding happens here, synchronously, so a port already in use fails
    /// `drt start` at startup with the address in the message rather than
    /// becoming a thread that dies quietly a moment later.
    pub fn start(config: &StunConfig) -> Result<StunBridge, String> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| format!("the stun server needs a runtime: {e}"))?;
        let server = rt.block_on(bind(config))?;
        let addr = server.local_addr();
        let metrics = server.metrics();
        let runtime = std::thread::spawn(move || {
            if let Err(e) = rt.block_on(server.run()) {
                eprintln!("drt start: stun stopped: {e}");
            }
        });
        Ok(StunBridge {
            metrics,
            addr,
            queue: config.queue.clone(),
            every: Duration::from_millis(config.report_ms),
            // Report once on the first pass, so a panel has a reading
            // before the first interval has elapsed.
            next_report: Instant::now(),
            last: None,
            _runtime: runtime,
        })
    }

    /// The address actually bound.
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    /// Push a counter snapshot onto the root's queue when the interval has
    /// elapsed and something has changed. Non-blocking, and silent when
    /// idle: a stateless server on a quiet network would otherwise report
    /// the same numbers forever.
    pub fn report(&mut self, push: &mut dyn FnMut(&str, &[u8]) -> bool) {
        if Instant::now() < self.next_report {
            return;
        }
        self.next_report = Instant::now() + self.every;
        let snap = self.metrics.snapshot();
        if self.last == Some(snap) {
            return;
        }
        self.last = Some(snap);
        let mut msg = Vec::new();
        rmpv::encode::write_value(&mut msg, &snapshot_value(&self.addr, &snap))
            .expect("a stun snapshot encodes");
        push(&self.queue, &msg);
    }
}

/// One snapshot as the supervisor sees it. `event` names it the way the
/// relay's events are named, so one `if msg.event == …` chain in a Lua
/// supervisor handles both without knowing which subsystem spoke.
fn snapshot_value(addr: &std::net::SocketAddr, s: &StunServerSnapshot) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("event".into(), "stun".into()),
        ("addr".into(), addr.to_string().as_str().into()),
        ("requests".into(), rmpv::Value::from(s.requests)),
        ("responses".into(), rmpv::Value::from(s.responses)),
        // Datagrams that were not binding requests. A number that climbs
        // while `requests` does not is scanner traffic, not clients.
        ("dropped".into(), rmpv::Value::from(s.dropped)),
        ("bytes_in".into(), rmpv::Value::from(s.bytes_in)),
        ("bytes_out".into(), rmpv::Value::from(s.bytes_out)),
        (
            "last_activity_ms".into(),
            rmpv::Value::from(s.last_activity_ms),
        ),
    ])
}

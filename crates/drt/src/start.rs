//! `drt start`: run the deployment — the root program and its swarm —
//! foreground, until it drains (SPEC.md §13a: one process per deployment,
//! a process supervisor backgrounds it, and there is deliberately no
//! `--detach`).
//!
//! The host here is [`DeployHost`]: `StepHost`'s drive discipline plus the
//! one thing a deployment owes its programs that the clockless bench host
//! does not — **park timeouts are honoured on this process's clock**. The
//! swarm itself owns no clock (that is `dvs.h` doctrine and `StepHost`
//! keeps it), but a deployment is precisely the someone-else whose job the
//! timeout is. Without it, `queue.wait({q}, 100)` in a deployment would be
//! a park nothing ever answers.
//!
//! Listeners are served (`crate::listen`, the C host's queue-bridge
//! contract), and the residency policy is real: a config naming
//! `residency.max_resident` gets [`enforce_residency`]'s LRU each pass.
//! What `start` does not do yet, stated rather than implied: **the control
//! endpoint** — `ps`/`pause`/`stop` reach a running deployment over the
//! sshd subsystem (SPEC.md §13a), and until that lands the only controls
//! are the terminal's.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use drt_caps::CapSet;
use drt_config::RootConfig;
use drt_connector::Dispatcher;
use drt_platform::clock::Instant;
use drt_swarm::engine::diluvium_engine::DiluviumEngine;
use drt_swarm::engine::{Instance, QueueStatus, Step, WaitSet};
use drt_swarm::pump::PumpHost;
use drt_swarm::swarm::{Driven, Swarm, SwarmHost};
use drt_swarm::InstanceId;

use crate::drive::{Next, Outcome};
#[cfg(feature = "listen")]
use crate::listen::Acceptor;

/// How long the drive loop sleeps when a step moved nothing. Latency is
/// bounded by it; idle cost is one step per tick. Event-driven wakeups
/// arrive with the listeners, which is when a socket exists to select on.
pub const IDLE_TICK: Duration = Duration::from_millis(1);

/// `StepHost` with a clock: resume a parked instance when a waited queue is
/// ready, or when its own stated timeout has elapsed — whichever comes
/// first. The wait cache follows residency exactly as `StepHost`'s does.
#[derive(Default)]
pub struct DeployHost {
    parked: HashMap<u32, Park>,
    /// A drive counter, and when each instance last did work (ran or was
    /// resumed — an idle "still parked" check is not activity). The
    /// residency policy's LRU order, kept here because the host is the
    /// only place that sees work happen.
    tick: u64,
    active: HashMap<u32, u64>,
}

struct Park {
    wait: WaitSet,
    /// When the wait's timeout elapses, on our clock. `None` for a park
    /// with no timeout — those wait for a message and nothing else.
    deadline: Option<Instant>,
}

impl DeployHost {
    pub fn new() -> DeployHost {
        DeployHost::default()
    }

    fn note(&mut self, id: InstanceId, step: Step) -> Driven {
        match step {
            Step::Parked(wait) => {
                let deadline = wait.timeout.map(|t| Instant::now() + t);
                self.parked.insert(id.0, Park { wait, deadline });
                Driven::Alive
            }
            Step::Done => {
                self.parked.remove(&id.0);
                Driven::Exited
            }
        }
    }
}

fn ready(wait: &WaitSet, info: QueueStatus) -> bool {
    if wait.for_space {
        info.len < info.capacity
    } else {
        info.len > 0
    }
}

impl SwarmHost for DeployHost {
    fn drive(&mut self, id: InstanceId, _caps: &CapSet, inst: &mut dyn Instance) -> Driven {
        let park = match self.parked.get(&id.0) {
            Some(park) => Some((park.wait, park.deadline)),
            // The restored-instance path: its park was never returned by
            // anything here, so ask. A fresh instance answers `None` and
            // runs.
            None => inst.current_wait().map(|wait| {
                let deadline = wait.timeout.map(|t| Instant::now() + t);
                (wait, deadline)
            }),
        };
        self.tick += 1;
        let step = match park {
            None => {
                self.active.insert(id.0, self.tick);
                inst.run()
            }
            Some((wait, deadline)) => {
                let fired = wait
                    .queues()
                    .iter()
                    .copied()
                    .find(|&q| inst.queue_info(q).map(|i| ready(&wait, i)).unwrap_or(false));
                match fired {
                    Some(q) => {
                        self.active.insert(id.0, self.tick);
                        inst.resume(q)
                    }
                    // A queue firing beats the timeout when both are true on
                    // the same tick — the message was there first as far as
                    // anyone can observe, and answering it loses nothing.
                    None if deadline.is_some_and(|d| Instant::now() >= d) => {
                        self.active.insert(id.0, self.tick);
                        inst.resume_timeout()
                    }
                    None => {
                        // Keep the deadline armed across residency: the
                        // cache entry may have been dropped by a wake.
                        self.parked.insert(id.0, Park { wait, deadline });
                        return Driven::Alive;
                    }
                }
            }
        };
        match step {
            Ok(step) => self.note(id, step),
            Err(e) => {
                self.parked.remove(&id.0);
                // A deployment that loses an instance says so. The swarm
                // reports the fault to the parent's system/events queue,
                // which is right for supervised children — but the root has
                // no supervisor but this process, and a deployment that
                // drains silently after a root fault reads as a mystery
                // exit, not a diagnosis.
                let _ = writeln!(
                    drt_platform::stdio::stderr(),
                    "drt: instance {} faulted: {e}",
                    id.0
                );
                Driven::Faulted(e.to_string())
            }
        }
    }

    fn attached(&mut self, id: InstanceId) {
        self.parked.remove(&id.0);
    }

    fn detached(&mut self, id: InstanceId) {
        self.parked.remove(&id.0);
    }
}

impl DeployHost {
    /// When the instance last did work, in drive ticks. An instance the
    /// host never drove reads 0 — the oldest possible, which is right: it
    /// has done nothing since it arrived.
    pub fn last_active(&self, id: InstanceId) -> u64 {
        self.active.get(&id.0).copied().unwrap_or(0)
    }
}

/// Hold the residency budget: hibernate the least-recently-active
/// instances past it. The same LRU the churn harness carries (ties evict
/// the highest id, matching the C's `<=` scan), applied with the
/// deployment's own exemptions:
///
/// - The **root** is exempt — it holds the request queues, and a
///   deployment whose front door hibernates is not saving memory, it is
///   closed.
/// - An instance without **`wake_on_message`** is exempt: the delivery
///   table makes a cached instance without that flag `Gone` to every
///   sender, so hibernating one would disconnect its mailbox, not park
///   it. Such an instance's memory is what its spawner chose for it.
///
/// A hibernate the swarm refuses (the instance is mid-run) is skipped, as
/// the churn harness skips it: it will be evicted on a later pass.
pub fn enforce_residency(sw: &mut Deployment, root: InstanceId, max_resident: usize) {
    let mut candidates: Vec<(u64, u32, InstanceId)> = sw
        .ids()
        .into_iter()
        .filter(|id| *id != root && sw.resident(*id) && sw.wake_on_message(*id))
        .map(|id| (sw.host().inner().last_active(id), id.0, id))
        .collect();
    if candidates.len() <= max_resident {
        return;
    }
    // Oldest activity first; among ties, the highest id.
    candidates.sort_by_key(|(at, raw, _)| (*at, std::cmp::Reverse(*raw)));
    let mut over = candidates.len() - max_resident;
    for (_, _, id) in candidates {
        if over == 0 {
            break;
        }
        if sw.hibernate(id).is_ok() {
            over -= 1;
        }
    }
}

/// The earliest pending deadline across every park, so the idle sleep never
/// overshoots a timeout an instance asked for.
fn next_deadline(host: &DeployHost) -> Option<Instant> {
    host.parked.values().filter_map(|p| p.deadline).min()
}

/// The deployment, driven one step at a time (doc/Wasm.md D6): the swarm,
/// its residency policy, and the clock arithmetic that says how long the
/// host may sleep before the next step is due. Never sleeps itself — the
/// native loops in this file do, and a page schedules a timer.
pub struct DeployDriver {
    sw: Deployment,
    root: InstanceId,
    max_resident: Option<usize>,
}

impl DeployDriver {
    pub fn new(config: &RootConfig, dispatcher: Dispatcher) -> Result<Self, String> {
        let (sw, root) = deployment(config, dispatcher)?;
        Ok(DeployDriver {
            sw,
            root,
            max_resident: config.residency.map(|r| r.max_resident),
        })
    }

    /// One step of the swarm, and the residency policy after it. Returns
    /// how many instances are alive, which is the loop's own termination
    /// condition.
    pub fn step(&mut self) -> usize {
        let alive = self.sw.step();
        if let Some(max_resident) = self.max_resident {
            enforce_residency(&mut self.sw, self.root, max_resident);
        }
        alive
    }

    /// How long the host may sleep before the next step is due: until the
    /// earliest park deadline, and never longer than [`IDLE_TICK`].
    pub fn idle(&self) -> Duration {
        next_deadline(self.sw.host().inner())
            .map(|d| d.saturating_duration_since(Instant::now()).min(IDLE_TICK))
            .unwrap_or(IDLE_TICK)
    }

    /// [`DeployDriver::step`] and [`DeployDriver::idle`] as one answer, for
    /// a host with nothing to do between steps but wait.
    pub fn tick(&mut self) -> Next {
        if self.step() == 0 {
            Next::Done(Outcome::Exited)
        } else {
            Next::Sleep(self.idle())
        }
    }

    pub fn root(&self) -> InstanceId {
        self.root
    }

    /// The swarm, for a host that pumps its queues directly — the listener
    /// bridge, the relay's control plane, an observer.
    pub fn deployment_mut(&mut self) -> &mut Deployment {
        &mut self.sw
    }

    pub fn dispatcher(&self) -> &Dispatcher {
        self.sw.host().dispatcher()
    }
}

/// The deployment, ready to be driven by a host that owns the loop — a
/// page (doc/Wasm.md §5). Listeners are refused: a host that cannot bind
/// a port must not run a deployment that asked for one and silently not
/// bind it, which is the same refusal a build without `listen` makes.
pub fn prepare(config: &RootConfig, dispatcher: Dispatcher) -> Result<DeployDriver, String> {
    if !config.listeners.is_empty() {
        return Err(no_listeners_here(config.listeners.len()));
    }
    DeployDriver::new(config, dispatcher)
}

fn no_listeners_here(count: usize) -> String {
    format!(
        "this config names {count} listener(s), and this build does not carry \
         `listen` — running the deployment while silently not binding the \
         port the config asked for is the worst of both. Build with the \
         `listen` feature, or remove the `listeners` block"
    )
}

/// Run the deployment to completion. Returns when the swarm drains — every
/// instance exited — which for a server-shaped root program is never, and
/// foreground-forever is the contract.
pub fn start(config: &RootConfig, dispatcher: Dispatcher) -> Result<(), String> {
    #[cfg(feature = "listen")]
    {
        let bound = crate::listen::bind(&config.listeners)?;
        for (listener, addr) in config.listeners.iter().zip(bound.addrs()) {
            eprintln!("drt start: {} listening on {addr}", listener.scheme);
        }
        serve(config, dispatcher, bound)
    }
    #[cfg(not(feature = "listen"))]
    {
        serve_swarm_only(config, dispatcher)
    }
}

/// The deployment with its listeners: the drive loop, with the ingress
/// channel doubling as the idle sleep so a request never waits on a tick.
#[cfg(feature = "listen")]
pub fn serve<B: Acceptor>(
    config: &RootConfig,
    dispatcher: Dispatcher,
    bound: B,
) -> Result<(), String> {
    serve_with_observer(config, dispatcher, bound, |_, _| {})
}

/// [`serve`], with a hook run once per pass after the swarm has stepped
/// and everything pending has been delivered.
///
/// The embedding seam for a host that wants to watch its own deployment
/// without owning the loop: read an exported queue, sample usage, notice a
/// child that faulted. `drt ps` will want exactly this once the control
/// endpoint can ask for it, and a test wants it now for the same reason —
/// the loop owns the `Swarm`, so observing means being called by it.
///
/// The observer runs on the drive thread and blocks the next step, so it
/// should do bookkeeping, not work.
///
/// Generic over the acceptor so the polled one (wasi's) is driven by this
/// same loop natively, under test, and not only under wasmtime.
#[cfg(feature = "listen")]
pub fn serve_with_observer<B: Acceptor>(
    config: &RootConfig,
    dispatcher: Dispatcher,
    mut bound: B,
    mut observe: impl FnMut(&mut Deployment, InstanceId),
) -> Result<(), String> {
    let mut driver = DeployDriver::new(config, dispatcher)?;
    let root = driver.root();

    // The relay, if the config names one, on its own runtime beside this
    // loop. Its events and questions reach the root over the queue bridge
    // — the same one the listener uses — so presence, metering and
    // arbitration arrive as ordinary messages a supervisor already knows
    // how to read.
    #[cfg(feature = "relay")]
    let mut relay = match &config.relay {
        Some(cfg) => Some(RelayBridge::start(cfg.clone())?),
        None => None,
    };

    // The STUN server, same arrangement: its own runtime, its counters on
    // the same queue bridge. Stateless, so it has nothing to ask and
    // nothing to answer — it only reports.
    #[cfg(feature = "stun")]
    let mut stun = match &config.stun {
        Some(cfg) => {
            let bridge = crate::stun::StunBridge::start(cfg)?;
            eprintln!("drt stun: listening on {}", bridge.addr());
            Some(bridge)
        }
        None => None,
    };

    // Step before delivering, always: the first step runs the root to its
    // first park, so the request queue exists before the first request is
    // pushed at it. Delivering first would race the program's own
    // `queue.declare` and answer an early connection 503 for arriving
    // while the deployment was still clearing its throat.
    loop {
        let alive = driver.step();
        let sw = driver.deployment_mut();
        pump_replies(sw, root, &mut bound);
        #[cfg(feature = "relay")]
        if let Some(relay) = relay.as_mut() {
            // Answers first: a question asked last pass is waiting, and a
            // caller is holding a socket open for it.
            relay.collect(sw, root);
            relay.deliver(sw, root);
        }
        #[cfg(feature = "stun")]
        if let Some(stun) = stun.as_mut() {
            // A dropped report costs a panel one interval's numbers, and
            // the next report carries the running totals anyway — the
            // counters are cumulative, not deltas, so nothing is lost.
            stun.report(&mut |queue, msg| sw.push(root, queue, msg).is_ok());
        }
        observe(sw, root);
        if alive == 0 {
            // The program chose to exit; ports serving a drained swarm
            // would answer 503 forever, and a supervisor should see the
            // exit instead.
            return crate::run::finish(driver.dispatcher());
        }
        let sleep = driver.idle();
        if let Some(ingress) = bound.next_within(sleep) {
            deliver(driver.deployment_mut(), root, &mut bound, ingress);
        }
        while let Some(ingress) = bound.try_next() {
            deliver(driver.deployment_mut(), root, &mut bound, ingress);
        }
    }
}

#[cfg(not(feature = "listen"))]
fn serve_swarm_only(config: &RootConfig, dispatcher: Dispatcher) -> Result<(), String> {
    let mut driver = prepare(config, dispatcher)?;
    loop {
        match driver.tick() {
            Next::Sleep(sleep) => {
                if !sleep.is_zero() {
                    std::thread::sleep(sleep);
                }
            }
            Next::Done(_) => return crate::run::finish(driver.dispatcher()),
            Next::Input | Next::Stuck { .. } => unreachable!("a deployment asks only for time"),
            Next::Failed(why) => return Err(why),
        }
    }
}

/// The deployment's concrete swarm: the pump answering hostcalls, the
/// clocked host underneath.
pub type Deployment = Swarm<PumpHost<DeployHost>>;

fn deployment(
    config: &RootConfig,
    dispatcher: Dispatcher,
) -> Result<(Deployment, InstanceId), String> {
    let source = root_source(config)?;
    let engine = Arc::new(DiluviumEngine::new().map_err(|e| e.to_string())?);
    let mut sw = Swarm::new(engine, PumpHost::new(DeployHost::new(), dispatcher));
    let root = sw
        .root(
            source.as_bytes(),
            crate::config::ceiling(config),
            config.root.budget,
        )
        .map_err(|e| format!("the root program would not start: {e}"))?;
    Ok((sw, root))
}

/// One parsed request onto the root's queue. The refusals and their texts
/// are `dhost_http.c`'s, so an operator's runbook matches either host.
#[cfg(feature = "listen")]
fn deliver<B: Acceptor>(
    sw: &mut Deployment,
    root: InstanceId,
    bound: &mut B,
    ingress: crate::listen::Ingress,
) {
    use crate::listen::Outcome;
    use drt_swarm::swarm::SwarmError;
    let queue = bound.listeners()[ingress.listener].queue.clone();
    match sw.push(root, &queue, &ingress.message) {
        Ok(_) => {} // parked in the queue; the reply pump answers
        Err(SwarmError::UnknownQueue) => bound.answer(
            ingress.token,
            Outcome::refused(503, "the program declares no request queue\n"),
        ),
        Err(SwarmError::Gone | SwarmError::Unknown) => bound.answer(
            ingress.token,
            Outcome::refused(503, "the root instance is not resident\n"),
        ),
        Err(SwarmError::Limit(_)) => bound.answer(
            ingress.token,
            Outcome::refused(503, "the request queue is full; try again\n"),
        ),
        Err(_) => bound.answer(ingress.token, Outcome::refused(500, "delivery failed\n")),
    }
}

/// Drain every reply queue and finish the connections the replies name.
/// Two listeners may name the same reply queue (all ports feeding one
/// supervisor loop is a sane deployment), so a queue already drained this
/// pass is skipped. A reply naming no waiting connection is consumed all
/// the same.
#[cfg(feature = "listen")]
fn pump_replies<B: Acceptor>(sw: &mut Deployment, root: InstanceId, bound: &mut B) {
    let listeners: Vec<Arc<crate::listen::ListenerRt>> = bound.listeners().to_vec();
    let mut drained: Vec<&str> = Vec::new();
    for rt in &listeners {
        if drained.contains(&rt.reply_queue.as_str()) {
            continue;
        }
        drained.push(&rt.reply_queue);
        let Some(inst) = sw.instance_mut(root) else {
            return;
        };
        let Some(q) = inst.queue(&rt.reply_queue) else {
            continue;
        };
        while let Ok(Some(raw)) = inst.pop(q) {
            let token = crate::listen::reply_token(&raw);
            let Some(owner) = bound.owner_of(token) else {
                continue; // deadline beat the reply; consumed all the same
            };
            let outcome = crate::listen::parse_reply(&raw, &listeners[owner]);
            bound.answer(token, outcome);
        }
    }
}

fn root_source(config: &RootConfig) -> Result<String, String> {
    match &config.root.program {
        Some(drt_config::Program::Path(path)) => drt_platform::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display())),
        Some(drt_config::Program::Source(src)) => Ok(src.clone()),
        // The one place a pointer to dollup belongs: the user has
        // nothing to run, which is the only moment "where do programs come
        // from" is the question they actually have. A missing `relay`
        // block or a program that failed to parse are different problems,
        // and answering them with a package manager would be noise.
        None => Err(
            "the config names no program, and a deployment is config + a \
             program (SPEC.md §5).\n  \
             name one:  \"program\": {\"path\": \"...\"}\n  \
             or fetch one: dollup fetches and verifies them — \
             https://dollup.aloecraft.org"
                .to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// The relay's control plane, bridged into the deployment
// ---------------------------------------------------------------------------

/// The deployment's end of an in-process relay.
///
/// The relay is tokio; `drt start`'s drive loop is not (the http listener
/// is thread-per-connection over blocking sockets, so there is no runtime
/// here otherwise). So the relay runs on its own runtime on its own thread
/// and speaks to the loop through channels: events flow one way, admit
/// questions flow in and their answers back. Both ends are non-blocking,
/// which is what lets a synchronous drive loop arbitrate for an async
/// relay without either waiting on the other.
#[cfg(feature = "relay")]
pub struct RelayBridge {
    events: tokio::sync::mpsc::UnboundedReceiver<crate::relay::RelayEvent>,
    admits: tokio::sync::mpsc::UnboundedReceiver<crate::relay::Admit>,
    pending: HashMap<u64, tokio::sync::oneshot::Sender<bool>>,
    next_tok: u64,
    queue: String,
    reply_queue: String,
    /// Kept alive for the process's life: dropping it stops the relay.
    _runtime: std::thread::JoinHandle<()>,
}

#[cfg(feature = "relay")]
impl RelayBridge {
    /// Bind the relay and start serving it on its own runtime.
    pub fn start(config: drt_config::RelayConfig) -> Result<RelayBridge, String> {
        let (event_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let (admit_tx, admits) = tokio::sync::mpsc::unbounded_channel();
        let arbitrating = !config.reply_queue.is_empty();
        let control = crate::relay::Control {
            events: event_tx,
            // No reply queue named means the deployment opted out of being
            // asked, and the static key stays the only gate.
            admit: arbitrating.then_some(admit_tx),
            admit_timeout: Duration::from_millis(config.admit_timeout_ms),
        };
        let queue = config.queue.clone();
        let reply_queue = config.reply_queue.clone();
        let relay = crate::relay::Relay::with_control(config, control);
        let runtime = std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("drt start: the relay needs a runtime: {e}");
                    return;
                }
            };
            if let Err(e) = rt.block_on(crate::relay::serve(relay)) {
                eprintln!("drt start: relay stopped: {e}");
            }
        });
        Ok(RelayBridge {
            events,
            admits,
            pending: HashMap::new(),
            next_tok: 0,
            queue,
            reply_queue,
            _runtime: runtime,
        })
    }

    /// Push whatever the relay has said onto the root's queue, and ask
    /// whatever it needs asked. Non-blocking: anything not ready now is
    /// picked up next pass.
    fn deliver(&mut self, sw: &mut Deployment, root: InstanceId) {
        while let Ok(event) = self.events.try_recv() {
            let msg = encode(&event_value(&event));
            // A full or undeclared queue is the deployment's own sizing to
            // see. An event dropped here costs a panel an update; failing
            // the relay over it would cost a session.
            let _ = sw.push(root, &self.queue, &msg);
        }
        while let Ok(admit) = self.admits.try_recv() {
            self.next_tok += 1;
            let tok = self.next_tok;
            let msg = encode(&rmpv::Value::Map(vec![
                ("event".into(), "admit".into()),
                ("tok".into(), rmpv::Value::from(tok)),
                ("label".into(), admit.label.as_str().into()),
                ("leg".into(), admit.leg.into()),
            ]));
            if sw.push(root, &self.queue, &msg).is_ok() {
                self.pending.insert(tok, admit.reply);
            }
            // On a failed push the sender drops, which the relay reads as a
            // refusal — the safe direction when a deployment that asked to
            // arbitrate cannot be reached.
        }
    }

    /// Drain the deployment's answers and complete the questions they name.
    fn collect(&mut self, sw: &mut Deployment, root: InstanceId) {
        if self.reply_queue.is_empty() || self.pending.is_empty() {
            return;
        }
        let Some(inst) = sw.instance_mut(root) else {
            return;
        };
        let Some(q) = inst.queue(&self.reply_queue) else {
            return;
        };
        while let Ok(Some(raw)) = inst.pop(q) {
            let Ok(value) = rmpv::decode::read_value(&mut &raw[..]) else {
                continue;
            };
            let tok = field(&value, "tok").and_then(|v| v.as_u64());
            let ok = field(&value, "ok")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Some(reply) = tok.and_then(|t| self.pending.remove(&t)) {
                let _ = reply.send(ok);
            }
        }
    }
}

#[cfg(feature = "relay")]
fn field<'a>(map: &'a rmpv::Value, name: &str) -> Option<&'a rmpv::Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

#[cfg(feature = "relay")]
fn encode(value: &rmpv::Value) -> Vec<u8> {
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, value).expect("a relay event encodes");
    out
}

/// One event as the guest sees it. Flat maps with an `event` tag, the same
/// shape the http listener uses for a request — a program branches on a
/// field, never on a message's position or length.
#[cfg(feature = "relay")]
fn event_value(event: &crate::relay::RelayEvent) -> rmpv::Value {
    use crate::relay::RelayEvent;
    match event {
        RelayEvent::Parked { label } => rmpv::Value::Map(vec![
            ("event".into(), "parked".into()),
            ("label".into(), label.as_str().into()),
        ]),
        RelayEvent::Claimed { label, session } => rmpv::Value::Map(vec![
            ("event".into(), "claimed".into()),
            ("label".into(), label.as_str().into()),
            ("session".into(), rmpv::Value::from(*session)),
        ]),
        RelayEvent::Closed {
            label,
            session,
            bytes,
        } => rmpv::Value::Map(vec![
            ("event".into(), "closed".into()),
            ("label".into(), label.as_str().into()),
            ("session".into(), rmpv::Value::from(*session)),
            ("bytes".into(), rmpv::Value::from(*bytes)),
        ]),
    }
}

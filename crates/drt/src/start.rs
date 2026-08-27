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
use std::sync::Arc;
use std::time::{Duration, Instant};

use drt_caps::CapSet;
use drt_config::RootConfig;
use drt_connector::Dispatcher;
use drt_swarm::engine::diluvium_engine::DiluviumEngine;
use drt_swarm::engine::{Instance, QueueStatus, Step, WaitSet};
use drt_swarm::pump::PumpHost;
use drt_swarm::swarm::{Driven, Swarm, SwarmHost};
use drt_swarm::InstanceId;

/// How long the drive loop sleeps when a step moved nothing. Latency is
/// bounded by it; idle cost is one step per tick. Event-driven wakeups
/// arrive with the listeners, which is when a socket exists to select on.
const IDLE_TICK: Duration = Duration::from_millis(1);

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
        if !config.listeners.is_empty() {
            return Err(format!(
                "this config names {} listener(s), and this build does not carry \
                 `listen` — running the deployment while silently not binding the \
                 port the config asked for is the worst of both. Build with the \
                 `listen` feature, or remove the `listeners` block",
                config.listeners.len()
            ));
        }
        serve_swarm_only(config, dispatcher)
    }
}

/// The deployment with its listeners: the drive loop, with the ingress
/// channel doubling as the idle sleep so a request never waits on a tick.
#[cfg(feature = "listen")]
pub fn serve(
    config: &RootConfig,
    dispatcher: Dispatcher,
    bound: crate::listen::Bound,
) -> Result<(), String> {
    let (mut sw, root) = deployment(config, dispatcher)?;
    // Step before delivering, always: the first step runs the root to its
    // first park, so the request queue exists before the first request is
    // pushed at it. Delivering first would race the program's own
    // `queue.declare` and answer an early connection 503 for arriving
    // while the deployment was still clearing its throat.
    loop {
        let alive = sw.step();
        pump_replies(&mut sw, root, &bound);
        if let Some(residency) = config.residency {
            enforce_residency(&mut sw, root, residency.max_resident);
        }
        if alive == 0 {
            // The program chose to exit; ports serving a drained swarm
            // would answer 503 forever, and a supervisor should see the
            // exit instead.
            return Ok(());
        }
        let sleep = next_deadline(sw.host().inner())
            .map(|d| d.saturating_duration_since(Instant::now()).min(IDLE_TICK))
            .unwrap_or(IDLE_TICK);
        if let Some(ingress) = bound.next_within(sleep) {
            deliver(&mut sw, root, &bound, ingress);
        }
        while let Some(ingress) = bound.try_next() {
            deliver(&mut sw, root, &bound, ingress);
        }
    }
}

#[cfg(not(feature = "listen"))]
fn serve_swarm_only(config: &RootConfig, dispatcher: Dispatcher) -> Result<(), String> {
    let (mut sw, root) = deployment(config, dispatcher)?;
    loop {
        let alive = sw.step();
        if let Some(residency) = config.residency {
            enforce_residency(&mut sw, root, residency.max_resident);
        }
        if alive == 0 {
            return Ok(());
        }
        let sleep = next_deadline(sw.host().inner())
            .map(|d| d.saturating_duration_since(Instant::now()).min(IDLE_TICK))
            .unwrap_or(IDLE_TICK);
        if !sleep.is_zero() {
            std::thread::sleep(sleep);
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
fn deliver(
    sw: &mut Deployment,
    root: InstanceId,
    bound: &crate::listen::Bound,
    ingress: crate::listen::Ingress,
) {
    use crate::listen::Outcome;
    use drt_swarm::swarm::SwarmError;
    let queue = &bound.listeners[ingress.listener].queue;
    match sw.push(root, queue, &ingress.message) {
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
fn pump_replies(sw: &mut Deployment, root: InstanceId, bound: &crate::listen::Bound) {
    let mut drained: Vec<&str> = Vec::new();
    for rt in &bound.listeners {
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
            let outcome = crate::listen::parse_reply(&raw, &bound.listeners[owner]);
            bound.answer(token, outcome);
        }
    }
}

fn root_source(config: &RootConfig) -> Result<String, String> {
    match &config.root.program {
        Some(drt_config::Program::Path(path)) => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display())),
        Some(drt_config::Program::Source(src)) => Ok(src.clone()),
        None => Err(
            "the config names no program, and a deployment is config + a program \
             (SPEC.md §5): add `\"program\": {\"path\": \"...\"}`"
                .to_string(),
        ),
    }
}

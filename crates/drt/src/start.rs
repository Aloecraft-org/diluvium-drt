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
//! What `start` does not do yet, stated rather than implied:
//!
//! - **Listeners.** A config naming `listeners` is refused loudly, not
//!   accepted and ignored: an operator who wrote a listener block believes
//!   a port is being served.
//! - **The control endpoint.** `ps`/`pause`/`stop` reach a running
//!   deployment over the sshd subsystem (SPEC.md §13a); until that lands,
//!   the only controls are the terminal's.
//! - **Residency policy.** Everything stays resident, bounded by the
//!   instance table. Hibernation under memory pressure is the next piece
//!   of this file.

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
        let step = match park {
            None => inst.run(),
            Some((wait, deadline)) => {
                let fired = wait
                    .queues()
                    .iter()
                    .copied()
                    .find(|&q| inst.queue_info(q).map(|i| ready(&wait, i)).unwrap_or(false));
                match fired {
                    Some(q) => inst.resume(q),
                    // A queue firing beats the timeout when both are true on
                    // the same tick — the message was there first as far as
                    // anyone can observe, and answering it loses nothing.
                    None if deadline.is_some_and(|d| Instant::now() >= d) => inst.resume_timeout(),
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

/// The earliest pending deadline across every park, so the idle sleep never
/// overshoots a timeout an instance asked for.
fn next_deadline(host: &DeployHost) -> Option<Instant> {
    host.parked.values().filter_map(|p| p.deadline).min()
}

/// Run the deployment to completion. Returns when the swarm drains — every
/// instance exited — which for a server-shaped root program is never, and
/// foreground-forever is the contract.
pub fn start(config: &RootConfig, dispatcher: Dispatcher) -> Result<(), String> {
    if !config.listeners.is_empty() {
        return Err(format!(
            "this config names {} listener(s), and `drt start` does not serve \
             listeners yet — it would run the deployment while silently not \
             binding the port the config asked for. Remove the `listeners` \
             block to run swarm-only, or wait for the listen milestone",
            config.listeners.len()
        ));
    }
    let source = root_source(config)?;
    let engine = Arc::new(DiluviumEngine::new().map_err(|e| e.to_string())?);
    let mut sw = Swarm::new(engine, PumpHost::new(DeployHost::new(), dispatcher));
    sw.root(
        source.as_bytes(),
        crate::config::ceiling(config),
        config.root.budget,
    )
    .map_err(|e| format!("the root program would not start: {e}"))?;

    loop {
        let alive = sw.step();
        if alive == 0 {
            return Ok(());
        }
        // Sleep to the nearest armed deadline, capped at the tick. The
        // inner PumpHost answered hostcalls during the step, so anything
        // still parked is waiting on a message or a clock.
        let sleep = next_deadline(sw.host().inner())
            .map(|d| d.saturating_duration_since(Instant::now()).min(IDLE_TICK))
            .unwrap_or(IDLE_TICK);
        if !sleep.is_zero() {
            std::thread::sleep(sleep);
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

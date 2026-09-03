//! The drive loop as a state machine (doc/Wasm.md D6): [`Solo::tick`] says
//! what the host does next, and the host does it.
//!
//! `run`, `repl` and `start` used to own three loops that slept and blocked
//! on their own, which is fine natively and under wasmtime and impossible
//! on a browser thread, where nothing may block and there is no `main`.
//! Inverted once: a tick advances the instance as far as it can go without
//! waiting — runs it, answers its hostcalls, resumes it on a ready queue —
//! and returns [`Next`], which is the one thing it needs the host for: a
//! sleep of a stated length, a line of input, or the news that it is over.
//! The native loops are `match tick() { Sleep(d) => thread::sleep(d), .. }`;
//! a page calls `tick()` from `setTimeout` with the duration it was handed.
//! The sleep and the clock are the host's, which is `dvs.h`'s doctrine —
//! the host drives, there is no scheduler and no clock inside — restated
//! one level up.
//!
//! ## surface block
//!
//! - [`Next`], [`Outcome`]: what a tick asks of the host.
//! - [`Solo`]: one instance under the hostcall pump — `drt run`'s and
//!   `drt repl`'s. The deployment's driver is `start::DeployDriver`, beside
//!   the swarm it drives.
//! - [`POLL_TICK`]: the sleep while a connector's answer is in flight.

use std::sync::Arc;
use std::time::Duration;

use drt_caps::CapSet;
use drt_connector::Dispatcher;
use drt_platform::clock::Instant;
use drt_swarm::engine::{
    Engine, EngineError, Instance, LoadSpec, PushOutcome, QueueHandle, QueueStatus, Step, WaitSet,
};
use drt_swarm::pump::Pump;
use drt_swarm::InstanceId;

/// How long the host sleeps between ticks while a connector's answer is in
/// flight: the pump polls futures on the loop's cadence rather than on a
/// waker (see `drt_swarm::pump`), so this is the latency a deferred answer
/// pays, and the whole of the CPU an idle wait costs.
pub const POLL_TICK: Duration = Duration::from_millis(1);

/// The one instance a solo driver has, as the pump names it.
const SOLO: InstanceId = InstanceId(1);

/// What the host does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Nothing to do until this elapses: a park deadline, or the poll tick
    /// while an answer is in flight. Then tick again.
    Sleep(Duration),
    /// The instance is waiting on the queue the host reads input into.
    /// Feed a line, then tick again.
    Input,
    /// Parked on something nobody will ever push to. `for_space` when it
    /// is waiting for room in a queue nothing drains. The driver's caller
    /// words the refusal, since what "nobody" means is its to say.
    Stuck { for_space: bool },
    /// Ran to completion.
    Done(Outcome),
    /// A fault: the guest raised, or the engine refused. Sticky — every
    /// later tick repeats it.
    Failed(String),
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Exited,
    /// Finished, but the budget was caught as an ordinary error and stopped
    /// being enforced on the way (`src/dv.c:219` at the pin): the program
    /// ran past its bound. `drt run` refuses to exit zero for it.
    Exceeded,
}

enum State {
    Fresh,
    Parked {
        wait: WaitSet,
        deadline: Option<Instant>,
    },
    Done(Outcome),
    Failed(String),
}

/// One instance, driven: `drt run`'s program, or the REPL's evaluator.
pub struct Solo {
    inst: Box<dyn Instance>,
    caps: Arc<CapSet>,
    dispatcher: Arc<Dispatcher>,
    pump: Pump,
    state: State,
}

impl Solo {
    /// Load the program. Nothing runs until the first tick.
    pub fn load(
        engine: &dyn Engine,
        spec: LoadSpec<'_>,
        caps: Arc<CapSet>,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self, String> {
        let inst = engine.load(spec).map_err(|e| e.to_string())?;
        Ok(Solo {
            inst,
            caps,
            dispatcher,
            pump: Pump::new(),
            state: State::Fresh,
        })
    }

    /// Advance as far as possible without waiting, then say what is needed.
    ///
    /// `input` names the queue the host feeds lines into, when there is
    /// one: an instance parked on it is asking the host for a line, not for
    /// time. A name rather than a handle, looked up at each park, because
    /// the program declares its queues at runtime — before its first step
    /// there is nothing to look up. Hostcalls are answered before anything
    /// else on every tick — a request left unanswered is the one thing the
    /// host protocol forbids — and a queue that is ready is resumed before
    /// a deadline is consulted, because the message was there first as far
    /// as anyone can observe.
    pub fn tick(&mut self, input: Option<&str>) -> Next {
        loop {
            match &self.state {
                State::Fresh => {
                    let step = self.inst.run();
                    self.settle(step);
                }
                State::Done(outcome) => return Next::Done(*outcome),
                State::Failed(why) => return Next::Failed(why.clone()),
                State::Parked { wait, deadline } => {
                    let (wait, deadline) = (*wait, *deadline);
                    self.pump();
                    if let Some(fired) = self.ready(&wait) {
                        let step = self.inst.resume(fired);
                        self.settle(step);
                        continue;
                    }
                    let input = input.and_then(|name| self.inst.queue(name));
                    if input.is_some_and(|q| wait.queues().contains(&q)) {
                        return Next::Input;
                    }
                    let outstanding = self.pump.outstanding(SOLO) > 0;
                    if let Some(deadline) = deadline {
                        let now = Instant::now();
                        if now >= deadline {
                            let step = self.inst.resume_timeout();
                            self.settle(step);
                            continue;
                        }
                        let remaining = deadline - now;
                        return Next::Sleep(if outstanding {
                            remaining.min(POLL_TICK)
                        } else {
                            remaining
                        });
                    }
                    if outstanding {
                        return Next::Sleep(POLL_TICK);
                    }
                    return Next::Stuck {
                        for_space: wait.for_space,
                    };
                }
            }
        }
    }

    /// Answers outstanding from connectors: in flight, or waiting for room.
    pub fn in_flight(&self) -> usize {
        self.pump.outstanding(SOLO)
    }

    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    /// Look up a queue the program declared, for a host reading its output
    /// or feeding its input. Looked up per use, not once: the program
    /// declares its queues at runtime.
    pub fn queue(&mut self, name: &str) -> Option<QueueHandle> {
        self.inst.queue(name)
    }

    pub fn pop(&mut self, queue: QueueHandle) -> Result<Option<Vec<u8>>, String> {
        self.inst.pop(queue).map_err(|e| e.to_string())
    }

    pub fn push(&mut self, queue: QueueHandle, msgpack: &[u8]) -> Result<PushOutcome, String> {
        self.inst.push(queue, msgpack).map_err(|e| e.to_string())
    }

    // depth: the state transitions

    fn pump(&mut self) {
        self.pump.poll();
        self.pump
            .pump(SOLO, &self.caps, &self.dispatcher, &mut *self.inst);
    }

    fn settle(&mut self, step: Result<Step, EngineError>) {
        self.state = match step {
            Ok(Step::Parked(wait)) => State::Parked {
                deadline: wait.timeout.map(|t| Instant::now() + t),
                wait,
            },
            // A program that finished is not necessarily a program that
            // stayed inside its budget: instruction exhaustion arrives in
            // the guest as an ordinary Lua error, so a `pcall` catches it
            // and the rest of the run is unbounded. Reporting exit 0 for
            // that would make `drt run` the only place in DRT that hides
            // it; `drt start` already classifies a stop as `exceeded` from
            // the same flag. Not enforcement — the program has already run
            // — but the difference between a budget that was escaped and
            // one that was escaped silently.
            Ok(Step::Done) if self.inst.exceeded() => State::Done(Outcome::Exceeded),
            Ok(Step::Done) => State::Done(Outcome::Exited),
            Err(e) => State::Failed(e.to_string()),
        };
    }

    /// The first waited queue that is ready, in the order the program
    /// named them. Ready is not the same question for both kinds of park:
    /// a program waiting for a message wants a non-empty queue, one waiting
    /// for space wants a queue with room.
    fn ready(&mut self, wait: &WaitSet) -> Option<QueueHandle> {
        let ready = |info: QueueStatus| {
            if wait.for_space {
                info.len < info.capacity
            } else {
                info.len > 0
            }
        };
        wait.queues()
            .iter()
            .copied()
            .find(|&q| self.inst.queue_info(q).map(ready).unwrap_or(false))
    }
}

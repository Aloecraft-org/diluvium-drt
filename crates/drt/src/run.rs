//! `drt run`: config + one program is a complete deployment (SPEC.md §5).
//!
//! The single-instance drive loop, doing the host-protocol duties of
//! `doc/Host.md` for a population of one: run until parked, pump
//! `host/calls` through the dispatcher into `host/replies` (every drained
//! request answered), honour park timeouts on our clock, and call a park
//! that nothing will ever fire what it is — a deadlock the program can see.

use std::path::Path;
use std::sync::Arc;

use drt_caps::{CapSet, Grant};
use drt_connector::Dispatcher;
use drt_swarm::engine::{
    diluvium_engine::DiluviumEngine, Engine, EngineError, LoadSpec, ProgramBytes, Step,
};

/// The queue names `doc/Host.md` fixes so guests are portable between hosts.
const CALLS: &str = "host/calls";
const REPLIES: &str = "host/replies";

/// Where the budget-escape refusal below sends the reader. A pointer, not a
/// message: the failure needs more explanation than an error line can carry
/// and the explanation is not this file's to hold.
const BUDGET_ESCAPE_DOC: &str = "doc/Ask-0.5.0-Reply.md \u{a7}1.2";

pub fn run(
    program: &Path,
    dispatcher: &Dispatcher,
    caps: Vec<Grant>,
    budget: drt_config::Budget,
) -> Result<(), String> {
    let source = std::fs::read_to_string(program)
        .map_err(|e| format!("cannot read {}: {e}", program.display()))?;
    let name = program
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("program");

    let engine = DiluviumEngine::new().map_err(|e| e.to_string())?;
    let mut inst = engine
        .load(LoadSpec {
            program: ProgramBytes::Source(&source),
            name,
            budget,
            unsafe_stdlib: false,
        })
        .map_err(|e| e.to_string())?;

    // The ceiling the config set (or the wide local default when there is
    // no config). What is actually reachable is the intersection with what
    // this build wires — an unwired family answers `denied` either way.
    let caps: Arc<CapSet> = CapSet::root(caps);

    let mut step = inst.run().map_err(guest_error)?;
    loop {
        let wait = match step {
            // A program that finished is not necessarily a program that
            // stayed inside its budget. Instruction exhaustion arrives in
            // the guest as an ordinary Lua error, so a `pcall` catches it
            // -- and at the pin, the hook clears itself before raising
            // (`src/dv.c:219`), so nothing re-arms and the rest of the run
            // is unbounded. Reporting exit 0 for that would make `drt run`
            // the only place in DRT that hides it: `drt start` already
            // classifies a stop as `exceeded` from the same flag
            // (`drt-swarm/src/swarm.rs:829`), and a supervisor reads it.
            //
            // This is not enforcement and does not pretend to be -- the
            // program has already run. It is the difference between a
            // budget that was escaped and a budget that was escaped
            // silently. The enforcement fix is upstream; the brief is
            // doc/Ask-0.5.0-Reply.md §1.2.
            Step::Done if inst.exceeded() => {
                return Err(format!(
                    "the program exhausted its instruction budget and then \
                     continued: the budget was caught as an ordinary error and \
                     stopped being enforced. Exit status reports it because \
                     nothing else can ({BUDGET_ESCAPE_DOC})."
                ))
            }
            Step::Done => return finish(dispatcher),
            Step::Parked(wait) => wait,
        };

        // Looked up per park, not once: the guest declares these queues at
        // runtime (through the `host` library or by hand), so they may not
        // exist until after the first step.
        let calls = inst.queue(CALLS);
        let replies = inst.queue(REPLIES);

        // The hostcall pump: drain every pending request and answer each one.
        let mut answered = false;
        if let (Some(cq), Some(rq)) = (calls, replies) {
            while let Some(raw) = inst.pop(cq).map_err(guest_error)? {
                let reply = pollster::block_on(dispatcher.dispatch(&caps, &raw));
                let bytes = drt_hostcall::to_bytes(&reply).map_err(|e| e.to_string())?;
                if !inst.push(rq, &bytes).map_err(guest_error)?.is_accepted() {
                    return Err(format!(
                        "a reply had nowhere to land: '{REPLIES}' is full or disabled; \
                         size the reply queue for the requests kept outstanding"
                    ));
                }
                answered = true;
            }
        }

        step = if answered && replies.is_some_and(|rq| wait.queues().contains(&rq)) {
            inst.resume(replies.unwrap()).map_err(guest_error)?
        } else if let Some(timeout) = wait.timeout {
            // We own the clock (the instance has none): honour the ask.
            std::thread::sleep(timeout);
            inst.resume_timeout().map_err(guest_error)?
        } else if wait.for_space {
            return Err(
                "the program is parked waiting for space in a queue this run never drains"
                    .to_string(),
            );
        } else {
            return Err(
                "the program is parked waiting on queues nothing in `drt run` will push to \
                 (a served deployment or a swarm parent would); it will never wake"
                    .to_string(),
            );
        };
    }
}

/// The run ended; ask the connectors whether it ended cleanly.
///
/// A connector that holds state across hostcalls can lose work at teardown
/// without any call having failed -- `sql` is the one that does, and its
/// answer names the databases and what happened to them. The program is
/// already over, so this changes nothing about what ran; it decides whether
/// the process reports success for it.
///
/// Reporting it as an error rather than a warning is the point. A warning
/// on stderr is a thing a supervisor does not act on.
///
/// `drt start` calls this too, at the two places its swarm drains, because
/// a long-running deployment is the shape where an abandoned transaction is
/// most likely and least visible.
pub fn finish(dispatcher: &Dispatcher) -> Result<(), String> {
    let lost = dispatcher.finish();
    if lost.is_empty() {
        return Ok(());
    }
    Err(lost.join("; "))
}

fn guest_error(e: EngineError) -> String {
    e.to_string()
}

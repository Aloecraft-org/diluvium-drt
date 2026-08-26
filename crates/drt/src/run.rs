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

pub fn run(program: &Path, dispatcher: &Dispatcher) -> Result<(), String> {
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
            budget: Default::default(),
        })
        .map_err(|e| e.to_string())?;

    // A local run is the operator running their own program: the ceiling is
    // wide (`host:*`), and what is actually reachable is what this build
    // wires — an unwired family answers `denied`, exactly as deployed.
    let caps: Arc<CapSet> = CapSet::root(vec![Grant::grant("host:*")]);

    let mut step = inst.run().map_err(guest_error)?;
    loop {
        let wait = match step {
            Step::Done => return Ok(()),
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

        step = if answered && replies.is_some_and(|rq| wait.queues.contains(&rq)) {
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

fn guest_error(e: EngineError) -> String {
    e.to_string()
}

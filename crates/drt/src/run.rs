//! `drt run`: config + one program is a complete deployment (SPEC.md §5).
//!
//! The single-instance drive loop, doing the host-protocol duties of
//! `doc/Host.md` for a population of one: run until parked, pump
//! `host/calls` through the dispatcher into `host/replies` (every drained
//! request answered), honour park timeouts on our clock, and call a park
//! that nothing will ever fire what it is — a deadlock the program can see.
//!
//! The loop itself is [`crate::drive::Solo`]'s; this file is the native
//! host around it — the one that may sleep — and the wording of what a
//! stuck program is told.

use std::path::Path;
use std::sync::Arc;

use drt_caps::{CapSet, Grant};
use drt_connector::Dispatcher;
use drt_swarm::engine::{diluvium_engine::DiluviumEngine, LoadSpec, ProgramBytes};

use crate::drive::{Next, Outcome, Solo};

/// Where the budget-escape refusal below sends the reader. A pointer, not a
/// message: the failure needs more explanation than an error line can carry
/// and the explanation is not this file's to hold.
const BUDGET_ESCAPE_DOC: &str = "doc/Ask-0.5.0-Reply.md \u{a7}1.2";

/// Load the program under the ceiling. Nothing runs until the first tick.
pub fn prepare(
    program: &Path,
    dispatcher: Arc<Dispatcher>,
    caps: Vec<Grant>,
    budget: drt_config::Budget,
) -> Result<Solo, String> {
    let source = drt_platform::fs::read_to_string(program)
        .map_err(|e| format!("cannot read {}: {e}", program.display()))?;
    let name = program
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("program");

    let engine = DiluviumEngine::new().map_err(|e| e.to_string())?;
    // The ceiling the config set (or the wide local default when there is
    // no config). What is actually reachable is the intersection with what
    // this build wires — an unwired family answers `denied` either way.
    let caps: Arc<CapSet> = CapSet::root(caps);
    Solo::load(
        &engine,
        LoadSpec {
            program: ProgramBytes::Source(&source),
            name,
            budget,
            unsafe_stdlib: false,
        },
        caps,
        dispatcher,
    )
}

/// What a tick's answer means for the run: `None` while the host should
/// sleep and tick again, `Some` when the run is over — cleanly, or with
/// the sentence `drt run` says for it. The wording lives here so a
/// terminal in a page says what a shell says.
pub fn settle(next: &Next, dispatcher: &Dispatcher) -> Option<Result<(), String>> {
    Some(match next {
        Next::Sleep(_) => return None,
        Next::Done(Outcome::Exited) => finish(dispatcher),
        Next::Done(Outcome::Exceeded) => Err(format!(
            "the program exhausted its instruction budget and then \
             continued: the budget was caught as an ordinary error and \
             stopped being enforced. Exit status reports it because \
             nothing else can ({BUDGET_ESCAPE_DOC})."
        )),
        Next::Failed(why) => Err(why.clone()),
        Next::Stuck { for_space: true } => Err(
            "the program is parked waiting for space in a queue this run never drains".to_string(),
        ),
        Next::Stuck { for_space: false } => Err(
            "the program is parked waiting on queues nothing in `drt run` will push to \
             (a served deployment or a swarm parent would); it will never wake"
                .to_string(),
        ),
        // `run` feeds no input queue, so a tick never asks for one.
        Next::Input => {
            Err("the program is waiting for input, and `drt run` has none to give".into())
        }
    })
}

/// The native loop: [`prepare`], then tick and sleep until [`settle`] says
/// it is over.
pub fn run(
    program: &Path,
    dispatcher: Arc<Dispatcher>,
    caps: Vec<Grant>,
    budget: drt_config::Budget,
) -> Result<(), String> {
    let mut solo = prepare(program, dispatcher.clone(), caps, budget)?;
    loop {
        let next = solo.tick(None);
        // We own the clock (the instance has none): honour the ask.
        if let Next::Sleep(how_long) = next {
            std::thread::sleep(how_long);
            continue;
        }
        if let Some(ended) = settle(&next, &dispatcher) {
            return ended;
        }
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

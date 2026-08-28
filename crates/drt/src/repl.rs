//! `drt repl`: a REPL is an instance, not a mode.
//!
//! The evaluation lives in a guest program ([`repl.dlua`]) that holds one
//! environment across lines; this file is only the terminal half — read a
//! line, push it on `repl/in`, drive the instance, print what comes back
//! on `repl/out`. Hostcalls are pumped exactly as `drt run` pumps them, so
//! a REPL reaches every connector the config wired and nothing it did not.
//!
//! The split is the point. "Line in, text out" is a contract a terminal
//! satisfies today and an xterm.js in a browser satisfies unchanged, over
//! the same two queues — so there is one REPL with one behavior, not a
//! desktop one and a web one that drift.
//!
//! What this is *not* is `repl --attach`: reaching into a **running**
//! deployment is the control endpoint's job (SPEC.md §13a), and it lands
//! with sshd. This one starts a fresh instance of its own.

use std::io::{BufRead, Write};
use std::sync::Arc;

use drt_caps::{CapSet, Grant};
use drt_connector::Dispatcher;
use drt_swarm::engine::{
    diluvium_engine::DiluviumEngine, Engine, EngineError, LoadSpec, ProgramBytes, Step,
};

const PROGRAM: &str = include_str!("repl.dlua");
const IN: &str = "repl/in";
const OUT: &str = "repl/out";
const CALLS: &str = "host/calls";
const REPLIES: &str = "host/replies";

pub fn repl(
    dispatcher: &Dispatcher,
    caps: Vec<Grant>,
    budget: drt_config::Budget,
) -> Result<(), String> {
    let engine = DiluviumEngine::new().map_err(|e| e.to_string())?;
    let mut inst = engine
        .load(LoadSpec {
            program: ProgramBytes::Source(PROGRAM),
            name: "repl",
            budget,
            // The REPL's own source is ours, not the user's, and the lines
            // it evaluates run under the same stdlib every other guest
            // gets. A REPL is not a way around the sandbox.
            unsafe_stdlib: false,
        })
        .map_err(|e| e.to_string())?;

    let caps: Arc<CapSet> = CapSet::root(caps);
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    // Text carried over from a line that ended mid-expression.
    let mut pending = String::new();

    eprintln!("drt repl — ^D to leave");
    let mut step = inst.run().map_err(guest_error)?;
    loop {
        let wait = match step {
            Step::Done => return Ok(()),
            Step::Parked(wait) => wait,
        };

        // Answer every drained hostcall before anything else, exactly as
        // `drt run` does: a request left unanswered is the one thing the
        // host protocol forbids.
        let calls = inst.queue(CALLS);
        let replies = inst.queue(REPLIES);
        let mut answered = false;
        if let (Some(cq), Some(rq)) = (calls, replies) {
            while let Some(raw) = inst.pop(cq).map_err(guest_error)? {
                let reply = pollster::block_on(dispatcher.dispatch(&caps, &raw));
                let bytes = drt_hostcall::to_bytes(&reply).map_err(|e| e.to_string())?;
                if !inst.push(rq, &bytes).map_err(guest_error)?.is_accepted() {
                    return Err(format!("a reply had nowhere to land: '{REPLIES}' is full"));
                }
                answered = true;
            }
        }
        if answered && replies.is_some_and(|rq| wait.queues().contains(&rq)) {
            step = inst.resume(replies.unwrap()).map_err(guest_error)?;
            continue;
        }

        // Whatever the last line produced, before asking for the next one.
        let mut want_more = false;
        if let Some(oq) = inst.queue(OUT) {
            while let Some(raw) = inst.pop(oq).map_err(guest_error)? {
                want_more = print_answer(&raw) || want_more;
            }
        }
        if !want_more {
            pending.clear();
        }

        let Some(inq) = inst.queue(IN) else {
            return Err("the repl program never declared its input queue".into());
        };
        if !wait.queues().contains(&inq) {
            // Not waiting for us: a timeout is ours to honour, anything
            // else is a park this terminal can never wake.
            if let Some(timeout) = wait.timeout {
                std::thread::sleep(timeout);
                step = inst.resume_timeout().map_err(guest_error)?;
                continue;
            }
            return Err("the repl parked on something the terminal cannot wake".into());
        }

        let Some(line) = prompt(&mut lines, want_more)? else {
            eprintln!();
            return Ok(());
        };
        if !want_more && line.trim().is_empty() {
            continue;
        }
        if !pending.is_empty() {
            pending.push('\n');
        }
        pending.push_str(&line);

        let mut msg = Vec::new();
        rmpv::encode::write_value(&mut msg, &rmpv::Value::from(pending.as_str()))
            .map_err(|e| e.to_string())?;
        if !inst.push(inq, &msg).map_err(guest_error)?.is_accepted() {
            return Err("the repl's input queue is full".into());
        }
        step = inst.resume(inq).map_err(guest_error)?;
    }
}

/// Print one answer. Returns true when the line was unfinished rather than
/// wrong, which is the host's cue to keep reading instead of reporting.
fn print_answer(raw: &[u8]) -> bool {
    let Ok(value) = rmpv::decode::read_value(&mut &raw[..]) else {
        return false;
    };
    let get = |name: &str| {
        value
            .as_map()
            .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some(name)))
            .map(|(_, v)| v.clone())
    };
    let text = get("text").and_then(|v| v.as_str().map(str::to_string));
    if get("more").and_then(|v| v.as_bool()).unwrap_or(false) {
        return true;
    }
    match (get("ok").and_then(|v| v.as_bool()).unwrap_or(false), text) {
        (true, Some(t)) => println!("{t}"),
        (true, None) => {}
        (false, Some(t)) => eprintln!("{t}"),
        (false, None) => eprintln!("the repl answered nothing"),
    }
    false
}

fn prompt(
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    continuing: bool,
) -> Result<Option<String>, String> {
    // stderr, so `drt repl < script.dlua > out` gives clean output: the
    // prompt is furniture, the answers are the product.
    eprint!("{}", if continuing { ">> " } else { "dv> " });
    std::io::stderr().flush().map_err(|e| e.to_string())?;
    match lines.next() {
        Some(line) => line.map(Some).map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

fn guest_error(e: EngineError) -> String {
    e.to_string()
}

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
//! desktop one and a web one that drift. [`Repl`] is the half both share:
//! it never reads a terminal and never sleeps; its `tick` says when a line
//! is wanted (doc/Wasm.md D6, D8), and [`repl`] is the native loop that
//! reads one from stdin.
//!
//! What this is *not* is `repl --attach`: reaching into a **running**
//! deployment is the control endpoint's job (SPEC.md §13a), and it lands
//! with sshd. This one starts a fresh instance of its own.

use std::io::{BufRead, Write};
use std::sync::Arc;

use drt_caps::{CapSet, Grant};
use drt_connector::Dispatcher;
use drt_platform::stdio;
use drt_swarm::engine::{diluvium_engine::DiluviumEngine, LoadSpec, ProgramBytes};

use crate::drive::{Next, Solo};

const PROGRAM: &str = include_str!("repl.dlua");
const IN: &str = "repl/in";
const OUT: &str = "repl/out";

/// The REPL instance, driven by whoever has the terminal.
pub struct Repl {
    solo: Solo,
    /// Text carried over from a line that ended mid-expression.
    pending: String,
    /// The last answer said the line was unfinished: ask for more.
    want_more: bool,
}

impl Repl {
    pub fn new(
        dispatcher: Arc<Dispatcher>,
        caps: Vec<Grant>,
        budget: drt_config::Budget,
    ) -> Result<Self, String> {
        let engine = DiluviumEngine::new().map_err(|e| e.to_string())?;
        let caps: Arc<CapSet> = CapSet::root(caps);
        let solo = Solo::load(
            &engine,
            LoadSpec {
                program: ProgramBytes::Source(PROGRAM),
                name: "repl",
                budget,
                // The REPL's own source is ours, not the user's, and the
                // lines it evaluates run under the same stdlib every other
                // guest gets. A REPL is not a way around the sandbox.
                unsafe_stdlib: false,
            },
            caps,
            dispatcher,
        )?;
        Ok(Repl {
            solo,
            pending: String::new(),
            want_more: false,
        })
    }

    /// Advance, print whatever the last line produced, and say what is
    /// needed next. [`Next::Input`] means a line; whether it continues an
    /// unfinished one is [`Repl::continuing`].
    pub fn tick(&mut self) -> Next {
        let next = self.solo.tick(Some(IN));
        // Whatever the last line produced, before asking for the next one.
        self.print_answers();
        match next {
            // Not waiting for us: a park this terminal can never wake.
            Next::Stuck { .. } => {
                Next::Failed("the repl parked on something the terminal cannot wake".into())
            }
            other => other,
        }
    }

    /// Whether the next line continues an unfinished one.
    pub fn continuing(&self) -> bool {
        self.want_more
    }

    /// Feed one line. Returns whether anything was sent: a blank line
    /// outside a continuation is nothing to evaluate and is not sent.
    pub fn feed(&mut self, line: &str) -> Result<bool, String> {
        if !self.want_more && line.trim().is_empty() {
            return Ok(false);
        }
        if !self.pending.is_empty() {
            self.pending.push('\n');
        }
        self.pending.push_str(line);
        let Some(input) = self.solo.queue(IN) else {
            return Err("the repl program never declared its input queue".into());
        };
        let mut msg = Vec::new();
        rmpv::encode::write_value(&mut msg, &rmpv::Value::from(self.pending.as_str()))
            .map_err(|e| e.to_string())?;
        if !self.solo.push(input, &msg)?.is_accepted() {
            return Err("the repl's input queue is full".into());
        }
        Ok(true)
    }

    pub fn dispatcher(&self) -> &Dispatcher {
        self.solo.dispatcher()
    }

    // depth: answers

    fn print_answers(&mut self) {
        let Some(out) = self.solo.queue(OUT) else {
            return;
        };
        let mut want_more = false;
        while let Ok(Some(raw)) = self.solo.pop(out) {
            want_more = print_answer(&raw) || want_more;
        }
        self.want_more = want_more;
        if !want_more {
            self.pending.clear();
        }
    }
}

/// The native terminal: stdin lines in, answers on stdout, everything else
/// on stderr — so `drt repl < script.dlua > out` gives clean output, the
/// prompt being furniture and the answers the product.
pub fn repl(
    dispatcher: Arc<Dispatcher>,
    caps: Vec<Grant>,
    budget: drt_config::Budget,
) -> Result<(), String> {
    let mut repl = Repl::new(dispatcher, caps, budget)?;
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();

    eprintln!("drt repl — ^D to leave");
    loop {
        match repl.tick() {
            Next::Sleep(how_long) => std::thread::sleep(how_long),
            Next::Input => {
                let Some(line) = prompt(&mut lines, repl.continuing())? else {
                    eprintln!();
                    return Ok(());
                };
                repl.feed(&line)?;
            }
            Next::Done(_) => return Ok(()),
            Next::Failed(why) => return Err(why),
            Next::Stuck { .. } => {
                return Err("the repl parked on something the terminal cannot wake".into())
            }
        }
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
    let ok = get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    match (ok, text) {
        (true, Some(t)) => {
            let _ = writeln!(stdio::stdout(), "{t}");
        }
        (true, None) => {}
        (false, Some(t)) => {
            let _ = writeln!(stdio::stderr(), "{t}");
        }
        (false, None) => {
            let _ = writeln!(stdio::stderr(), "the repl answered nothing");
        }
    }
    false
}

fn prompt(
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    continuing: bool,
) -> Result<Option<String>, String> {
    eprint!("{}", if continuing { ">> " } else { "dv> " });
    std::io::stderr().flush().map_err(|e| e.to_string())?;
    match lines.next() {
        Some(line) => line.map(Some).map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

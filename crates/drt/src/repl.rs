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
//!
//! ## surface block
//!
//! - [`Repl`]: the half every host shares — `tick`, `feed`, `continuing`,
//!   [`Repl::names`].
//! - [`repl`]: the native loop. On a tty it is [`edited`] (ego-cli:
//!   history, word motions, undo, Tab); through a pipe it is [`piped`],
//!   whose prompts go to stderr so a redirect stays clean.
//! - [`PROGRAM`], [`IN`], [`OUT`]: the guest and the two queues.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

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
    /// The names the guest last said were in scope, for Tab. Shared,
    /// because the editor's completer holds it and is asked during a
    /// keystroke while this side refreshes it between lines.
    names: Arc<Mutex<Vec<String>>>,
    /// A line has run since the snapshot was taken, so it is worth
    /// asking again before the next prompt.
    names_stale: bool,
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
            names: Arc::new(Mutex::new(Vec::new())),
            names_stale: true,
        })
    }

    /// The names the guest last reported in scope. Empty until the first
    /// answer arrives, which is one tick after the instance starts.
    pub fn names(&self) -> Arc<Mutex<Vec<String>>> {
        self.names.clone()
    }

    /// Advance, print whatever the last line produced, and say what is
    /// needed next. [`Next::Input`] means a line; whether it continues an
    /// unfinished one is [`Repl::continuing`].
    pub fn tick(&mut self) -> Next {
        // Twice at most: once to run whatever was fed, and once more to
        // let the guest answer what is in scope, so the prompt that
        // follows has a snapshot for Tab rather than the last line's.
        for _ in 0..2 {
            let next = self.solo.tick(Some(IN));
            // Whatever the last line produced, before asking for the next.
            self.print_answers();
            match next {
                // Not waiting for us: a park this terminal can never wake.
                Next::Stuck { .. } => {
                    return Next::Failed(
                        "the repl parked on something the terminal cannot wake".into(),
                    )
                }
                Next::Input if self.names_stale => {
                    self.names_stale = false;
                    self.ask_for_names();
                }
                other => return other,
            }
        }
        Next::Input
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
        self.names_stale = true;
        Ok(true)
    }

    /// Throw away an unfinished line: what `^C` means at a prompt. The
    /// instance is untouched — only the text this side was carrying.
    pub fn abandon(&mut self) {
        self.pending.clear();
        self.want_more = false;
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
        let mut answered = false;
        while let Ok(Some(raw)) = self.solo.pop(out) {
            match Answer::of(&raw) {
                // Consumed, never printed: this one is for Tab.
                Answer::Names(names) => {
                    *self.names.lock().unwrap_or_else(|e| e.into_inner()) = names
                }
                Answer::Text { more } => {
                    answered = true;
                    want_more = more || want_more;
                }
            }
        }
        if answered {
            self.want_more = want_more;
            if !want_more {
                self.pending.clear();
            }
        }
    }

    /// Ask the guest what is in scope. Its answer lands on a later tick
    /// and is consumed by [`Repl::print_answers`]; nothing waits for it,
    /// because a Tab that has not been prepared for offers nothing rather
    /// than stalling (doc/Wasm.md §5).
    fn ask_for_names(&mut self) {
        let Some(input) = self.solo.queue(IN) else {
            return;
        };
        let ask = rmpv::Value::Map(vec![(
            rmpv::Value::from("complete"),
            rmpv::Value::Boolean(true),
        )]);
        let mut msg = Vec::new();
        if rmpv::encode::write_value(&mut msg, &ask).is_ok() {
            let _ = self.solo.push(input, &msg);
        }
    }
}

/// The native terminal. Two shapes, and the terminal decides which:
///
/// - a tty gets [`edited`] — history, word motions, undo and Tab, from
///   the one line editor every target uses (doc/Wasm.md D8);
/// - anything else gets [`piped`], where answers go to stdout and every
///   other byte to stderr, so `drt repl < script.dlua > out` gives clean
///   output with the prompt as furniture.
///
/// A pipe is never edited, so the second shape is not a fallback for a
/// missing feature: it is what a pipe means. Without `cli` compiled in,
/// a tty gets it too.
pub fn repl(
    dispatcher: Arc<Dispatcher>,
    caps: Vec<Grant>,
    budget: drt_config::Budget,
) -> Result<(), String> {
    let repl = Repl::new(dispatcher, caps, budget)?;
    // Not in a page: see `edited` for why the browser has no tty to ask
    // about, and `drt-web`'s `editor` module for what it has instead.
    #[cfg(all(
        feature = "cli",
        not(all(target_arch = "wasm32", target_os = "unknown"))
    ))]
    {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            return edited(repl);
        }
    }
    piped(repl)
}

/// The pipe: one line in, one answer out, prompts on stderr.
fn piped(mut repl: Repl) -> Result<(), String> {
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

// depth: the edited path
//
// One line editor, over the terminal this target has (doc/Wasm.md D8).
// The seam is `Next::Input`, and it is the only place a host may wait:
// the driver returns it exactly when the instance is parked on the input
// queue with nothing left to run, so blocking there is the case D6
// identifies rather than the one it forbids.

/// Tab's candidates: the names the guest last said were in scope,
/// filtered by the dotted token under the cursor.
///
/// A snapshot rather than a question asked at the keystroke — `Completer`
/// is synchronous, and reaching the guest is a round trip through the
/// driver. `Repl` refreshes it after every line, which is when what is in
/// scope can have changed.
#[cfg(feature = "cli")]
pub struct Names {
    names: Arc<Mutex<Vec<String>>>,
}

#[cfg(feature = "cli")]
impl Names {
    /// Complete from `names`, which the caller refreshes.
    ///
    /// Public so a host that drives the editor itself -- `drt-web`, whose
    /// tick loop lives in the page -- completes by the same rule as a tty
    /// rather than a second one that drifts.
    pub fn new(names: Arc<Mutex<Vec<String>>>) -> Self {
        Names { names }
    }
}

#[cfg(feature = "cli")]
impl ego_cli::extend::Completer for Names {
    fn complete(&self, line: &str, cursor: usize) -> ego_cli::extend::Completion {
        // The token is the run of identifier characters and dots left of
        // the cursor, so `host.ti` completes and so does the same thing
        // inside `print(host.ti` — a paren is not one of them.
        let start = line[..cursor]
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '.'))
            .map(|(at, c)| at + c.len_utf8())
            .unwrap_or(0);
        let token = &line[start..cursor];
        let names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        let candidates: Vec<String> = names
            .iter()
            .filter(|name| name.starts_with(token))
            .cloned()
            .collect();
        ego_cli::extend::Completion::new(start..cursor, candidates)
    }
}

/// Bind a [`Repl`] to a line editor: set the completer, and hand back the
/// session to drive with [`edit`].
///
/// Public because the terminal is the argument: a test drives this over
/// `ego_cli::term::mem::MemTerminal` and gets the same loop a tty gets,
/// which is the only way to hold "one editor" to anything.
#[cfg(feature = "cli")]
pub fn editor<T: ego_cli::term::Terminal>(repl: &Repl, terminal: T) -> ego_cli::Session<T> {
    let mut session = ego_cli::Session::new(terminal);
    session.set_completer(Names {
        names: repl.names(),
    });
    session
}

/// The loop, over whatever terminal the session holds.
#[cfg(feature = "cli")]
pub async fn edit<T: ego_cli::term::Terminal>(
    repl: &mut Repl,
    session: &mut ego_cli::Session<T>,
) -> Result<(), String> {
    use ego_cli::ReadOutcome;
    loop {
        match repl.tick() {
            Next::Sleep(how_long) => std::thread::sleep(how_long),
            Next::Input => {
                session.set_prompt(if repl.continuing() { ">> " } else { "dv> " });
                match session.read_line().await {
                    Ok(ReadOutcome::Line(line)) => {
                        repl.feed(&line)?;
                    }
                    // ^C: the line is gone, and so is anything it was
                    // continuing. The session is still good.
                    Ok(ReadOutcome::Interrupted) => repl.abandon(),
                    Ok(ReadOutcome::Eof) => return Ok(()),
                    Err(e) => return Err(format!("the terminal: {e}")),
                }
            }
            Next::Done(_) => return Ok(()),
            Next::Failed(why) => return Err(why),
            Next::Stuck { .. } => {
                return Err("the repl parked on something the terminal cannot wake".into())
            }
        }
    }
}

/// The tty: the same REPL, with an editor in front of it.
///
/// Not compiled for the browser, which has no terminal to *open*:
/// `ego_cli::term::platform()` exists wherever a process can ask its
/// target for one, and a page instead owns an xterm.js object and hands
/// it over. `drt-web`'s `editor` module takes it from there, over the
/// same [`editor`] and the same [`Names`] this builds.
#[cfg(all(
    feature = "cli",
    not(all(target_arch = "wasm32", target_os = "unknown"))
))]
fn edited(mut repl: Repl) -> Result<(), String> {
    let terminal = ego_cli::term::platform().map_err(|e| format!("no terminal: {e}"))?;
    let mut session = editor(&repl, terminal);

    // `block_on` and nothing else: with ego-cli's `runtime` feature off,
    // `platform()` is `BlockingNative`, which blocks on the terminal itself
    // and never returns `Pending` -- so this future runs to completion on
    // the first poll. No reactor to build, and nothing to leak past the
    // tokio teardown bug `cli.rs` still works around for the relay.
    eprintln!("drt repl — ^D to leave");
    futures_executor::block_on(edit(&mut repl, &mut session))
}

/// What came back on `repl/out`: something to show, or the names Tab
/// completes from.
enum Answer {
    /// Already printed. `more` means the line was unfinished rather than
    /// wrong, which is the host's cue to keep reading instead of reporting.
    Text {
        more: bool,
    },
    Names(Vec<String>),
}

impl Answer {
    /// Classify one answer, printing it if that is what it is.
    fn of(raw: &[u8]) -> Answer {
        let Ok(value) = rmpv::decode::read_value(&mut &raw[..]) else {
            return Answer::Text { more: false };
        };
        let get = |name: &str| {
            value
                .as_map()
                .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some(name)))
                .map(|(_, v)| v.clone())
        };
        if let Some(rmpv::Value::Array(names)) = get("names") {
            return Answer::Names(
                names
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
            );
        }
        let text = get("text").and_then(|v| v.as_str().map(str::to_string));
        if get("more").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Answer::Text { more: true };
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
        Answer::Text { more: false }
    }
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

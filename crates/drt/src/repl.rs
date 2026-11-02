//! `drt repl`: a REPL is an instance, not a mode.
//!
//! The evaluation lives in a guest program ([`repl.dlua`]) that holds one
//! environment across lines; this file is only the terminal half — read a
//! line, push it on `repl/in`, drive the instance, print what comes back
//! on `repl/out`. Hostcalls are pumped exactly as `drt run` pumps them, so
//! a REPL reaches every connector the config wired and nothing it did not.
//!
//! The instance is the root of a swarm of its own (`crate::start`'s
//! [`Deployment`], the one `drt start` drives), so that a line can
//! `host.spawn` and what it spawns runs beside the REPL under the same
//! ceiling and budget. The terminal drives the whole swarm: a child gets
//! its slices while a line evaluates and between lines, and a host whose
//! editor does not block (a page) keeps ticking while [`Repl::busy`]
//! holds, so a child runs while the next line is typed. With no config
//! the root holds `lifecycle` (`crate::config::repl_ceiling`); a config
//! that names its ceiling decides for itself.
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
//!   [`Repl::names`], [`Repl::variables`], [`Repl::busy`], and
//!   [`Repl::deployment_mut`] for a host that shows the swarm.
//! - [`repl`]: the native loop. On a tty it is [`edited`] (ego-cli:
//!   history, word motions, undo, Tab); through a pipe it is [`piped`],
//!   whose prompts go to stderr so a redirect stays clean.
//! - [`PROGRAM`], [`IN`], [`OUT`]: the guest and the two queues.
//!   [`STEPS_PER_TICK`]: how long a tick keeps stepping a running child
//!   before answering the terminal.
//! - Fan-out: [`meta`], the lines the host answers instead of the guest:
//!   `:help`, `:quit`, `:clear`, the control endpoint's orders, `:ssh`
//!   (the config's `host:ssh/shell`) and `:ssh <target> …` (`drt ssh`, on
//!   this terminal). [`META_HELP`] lists them.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drt_caps::Grant;
use drt_connector::Dispatcher;
use drt_platform::clock::Instant;
use drt_platform::stdio;
use drt_swarm::engine::diluvium_engine::DiluviumEngine;
use drt_swarm::engine::QueueHandle;
use drt_swarm::pump::PumpHost;
use drt_swarm::swarm::Swarm;
use drt_swarm::InstanceId;

use crate::drive::Next;
use crate::start::{DeployHost, Deployment, IDLE_TICK};

const PROGRAM: &str = include_str!("repl.dlua");
const IN: &str = "repl/in";
const OUT: &str = "repl/out";
/// How many swarm steps one tick takes at most before answering the
/// terminal, when something the REPL spawned keeps running.
const STEPS_PER_TICK: usize = 64;

/// The REPL instance, driven by whoever has the terminal.
pub struct Repl {
    /// The swarm the REPL is the root of. A swarm rather than a lone
    /// instance so that a line can `host.spawn`, and what it spawns runs
    /// here, under the REPL's own ceiling, stepped between lines.
    sw: Deployment,
    root: InstanceId,
    /// Text carried over from a line that ended mid-expression.
    pending: String,
    /// The last answer said the line was unfinished: ask for more.
    want_more: bool,
    /// The names the guest last said were in scope, for Tab. Shared,
    /// because the editor's completer holds it and is asked during a
    /// keystroke while this side refreshes it between lines.
    names: Arc<Mutex<Vec<String>>>,
    /// What the session defined, from the same snapshot as `names`: a JSON
    /// array of `{name, type, value?, params?, vararg?}` (repl.dlua's
    /// `defined`), for a page's variables panel.
    variables: String,
    /// A line has run since the snapshot was taken, so it is worth
    /// asking again before the next prompt.
    names_stale: bool,
    /// What [`Repl::banner`] says, and what the lines evaluate under.
    unsafe_stdlib: bool,
    /// `:quit` was typed: the loop that owns the terminal returns.
    quit: bool,
    /// Served to someone else's terminal (SSH): `print` comes back with the
    /// answers, and nothing reaches for this process's own terminal.
    served: bool,
    /// Whether `:pause`, `:resume` and `:stop` are this terminal's to
    /// give: the process's own REPL, or a served one whose key holds the
    /// ceiling (`crate::control`).
    may_order: bool,
}

impl Repl {
    /// The sealed REPL: the stdlib every other guest gets.
    pub fn new(
        dispatcher: Arc<Dispatcher>,
        caps: Vec<Grant>,
        budget: drt_config::Budget,
    ) -> Result<Self, String> {
        Self::build(dispatcher, caps, budget, false)
    }

    /// The REPL with `os`, `io` and `require` in scope.
    ///
    /// A separate constructor rather than a `bool` on [`Repl::new`], so
    /// that nothing acquires the unsafe stdlib without naming it and every
    /// caller that has it is one `grep` away. Still an instance: the
    /// capability grants and the budget apply as they always did, and what
    /// is off is the stdlib seal, not the sandbox. It costs replayability
    /// and makes the budget approximate (dv.h says why).
    pub fn unsealed(
        dispatcher: Arc<Dispatcher>,
        caps: Vec<Grant>,
        budget: drt_config::Budget,
    ) -> Result<Self, String> {
        Self::build(dispatcher, caps, budget, true)
    }

    fn build(
        dispatcher: Arc<Dispatcher>,
        caps: Vec<Grant>,
        budget: drt_config::Budget,
        unsafe_stdlib: bool,
    ) -> Result<Self, String> {
        let engine = Arc::new(DiluviumEngine::new().map_err(|e| e.to_string())?);
        // No residency policy: nothing the REPL spawns hibernates, so no
        // park floor either (`crate::start::deployment` says why they go
        // together).
        let mut sw = Swarm::new(engine, PumpHost::shared(DeployHost::new(), dispatcher));
        // The REPL's own source is ours, not the user's. Sealed, the lines
        // it evaluates run under the same stdlib every other guest gets,
        // and a REPL is not a way around the sandbox; unsealed, that is
        // what the caller asked for by name, and the caps and the budget
        // still hold. The debug set is not tied to it: a separate
        // authority, and a REPL has no more claim on `setlocal` than `drt
        // run` does. The numeric ceiling is the default, as `drt run`'s is
        // without a config: a line a user types is bounded by the same
        // ceiling everything else in this process is.
        sw.allow_unsafe_stdlib(unsafe_stdlib);
        let root = sw
            .root(PROGRAM.as_bytes(), caps, budget)
            .map_err(|e| e.to_string())?;
        Ok(Repl {
            sw,
            root,
            pending: String::new(),
            want_more: false,
            names: Arc::new(Mutex::new(Vec::new())),
            variables: "[]".into(),
            names_stale: true,
            unsafe_stdlib,
            quit: false,
            served: false,
            may_order: true,
        })
    }

    /// Whether the control endpoint's orders may be given from here.
    pub fn allow_orders(&mut self, may: bool) {
        self.may_order = may;
    }

    /// A sealed REPL for a terminal that is not this process's: an SSH
    /// session (`drt-sshd`, `crate::sshd`). `print` is collected and
    /// printed with each answer, because the core's own `print` writes to
    /// this process's stdout, and `:ssh` is refused, because it would take
    /// over this process's terminal rather than the session's.
    pub fn served(
        dispatcher: Arc<Dispatcher>,
        caps: Vec<Grant>,
        budget: drt_config::Budget,
    ) -> Result<Self, String> {
        let mut repl = Self::build(dispatcher, caps, budget, false)?;
        repl.served = true;
        // The guest declares its queues on its first slice; until then
        // there is nowhere to say it is served.
        for _ in 0..8 {
            if repl.input_queue().is_some() {
                break;
            }
            repl.sw.step();
        }
        let input = repl
            .input_queue()
            .ok_or("the repl program never declared its input queue")?;
        let served = rmpv::Value::Map(vec![(
            rmpv::Value::from("served"),
            rmpv::Value::Boolean(true),
        )]);
        let mut msg = Vec::new();
        rmpv::encode::write_value(&mut msg, &served).map_err(|e| e.to_string())?;
        let inst = repl
            .sw
            .instance_mut(repl.root)
            .ok_or("the repl's instance is gone")?;
        if !inst
            .push(input, &msg)
            .map_err(|e| e.to_string())?
            .is_accepted()
        {
            return Err("the repl's input queue is full".into());
        }
        Ok(repl)
    }

    /// The guest's input queue, once it has declared it.
    fn input_queue(&mut self) -> Option<QueueHandle> {
        self.sw
            .instance_mut(self.root)
            .and_then(|inst| inst.queue(IN))
    }

    /// What to print before the first prompt.
    ///
    /// An unsealed REPL that looked like a sealed one would be a trap, so
    /// the two do not look alike and the difference is named rather than
    /// hinted at. One function because three hosts print it -- the pipe,
    /// the tty and the page -- and they must not drift.
    pub fn banner(&self) -> &'static str {
        if self.unsafe_stdlib {
            "drt repl — unsafe stdlib: os, io, require — Show help: <:h+enter>"
        } else {
            "drt repl — Show help: <:h+enter>"
        }
    }

    /// Whether this REPL evaluates with the unsafe stdlib.
    pub fn is_unsealed(&self) -> bool {
        self.unsafe_stdlib
    }

    /// The names the guest last reported in scope. Empty until the first
    /// answer arrives, which is one tick after the instance starts.
    pub fn names(&self) -> Arc<Mutex<Vec<String>>> {
        self.names.clone()
    }

    /// What the session has defined, as of the same snapshot as
    /// [`Repl::names`]: a JSON array of `{name, type, value?, params?,
    /// vararg?}`, `value` as the REPL would show it (cut short) and
    /// `params` a function's fixed parameter count.
    pub fn variables(&self) -> &str {
        &self.variables
    }

    /// Advance, print whatever the last line produced, and say what is
    /// needed next. [`Next::Input`] means a line; whether it continues an
    /// unfinished one is [`Repl::continuing`].
    pub fn tick(&mut self) -> Next {
        // Twice at most: once to run whatever was fed, and once more to
        // let the guest answer what is in scope, so the prompt that
        // follows has a snapshot for Tab rather than the last line's.
        for _ in 0..2 {
            let next = self.drive();
            // Whatever the last line produced, before asking for the next.
            self.print_answers();
            match next {
                Next::Input if self.names_stale => {
                    self.names_stale = false;
                    self.ask_for_names();
                }
                other => return other,
            }
        }
        Next::Input
    }

    /// Step the swarm until the REPL is at its prompt, then say what it
    /// waits for. [`Next::Input`] when it is this terminal's line; a sleep
    /// while the REPL itself is parked on something else (a spawn's
    /// outcome, a connector reply, a deadline); a failure when it parked
    /// on something nothing here will ever wake.
    fn drive(&mut self) -> Next {
        // Bounded: a child that never parks would otherwise hold the
        // terminal, and the budget is the real bound on it, not this loop.
        for _ in 0..STEPS_PER_TICK {
            self.sw.step();
            for (id, what) in self.sw.host_mut().take_lost() {
                let _ = writeln!(stdio::stderr(), "drt: instance {} lost {what}", id.0);
            }
            if !self.sw.ids().contains(&self.root) {
                // The host already said why, on stderr, when it faulted.
                return Next::Failed("the repl's instance is gone".into());
            }
            // At the prompt, and nothing it spawned has a slice to run: time
            // to ask the terminal. A child still running gets the rest of
            // the tick first, so a line's spawn mostly finishes before the
            // prompt that follows it.
            if self.at_prompt() && !self.children_running() {
                return Next::Input;
            }
        }
        if self.at_prompt() {
            // Something it spawned is still running; it gets the next
            // tick, which is the host's to give (`busy`).
            return Next::Input;
        }
        if self.sw.host().inner().wait_of(self.root).is_none() {
            return Next::Sleep(self.idle());
        }
        let alone = self.sw.alive() == 1;
        let quiet = self.sw.host().pump().in_flight() == 0;
        if alone && quiet && self.sw.host().inner().next_deadline().is_none() {
            // Not waiting for us, and nothing else will ever answer it:
            // a park this terminal can never wake.
            return Next::Failed("the repl parked on something the terminal cannot wake".into());
        }
        Next::Sleep(self.idle())
    }

    /// Whether the REPL is parked on its input queue: its line is wanted.
    fn at_prompt(&mut self) -> bool {
        let Some(wait) = self.sw.host().inner().wait_of(self.root) else {
            return false;
        };
        self.input_queue()
            .is_some_and(|q| wait.queues().contains(&q))
    }

    /// Whether anything the REPL spawned is still here. A host that reads
    /// a line without blocking keeps ticking while this holds, so a child
    /// runs while the person types rather than only between lines; a host
    /// that blocks on its terminal steps it between lines, which is what
    /// the native tty does.
    pub fn busy(&self) -> bool {
        self.sw.alive() > 1
    }

    /// Whether an instance other than the root is resident and not
    /// parked: it has a slice to run now.
    fn children_running(&self) -> bool {
        let host = self.sw.host().inner();
        self.sw
            .ids()
            .into_iter()
            .any(|id| id != self.root && host.wait_of(id).is_none())
    }

    /// How long the host may sleep before the next step is due: until the
    /// earliest park deadline, and never longer than [`IDLE_TICK`].
    fn idle(&self) -> Duration {
        self.sw
            .host()
            .inner()
            .next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()).min(IDLE_TICK))
            .unwrap_or(IDLE_TICK)
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
        let Some(inst) = self.sw.instance_mut(self.root) else {
            return Err("the repl's instance is gone".into());
        };
        let Some(input) = inst.queue(IN) else {
            return Err("the repl program never declared its input queue".into());
        };
        let mut msg = Vec::new();
        rmpv::encode::write_value(&mut msg, &rmpv::Value::from(self.pending.as_str()))
            .map_err(|e| e.to_string())?;
        if !inst
            .push(input, &msg)
            .map_err(|e| e.to_string())?
            .is_accepted()
        {
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
        self.sw.host().dispatcher()
    }

    /// The swarm, for a host that shows or orders what the REPL spawned:
    /// the roster `drt ps` walks, and a page's Instances panel.
    pub fn deployment_mut(&mut self) -> &mut Deployment {
        &mut self.sw
    }

    // depth: answers

    fn print_answers(&mut self) {
        let Some(inst) = self.sw.instance_mut(self.root) else {
            return;
        };
        let Some(out) = inst.queue(OUT) else {
            return;
        };
        let mut want_more = false;
        let mut answered = false;
        let mut raws = Vec::new();
        while let Ok(Some(raw)) = inst.pop(out) {
            raws.push(raw);
        }
        for raw in raws {
            match Answer::of(&raw) {
                // Consumed, never printed: this one is for Tab.
                Answer::Names(names, variables) => {
                    *self.names.lock().unwrap_or_else(|e| e.into_inner()) = names;
                    self.variables = variables;
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
        let Some(inst) = self.sw.instance_mut(self.root) else {
            return;
        };
        let Some(input) = inst.queue(IN) else {
            return;
        };
        let ask = rmpv::Value::Map(vec![(
            rmpv::Value::from("complete"),
            rmpv::Value::Boolean(true),
        )]);
        let mut msg = Vec::new();
        if rmpv::encode::write_value(&mut msg, &ask).is_ok() {
            let _ = inst.push(input, &msg);
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
    unsafe_stdlib: bool,
) -> Result<(), String> {
    let _runtime = crate::runtime::enter();
    let repl = if unsafe_stdlib {
        Repl::unsealed(dispatcher, caps, budget)?
    } else {
        Repl::new(dispatcher, caps, budget)?
    };
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

    eprintln!("{}", repl.banner());
    loop {
        match repl.tick() {
            Next::Sleep(how_long) => std::thread::sleep(how_long),
            Next::Input => {
                let Some(line) = prompt(&mut lines, repl.continuing())? else {
                    eprintln!();
                    return Ok(());
                };
                if !repl.continuing() && meta(&mut repl, &line)? {
                    if repl.quit {
                        return Ok(());
                    }
                    continue;
                }
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

// depth: commands the host answers itself

/// A line the terminal's side answers rather than the guest: the colon
/// commands [`META_HELP`] lists. True
/// when it was one. A Diluvium line never starts with `:`, so nothing a
/// program could say is taken.
///
/// `:ssh` alone is the config's `host:ssh/shell`, evaluated as a guest
/// line, so the grant and the scope decide it exactly as they would for a
/// program. `:ssh <target> …` is `drt ssh <target> …`: the person at this
/// terminal naming a host, trusted through known_hosts. Either way the
/// editor is not reading while the session runs, so the terminal is the
/// session's, and the prompt comes back when it ends.
/// How long the guest waits on `:ssh`'s `host:ssh/shell` reply.
const SESSION_WAIT_MS: u32 = i32::MAX as u32;

/// What `:clear` writes: home, then erase to the end of the screen. The
/// same bytes ego-cli writes for Ctrl+L, so the two clear alike. Not
/// `\x1b[2J`: VS Code's terminal scrolls that into scrollback and leaves
/// its viewport a row short, hiding the prompt that follows.
const CLEAR_SCREEN: &str = "\x1b[H\x1b[J";

/// Every colon command, for `:help`: one line each, in the order a person
/// meets them.
const META_HELP: &str = "\
:help                this list
:quit, :q            leave (so does ^D)
:clear               clear the screen (so does ^L)
:ssh [args]          an SSH session on this terminal; alone, the config's host:ssh/shell
:ps                  a running deployment's instances (SPEC.md §13a)
:status              its state
:caps <id>           what instance <id> holds
:pause <id>          hibernate <id> if it is parked
:resume <id>         wake <id>
:stop                hibernate what is parked and end the deployment";

fn meta(repl: &mut Repl, line: &str) -> Result<bool, String> {
    let trimmed = line.trim_start();
    match trimmed.trim_end() {
        ":help" | ":h" | ":?" => {
            let _ = writeln!(stdio::stderr(), "{META_HELP}");
            return Ok(true);
        }
        ":quit" | ":q" | ":exit" => {
            repl.quit = true;
            return Ok(true);
        }
        ":clear" => {
            // Only a terminal has a screen. Through a redirect the bytes
            // would land in the answers, which stay clean.
            use std::io::IsTerminal;
            if std::io::stdout().is_terminal() {
                let mut out = stdio::stdout();
                let _ = out.write_all(CLEAR_SCREEN.as_bytes());
                let _ = out.flush();
            }
            return Ok(true);
        }
        _ => {}
    }
    if control_meta(repl, trimmed) {
        return Ok(true);
    }
    let Some(rest) = trimmed.strip_prefix(":ssh") else {
        return Ok(false);
    };
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return Ok(false);
    }
    if repl.served {
        let _ = writeln!(
            stdio::stderr(),
            ":ssh is not available in a REPL reached over SSH: the session it \
             would open is this server's terminal, not yours"
        );
        return Ok(true);
    }
    if rest.trim().is_empty() {
        // The reply deadline is the session's length, and a person decides
        // that: `host.call`'s default ten seconds would have the guest give
        // up while the shell still has the terminal. The longest wait a
        // queue takes, which is ~24 days.
        repl.feed(&format!(
            "return host.call('ssh/shell', nil, {SESSION_WAIT_MS})"
        ))?;
        return Ok(true);
    }
    ssh_to(rest);
    Ok(true)
}

/// `:ps`, `:status`, `:caps <id>`, `:pause <id>`, `:resume <id>`,
/// `:stop`: the control endpoint (SPEC.md §13a), from inside a REPL in a
/// `drt start` process. Elsewhere they say where a deployment would be.
fn control_meta(repl: &Repl, trimmed: &str) -> bool {
    use crate::control;
    let Some(rest) = trimmed.strip_prefix(':') else {
        return false;
    };
    let words: Vec<&str> = rest.split_whitespace().collect();
    if !matches!(
        words.first().copied(),
        Some("ps" | "status" | "caps" | "pause" | "resume" | "stop")
    ) {
        return false;
    }
    let ask = match control::parse(&words) {
        Ok(ask) => ask,
        Err(e) => {
            let _ = writeln!(stdio::stderr(), ":{}: {e}", words[0]);
            return true;
        }
    };
    let Some(handle) = control::handle() else {
        let _ = writeln!(
            stdio::stderr(),
            ":{}: no deployment is running in this process; these reach a `drt start`, \
             from a REPL it serves or with `drt ps ssh://host:port`",
            ask.verb()
        );
        return true;
    };
    if ask.is_order() && !repl.may_order {
        let _ = writeln!(
            stdio::stderr(),
            ":{}: needs a key holding host:*, the deployment's ceiling",
            ask.verb()
        );
        return true;
    }
    match handle.ask(ask.clone()) {
        Ok((answer, _delivered)) => {
            let text = control::render(&ask, &answer);
            let _ = write!(stdio::stdout(), "{text}");
        }
        Err(e) => {
            let _ = writeln!(stdio::stderr(), ":{}: {e}", ask.verb());
        }
    }
    true
}

#[cfg(feature = "connector-ssh")]
fn ssh_to(rest: &str) {
    let args = match crate::ssh::from_line(rest) {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(stdio::stderr(), "{e}");
            return;
        }
    };
    match crate::ssh::run(&args) {
        Ok(Some(status)) => {
            let _ = writeln!(
                stdio::stderr(),
                "ssh: {} closed, exit {status}",
                args.target
            );
        }
        Ok(None) => {
            let _ = writeln!(stdio::stderr(), "ssh: {} closed", args.target);
        }
        Err(e) => {
            let _ = writeln!(stdio::stderr(), "ssh: {e}");
        }
    }
}

#[cfg(not(feature = "connector-ssh"))]
fn ssh_to(_rest: &str) {
    let _ = writeln!(
        stdio::stderr(),
        "ssh: this build has no ssh client (it is in `full`)"
    );
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

/// Colour for the line being typed.
///
/// `ego_cli` has had the hook since DRT adopted it and nothing used it, so
/// the prompt was one colour. The keywords are **Diluvium's**, read off
/// `llex.c` rather than assumed from Lua: `global` is one and `continue`
/// is not (it is contextual, §3.3 of the syntax proposals), and an
/// f-string opens `$"` or `$'`.
///
/// `continue` is coloured anyway. It is contextual, so `continue = 1`
/// gets a keyword's colour wrongly -- accepted on the grounds that a
/// highlighter is a hint rather than a parser, that `continue` inside a
/// loop is the case people meet, and that naming a variable `continue`
/// is rare enough to be worth the trade. Every editor that colours
/// contextual keywords makes the same one.
///
/// **The contract is escapes only.** The cursor is measured against the
/// line the editor holds, so adding or dropping one printable character
/// puts it in the wrong column. `strip(highlight(line)) == line` is the
/// invariant, and it is the test.
#[cfg(feature = "cli")]
pub struct Syntax;

#[cfg(feature = "cli")]
const KEYWORDS: &[&str] = &[
    "and", "break", "continue", "do", "else", "elseif", "end", "false", "for", "function",
    "global", "goto", "if", "in", "local", "nil", "not", "or", "repeat", "return", "then", "true",
    "until", "while",
];

#[cfg(feature = "cli")]
impl ego_cli::extend::Highlighter for Syntax {
    fn highlight<'l>(&self, line: &'l str) -> std::borrow::Cow<'l, str> {
        use ego_cli::style::{fg, Color, RESET};

        // Every token start is ASCII, and the bytes between are copied
        // through untouched, so byte indices never land inside a char.
        let b = line.as_bytes();
        let mut out = String::with_capacity(line.len() + 32);
        let mut i = 0;
        // Nothing was coloured: hand back the borrow rather than a copy.
        let mut touched = false;

        let paint = |out: &mut String, text: &str, c: Color| {
            out.push_str(fg(c));
            out.push_str(text);
            out.push_str(RESET);
        };

        while i < b.len() {
            let start = i;
            // A comment runs to the end of the line. Long comments are
            // not special-cased: on one REPL line `--[[` reaches the end
            // either way, and the colour is the same.
            if b[i] == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
                paint(&mut out, &line[i..], Color::BrightBlack);
                touched = true;
                break;
            }
            // A string, plain or an f-string's `$` prefix. Unterminated
            // is the normal case here: the line is still being typed, so
            // the colour simply runs to the end.
            let fstring =
                b[i] == b'$' && i + 1 < b.len() && (b[i + 1] == b'"' || b[i + 1] == b'\'');
            if fstring || b[i] == b'"' || b[i] == b'\'' {
                let quote = if fstring { b[i + 1] } else { b[i] };
                i += if fstring { 2 } else { 1 };
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                paint(&mut out, &line[start..i.min(b.len())], Color::Green);
                touched = true;
                continue;
            }
            // A long string, `[[` or `[=[`. Same reasoning as comments:
            // one line, so it reaches the end.
            if b[i] == b'[' && i + 1 < b.len() && (b[i + 1] == b'[' || b[i + 1] == b'=') {
                let mut j = i + 1;
                while j < b.len() && b[j] == b'=' {
                    j += 1;
                }
                if j < b.len() && b[j] == b'[' {
                    paint(&mut out, &line[i..], Color::Green);
                    touched = true;
                    break;
                }
            }
            if b[i].is_ascii_digit() {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_')
                {
                    i += 1;
                }
                paint(&mut out, &line[start..i], Color::Cyan);
                touched = true;
                continue;
            }
            if b[i].is_ascii_alphabetic() || b[i] == b'_' {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let word = &line[start..i];
                if KEYWORDS.contains(&word) {
                    paint(&mut out, word, Color::Blue);
                    touched = true;
                } else {
                    out.push_str(word);
                }
                continue;
            }
            // Anything else -- operators, spaces, punctuation -- through
            // one character at a time. Multi-byte characters arrive here
            // and are copied by their own length.
            let ch = line[i..].chars().next().expect("index is on a boundary");
            out.push(ch);
            i += ch.len_utf8();
        }

        if touched {
            std::borrow::Cow::Owned(out)
        } else {
            std::borrow::Cow::Borrowed(line)
        }
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
    session.set_highlighter(Syntax);
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
                        if !repl.continuing() && meta(repl, &line)? {
                            if repl.quit {
                                return Ok(());
                            }
                            continue;
                        }
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
    let terminal = Keys(ego_cli::term::platform().map_err(|e| format!("no terminal: {e}"))?);
    let mut session = editor(&repl, terminal);

    // `block_on` and nothing else: with ego-cli's `runtime` feature off,
    // `platform()` is `BlockingNative`, which blocks on the terminal itself
    // and never returns `Pending` -- so this future runs to completion on
    // the first poll. No reactor to build, and nothing to leak past the
    // tokio teardown bug `cli.rs` still works around for the relay.
    eprintln!("{}", repl.banner());
    futures_executor::block_on(edit(&mut repl, &mut session))
}

/// The native tty, with Ctrl+Backspace read the way the page reads it.
///
/// Most terminals send BS (0x08) for Ctrl+Backspace and DEL for a plain
/// Backspace. crossterm, which `ego_cli` reads a native tty through, turns
/// BS into Ctrl+H, which nothing binds, so Ctrl+Backspace deleted nothing
/// while Ctrl+arrows (escape sequences crossterm does name) moved by
/// words. `ego_cli`'s own byte decoder, the page's, already reads BS as
/// Ctrl+Backspace, and so does this. It belongs in `ego_cli`'s crossterm
/// mapping; until it is there, it is here.
#[cfg(all(
    feature = "cli",
    not(all(target_arch = "wasm32", target_os = "unknown"))
))]
pub struct Keys<T>(pub T);

#[cfg(all(
    feature = "cli",
    not(all(target_arch = "wasm32", target_os = "unknown"))
))]
impl<T: ego_cli::term::Terminal> ego_cli::term::Terminal for Keys<T> {
    fn capabilities(&self) -> ego_cli::term::Capabilities {
        self.0.capabilities()
    }

    fn size(&self) -> ego_cli::term::Size {
        self.0.size()
    }

    fn set_raw(&mut self, enabled: bool) -> ego_cli::Result<()> {
        self.0.set_raw(enabled)
    }

    async fn next_event(&mut self) -> ego_cli::Result<ego_cli::term::Event> {
        use ego_cli::{KeyCode, KeyPress, Mods};
        Ok(match self.0.next_event().await? {
            ego_cli::term::Event::Key(KeyPress {
                code: KeyCode::Char('h'),
                mods,
            }) if mods == Mods::CTRL => {
                ego_cli::term::Event::Key(KeyPress::new(KeyCode::Backspace, Mods::CTRL))
            }
            other => other,
        })
    }

    async fn write(&mut self, text: &str) -> ego_cli::Result<()> {
        self.0.write(text).await
    }

    async fn flush(&mut self) -> ego_cli::Result<()> {
        self.0.flush().await
    }
}

/// What came back on `repl/out`: something to show, or the names Tab
/// completes from.
enum Answer {
    /// Already printed. `more` means the line was unfinished rather than
    /// wrong, which is the host's cue to keep reading instead of reporting.
    Text { more: bool },
    /// The names in scope, and what the session defined as JSON.
    Names(Vec<String>, String),
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
            let variables = match get("vars") {
                Some(vars @ rmpv::Value::Array(_)) => json_of(&vars).to_string(),
                _ => "[]".into(),
            };
            return Answer::Names(
                names
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                variables,
            );
        }
        // What a served REPL's line printed, before its answer.
        if let Some(printed) = get("printed").and_then(|v| v.as_str().map(str::to_string)) {
            let _ = write!(stdio::stdout(), "{printed}");
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

/// A guest's msgpack answer as JSON: what `defined` sends is maps, arrays,
/// strings, integers and booleans; anything else becomes null.
fn json_of(value: &rmpv::Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        rmpv::Value::Nil => J::Null,
        rmpv::Value::Boolean(b) => J::Bool(*b),
        rmpv::Value::Integer(i) => i
            .as_i64()
            .map(J::from)
            .or_else(|| i.as_u64().map(J::from))
            .unwrap_or(J::Null),
        rmpv::Value::F32(f) => J::from(f64::from(*f)),
        rmpv::Value::F64(f) => J::from(*f),
        rmpv::Value::String(s) => s.as_str().map(J::from).unwrap_or(J::Null),
        rmpv::Value::Array(items) => J::Array(items.iter().map(json_of).collect()),
        rmpv::Value::Map(pairs) => J::Object(
            pairs
                .iter()
                .filter_map(|(k, v)| k.as_str().map(|k| (k.to_string(), json_of(v))))
                .collect(),
        ),
        rmpv::Value::Binary(_) | rmpv::Value::Ext(..) => J::Null,
    }
}

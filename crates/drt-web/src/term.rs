//! The terminal contract (doc/Wasm.md §5): a `drt` command line in, bytes
//! out, and a tick the page calls.
//!
//! [`Term::exec`] is `main.rs` without the loop: the same `Cli`, the same
//! `assemble`, the same verbs, the same sentences on stderr and the same
//! exit statuses -- so `drt run app.dlua` typed into a page says what it
//! says in a shell, and `expected.txt` is the oracle there too. What
//! differs is only that a [`Session`] does not run itself: [`Session::tick`]
//! advances it and returns what the page should do next, which is sleep
//! for a stated time, feed a line, or show the exit status.
//!
//! ## surface block
//!
//! - [`Term`]: the memory filesystem a page seeds, and `exec`.
//!   [`Term::with_sink`] gives a terminal output of its own;
//!   [`Term::exec_to`] runs one command into a sink of the caller's.
//! - [`Session`]: one command, ticked to completion.
//!   [`Session::enter`] for host calls that reach the runtime between
//!   ticks.
//! - [`Step`]: what a tick asks of the page.
//! - [`Sink`]: where a terminal's bytes go.
//!
//! Several terminals in one page (doc/Browser.md): the runtime reads
//! through one process filesystem (`drt_platform::fs::host`) and writes
//! through one stdio sink, because that is where the C core's libc
//! `fd_write` lands and it has no way to say whose `print` it is. So each
//! terminal carries its own filesystem and sink as a [`Context`], and
//! everything that runs a command -- `exec`, every tick, every feed --
//! installs that context for its duration and puts back what was there.
//! The page is one thread and ticks are synchronous, so one kernel at a
//! time is the only kernel there is, the way one process at a time is on
//! one CPU.

use std::io::Write;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use clap::error::ErrorKind;
use clap::Parser;
use drt::cli::{self, Cli, Command};
use drt::config;
use drt::drive::{Next, Solo};
use drt::repl::Repl;
use drt::start::DeployDriver;
use drt_connector::Dispatcher;
use drt_platform::fs::{Backend, MemFs};
use drt_platform::stdio::{self, Fd};

/// Where a terminal's bytes go: the C core's `print`, the REPL's answers,
/// the runtime's own sentences, with the stream each was written to.
pub type Sink = Rc<dyn Fn(Fd, &[u8])>;

/// What the page does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing until this elapses; then tick again.
    Sleep(Duration),
    /// The REPL wants a line. `continuing` chooses the prompt: `>> ` for an
    /// unfinished line, `dv> ` otherwise.
    Input { continuing: bool },
    /// Over, with the status the binary would have exited with.
    Exit(i32),
}

/// The terminal: a filesystem the page fills, and a way to run commands
/// against it.
pub struct Term {
    fs: Arc<MemFs>,
    sink: Option<Sink>,
}

impl Default for Term {
    fn default() -> Self {
        Self::new()
    }
}

impl Term {
    /// A terminal over an empty memory filesystem, installed as the
    /// process's own (`drt_platform::fs::install`): a page is one process,
    /// and every file a command opens -- a program, a config, a granted
    /// directory -- is one the page put here.
    ///
    /// Its commands write to whatever sink is installed when they run. A
    /// page with more than one terminal wants [`Term::with_sink`].
    pub fn new() -> Self {
        let fs = Arc::new(MemFs::new());
        drt_platform::fs::install(fs.clone());
        Term { fs, sink: None }
    }

    /// A terminal whose commands write to `sink` and read this terminal's
    /// filesystem, whichever other terminals the page holds.
    pub fn with_sink(sink: Sink) -> Self {
        Term {
            sink: Some(sink),
            ..Term::new()
        }
    }

    /// The filesystem, to seed and to read back.
    pub fn fs(&self) -> &MemFs {
        &self.fs
    }

    /// Run one command line. `argv` is what a shell would hand `main`,
    /// `drt` first: `["drt", "run", "app.dlua"]`. Nothing runs until the
    /// first tick; parse errors, `--help` and `buildinfo` are answered here
    /// and the session is already over.
    pub fn exec(&self, argv: &[String]) -> Session {
        self.exec_to(argv, self.sink.clone())
    }

    /// [`Term::exec`], with this command's output going to `sink` instead
    /// of the terminal's own: a raw REPL's byte stream, say. `None` writes
    /// to whatever sink is installed when it runs.
    pub fn exec_to(&self, argv: &[String], sink: Option<Sink>) -> Session {
        let context = Context {
            fs: self.fs.clone(),
            sink,
        };
        let kind = {
            let _entered = context.enter();
            self.command(argv)
        };
        Session { kind, context }
    }

    fn command(&self, argv: &[String]) -> Kind {
        let cli = match Cli::try_parse_from(argv) {
            Ok(cli) => cli,
            Err(e) => {
                let mut text = e.to_string();
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                // clap's own conventions: help and version to stdout and
                // status 0; a usage error to stderr and status 2.
                return match e.kind() {
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                        say(Fd::Stdout, &text);
                        Kind::Exited(0)
                    }
                    _ => {
                        say(Fd::Stderr, &text);
                        Kind::Exited(2)
                    }
                };
            }
        };
        let (config, dispatcher) = match cli::assemble(&cli) {
            Ok(pair) => pair,
            Err(e) => {
                say(Fd::Stderr, &format!("drt: {e}\n"));
                return Kind::Exited(1);
            }
        };
        match cli.command {
            Command::Run {
                ref target,
                file,
                command,
                profile,
            } => {
                use drt_config::resolve::{classify, Explicit, Requested};
                // Same classifier as the shell, so `drt run "print('hi')"` in a
                // page means what it means in a terminal. A page declares no
                // profiles -- it has no root -- so the bare-token rule falls
                // through to a file, which is the right answer here.
                let explicit = match (file, command, profile) {
                    (true, _, _) => Some(Explicit::File),
                    (_, true, _) => Some(Explicit::Code),
                    (_, _, true) => Some(Explicit::Profile),
                    _ => None,
                };
                let requested = match target {
                    Some(token) => classify(token, explicit, &[]),
                    // No argument: the config's own program, as before.
                    None => match &config.root.program {
                        Some(drt_config::Program::Path(p)) => {
                            Requested::File(p.display().to_string())
                        }
                        Some(drt_config::Program::Source(src)) => Requested::Code(src.clone()),
                        None => {
                            say(
                                Fd::Stderr,
                                "drt run: name a program, as an argument or as `program` in the config\n",
                            );
                            return Kind::Exited(1);
                        }
                    },
                };
                let dispatcher = Arc::new(dispatcher);
                let prepared = match requested {
                    Requested::File(path) => drt::run::prepare(
                        Path::new(&path),
                        dispatcher.clone(),
                        config::ceiling(&config),
                        config.root.budget,
                        config.root.numeric,
                    ),
                    Requested::Code(source) => drt::run::prepare_source(
                        &source,
                        "=command",
                        dispatcher.clone(),
                        config::ceiling(&config),
                        config.root.budget,
                        config.root.numeric,
                    ),
                    // A page has no stdin to pipe and no root to hold a
                    // profile, and a stdlib program is a root's report. Each is
                    // refused by name rather than silently reading as a file.
                    Requested::Stdin => Err("a page has no standard input to read".to_string()),
                    Requested::Profile(name) => Err(format!(
                        "there is no root here, so '{name}' names no profile"
                    )),
                    Requested::Stdlib(name) => {
                        Err(format!("a page carries no stdlib program '{name}'"))
                    }
                    Requested::Default => unreachable!("classify never answers Default"),
                };
                match prepared {
                    Ok(solo) => Kind::Run { solo, dispatcher },
                    Err(e) => {
                        say(Fd::Stderr, &format!("drt run: {e}\n"));
                        Kind::Exited(1)
                    }
                }
            }
            // A page is always on the no-root path: there is no `.drt_root/`
            // in a memory filesystem a page seeded, and consent bounds a root
            // rather than a config. So a profile name here has nothing to name,
            // and saying so beats silently starting something else.
            Command::Start {
                profile: Some(name),
                ..
            } => {
                say(
                    Fd::Stderr,
                    &format!(
                        "drt start: there is no root here, so '{name}' names no profile; \
                         a page runs the config it was given\n"
                    ),
                );
                Kind::Exited(1)
            }
            Command::Start { profile: None, .. } => {
                match drt::start::prepare(&config, dispatcher) {
                    Ok(driver) => Kind::Start(driver),
                    Err(e) => {
                        say(Fd::Stderr, &format!("drt start: {e}\n"));
                        Kind::Exited(1)
                    }
                }
            }
            // The root verbs. A page has no `.drt_root/` -- there is no disk to
            // put one on and no `live/` to copy into -- so each says so rather
            // than appearing to work. `key` is absent for a different reason:
            // a page has nowhere to keep a private key that a page should keep.
            Command::Deploy | Command::Rm | Command::Commit | Command::Key { .. } => {
                say(
                    Fd::Stderr,
                    "drt: that verb acts on a root, and a page has none\n",
                );
                Kind::Exited(1)
            }
            Command::Repl { unsafe_stdlib } => {
                let build = if unsafe_stdlib {
                    Repl::unsealed
                } else {
                    Repl::new
                };
                match build(
                    Arc::new(dispatcher),
                    config::ceiling(&config),
                    config.root.budget,
                ) {
                    Ok(repl) => {
                        say(Fd::Stderr, &format!("{}\n", repl.banner()));
                        Kind::Repl(repl)
                    }
                    Err(e) => {
                        say(Fd::Stderr, &format!("drt repl: {e}\n"));
                        Kind::Exited(1)
                    }
                }
            }
            Command::Buildinfo { json } => {
                say(Fd::Stdout, &cli::buildinfo(json));
                Kind::Exited(0)
            }
            // The same script the native binary prints, for whoever copies it
            // out of the page; nothing in a page sources it.
            Command::Completions { shell } => {
                let mut script = Vec::new();
                cli::completions(shell, &mut script);
                say(Fd::Stdout, &String::from_utf8_lossy(&script));
                Kind::Exited(0)
            }
            Command::Ps(_) => {
                say(
                    Fd::Stderr,
                    "drt ps: reaches a running deployment over its ssh listener (SPEC.md \
                     §13a), and a page has no socket to reach one with\n",
                );
                Kind::Exited(1)
            }
            // The verbs behind native-only features (`tunnel`, `netcheck`,
            // `wg`): absent from a browser build of `drt`, and present when
            // this crate is compiled natively against a fuller one, where
            // they still have no loop to run in here. `relay`, `stun` and
            // `turn` are no longer among them -- they are `start` with a
            // block now, and `start` is refused above, by name.
            #[allow(unreachable_patterns)]
            _ => {
                say(
                    Fd::Stderr,
                    "drt: that verb needs sockets and a runtime of its own, which a terminal \
                     session does not have\n",
                );
                Kind::Exited(1)
            }
        }
    }
}

// depth: a terminal's context, installed for as long as its command runs

/// What a terminal's command reads through and writes to.
#[derive(Clone)]
struct Context {
    fs: Arc<MemFs>,
    sink: Option<Sink>,
}

impl Context {
    fn enter(&self) -> Entered {
        let fs = drt_platform::fs::install(self.fs.clone());
        let sink = self
            .sink
            .clone()
            .map(|sink| stdio::install_sink(Box::new(move |fd, bytes| sink(fd, bytes))));
        Entered { fs, sink }
    }
}

/// A sink as `drt_platform::stdio` holds it.
type InstalledSink = Box<dyn Fn(Fd, &[u8])>;

/// A terminal's context, installed; what was there before comes back on
/// drop. Nested entries unwind in order, so a sink that runs another
/// terminal's command hands the page back as it found it.
pub struct Entered {
    fs: Option<Arc<dyn Backend>>,
    /// `None`: the context has no sink and left the installed one alone.
    sink: Option<Option<InstalledSink>>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        match self.fs.take() {
            Some(fs) => {
                drt_platform::fs::install(fs);
            }
            None => {
                drt_platform::fs::uninstall();
            }
        }
        match self.sink.take() {
            Some(Some(sink)) => {
                stdio::install_sink(sink);
            }
            Some(None) => {
                stdio::uninstall_sink();
            }
            None => {}
        }
    }
}

/// One command, ticked to completion.
pub struct Session {
    kind: Kind,
    context: Context,
}

enum Kind {
    Run {
        solo: Solo,
        dispatcher: Arc<Dispatcher>,
    },
    Repl(Repl),
    Start(DeployDriver),
    Exited(i32),
}

impl Session {
    /// The deployment this session is driving, when it is driving one.
    ///
    /// `Some` only for `drt start`: that is the verb with a swarm. `drt
    /// run` and `drt repl` drive a single instance through `Solo`, so
    /// there is no roster to hand out and `None` is the honest answer
    /// rather than an empty one.
    ///
    /// This exists so a page's Instances panel can show the agents someone
    /// started in its terminal. Without it a `DrtSwarm` and a `DrtTerm`
    /// are two separate worlds with two separate deployments, and a panel
    /// beside a terminal shows everything except what the terminal is
    /// running.
    pub fn deployment_mut(&mut self) -> Option<&mut drt::start::Deployment> {
        match &mut self.kind {
            Kind::Start(driver) => Some(driver.deployment_mut()),
            Kind::Run { .. } | Kind::Repl(_) | Kind::Exited(_) => None,
        }
    }

    /// Install this session's terminal -- its filesystem and sink -- until
    /// the guard drops. `tick` and `feed` do this themselves; a host call
    /// that reaches the runtime some other way (a message pushed into the
    /// deployment, a kill) holds one around the call.
    pub fn enter(&self) -> Entered {
        self.context.enter()
    }

    /// Advance, and say what the page does next. Over sessions answer
    /// [`Step::Exit`] again on every tick.
    pub fn tick(&mut self) -> Step {
        let _entered = self.context.enter();
        let step = match &mut self.kind {
            Kind::Exited(status) => return Step::Exit(*status),
            Kind::Run { solo, dispatcher } => {
                let next = solo.tick(None);
                match drt::run::settle(&next, dispatcher) {
                    None => match next {
                        Next::Sleep(d) => Step::Sleep(d),
                        // `settle` returns `None` only for a sleep.
                        _ => Step::Exit(1),
                    },
                    Some(Ok(())) => Step::Exit(0),
                    Some(Err(why)) => {
                        say(Fd::Stderr, &format!("drt run: {why}\n"));
                        Step::Exit(1)
                    }
                }
            }
            Kind::Repl(repl) => match repl.tick() {
                Next::Sleep(d) => Step::Sleep(d),
                Next::Input => Step::Input {
                    continuing: repl.continuing(),
                },
                Next::Done(_) => Step::Exit(0),
                Next::Failed(why) => {
                    say(Fd::Stderr, &format!("drt repl: {why}\n"));
                    Step::Exit(1)
                }
                Next::Stuck { .. } => {
                    say(
                        Fd::Stderr,
                        "drt repl: the repl parked on something the terminal cannot wake\n",
                    );
                    Step::Exit(1)
                }
            },
            Kind::Start(driver) => {
                match driver.tick() {
                    Next::Sleep(d) => Step::Sleep(d),
                    Next::Done(_) => match drt::run::finish(driver.dispatcher()) {
                        Ok(()) => Step::Exit(0),
                        Err(why) => {
                            say(Fd::Stderr, &format!("drt start: {why}\n"));
                            Step::Exit(1)
                        }
                    },
                    Next::Failed(why) => {
                        say(Fd::Stderr, &format!("drt start: {why}\n"));
                        Step::Exit(1)
                    }
                    Next::Input | Next::Stuck { .. } => {
                        say(Fd::Stderr, "drt start: the deployment asked for something a swarm never asks for\n");
                        Step::Exit(1)
                    }
                }
            }
        };
        if let Step::Exit(status) = step {
            self.kind = Kind::Exited(status);
        }
        step
    }

    /// Feed the REPL one line. Returns whether anything was sent (a blank
    /// line outside a continuation is not). Only a `repl` session takes
    /// input.
    pub fn feed(&mut self, line: &str) -> Result<bool, String> {
        let _entered = self.context.enter();
        match &mut self.kind {
            Kind::Repl(repl) => repl.feed(line),
            _ => Err("this session takes no input".into()),
        }
    }

    /// Whether the REPL's next line continues an unfinished one.
    pub fn continuing(&self) -> bool {
        match &self.kind {
            Kind::Repl(repl) => repl.continuing(),
            _ => false,
        }
    }

    pub fn is_over(&self) -> bool {
        matches!(self.kind, Kind::Exited(_))
    }

    /// The names the guest last said were in scope, for Tab.
    ///
    /// §5's snapshot: `Repl` refreshes it after every accepted line, and
    /// the host hands it to the editor, whose `Completer` is synchronous
    /// on purpose. Not a repl session: nothing to complete.
    pub fn names(&self) -> Vec<String> {
        match &self.kind {
            Kind::Repl(repl) => {
                let names = repl.names();
                let names = names.lock().unwrap_or_else(|e| e.into_inner());
                names.clone()
            }
            _ => Vec::new(),
        }
    }

    /// Throw away an unfinished line, the way Ctrl+C does natively.
    pub fn abandon(&mut self) {
        let _entered = self.context.enter();
        if let Kind::Repl(repl) = &mut self.kind {
            repl.abandon();
        }
    }
}

fn say(fd: Fd, text: &str) {
    let _ = stdio::Stream(fd).write_all(text.as_bytes());
}

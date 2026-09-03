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
//! - [`Session`]: one command, ticked to completion.
//! - [`Step`]: what a tick asks of the page.

use std::io::Write;
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
use drt_platform::fs::MemFs;
use drt_platform::stdio::{self, Fd};

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
    pub fn new() -> Self {
        let fs = Arc::new(MemFs::new());
        drt_platform::fs::install(fs.clone());
        Term { fs }
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
                        Session::exited(0)
                    }
                    _ => {
                        say(Fd::Stderr, &text);
                        Session::exited(2)
                    }
                };
            }
        };
        let (config, dispatcher) = match cli::assemble(&cli) {
            Ok(pair) => pair,
            Err(e) => {
                say(Fd::Stderr, &format!("drt: {e}\n"));
                return Session::exited(1);
            }
        };
        match cli.command {
            Command::Run { ref program } => {
                // The CLI argument names the program; a config may name one
                // too, and the argument wins because it is the more specific
                // thing the operator just typed.
                let path = program.clone().or_else(|| match &config.root.program {
                    Some(drt_config::Program::Path(p)) => Some(p.clone()),
                    _ => None,
                });
                let Some(path) = path else {
                    say(
                        Fd::Stderr,
                        "drt run: name a program, as an argument or as `program` in the config\n",
                    );
                    return Session::exited(1);
                };
                let dispatcher = Arc::new(dispatcher);
                match drt::run::prepare(
                    &path,
                    dispatcher.clone(),
                    config::ceiling(&config),
                    config.root.budget,
                ) {
                    Ok(solo) => Session {
                        kind: Kind::Run { solo, dispatcher },
                    },
                    Err(e) => {
                        say(Fd::Stderr, &format!("drt run: {e}\n"));
                        Session::exited(1)
                    }
                }
            }
            Command::Start => match drt::start::prepare(&config, dispatcher) {
                Ok(driver) => Session {
                    kind: Kind::Start(driver),
                },
                Err(e) => {
                    say(Fd::Stderr, &format!("drt start: {e}\n"));
                    Session::exited(1)
                }
            },
            Command::Repl => {
                match Repl::new(
                    Arc::new(dispatcher),
                    config::ceiling(&config),
                    config.root.budget,
                ) {
                    Ok(repl) => {
                        say(Fd::Stderr, "drt repl — ^D to leave\n");
                        Session {
                            kind: Kind::Repl(repl),
                        }
                    }
                    Err(e) => {
                        say(Fd::Stderr, &format!("drt repl: {e}\n"));
                        Session::exited(1)
                    }
                }
            }
            Command::Buildinfo { json } => {
                say(Fd::Stdout, &cli::buildinfo(json));
                Session::exited(0)
            }
            Command::Ps => {
                say(
                    Fd::Stderr,
                    "drt ps: not built yet — it reaches a running deployment over the \
                     control endpoint, which lands with sshd (SPEC.md §13a)\n",
                );
                Session::exited(1)
            }
            // The verbs behind native-only features (`relay`, `stun`,
            // `tunnel`, `netcheck`): absent from a browser build of `drt`,
            // and present when this crate is compiled natively against a
            // fuller one, where they still have no loop to run in here.
            #[allow(unreachable_patterns)]
            _ => {
                say(
                    Fd::Stderr,
                    "drt: that verb needs sockets and a runtime of its own, which a terminal \
                     session does not have\n",
                );
                Session::exited(1)
            }
        }
    }
}

/// One command, ticked to completion.
pub struct Session {
    kind: Kind,
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
    fn exited(status: i32) -> Self {
        Session {
            kind: Kind::Exited(status),
        }
    }

    /// Advance, and say what the page does next. Over sessions answer
    /// [`Step::Exit`] again on every tick.
    pub fn tick(&mut self) -> Step {
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
        if let Kind::Repl(repl) = &mut self.kind {
            repl.abandon();
        }
    }
}

fn say(fd: Fd, text: &str) {
    let _ = stdio::Stream(fd).write_all(text.as_bytes());
}

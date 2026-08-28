//! The `drt` binary: `run | start | repl | ps` (SPEC.md §13a settles the
//! command surface — one process per deployment, client commands reaching a
//! running one over its control endpoint, no daemon and no registry).
//!
//! `run` is real: one program, driven to completion with the hostcall pump
//! (see `run.rs`). `start` is real: the deployment — root program plus its
//! swarm — foreground, with park timeouts honoured on the host clock (see
//! `start.rs`); listeners are refused until `listen` lands. `repl` and `ps`
//! are honest stubs until the control endpoint exists. `wire_connectors` is
//! where a profile's feature gates meet the root config, once, for every
//! subcommand.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use drt::{config, repl, run, start};
use drt_config::RootConfig;
use drt_connector::Registry;

#[derive(Parser)]
#[command(name = "drt", version, about = "The Diluvium RunTime")]
struct Cli {
    /// Root config file. Flags and env merge over it into one root object.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one program to completion: config + one program is a complete
    /// deployment.
    Run {
        /// A `.dlua` or `.lua` file. Optional when the config names one.
        program: Option<PathBuf>,
    },
    /// Run the deployment: the root program, its swarm, and whatever
    /// listeners the config names. Foreground; a process supervisor
    /// backgrounds it, and there is deliberately no --detach.
    Start,
    /// A REPL is an instance, not a mode: a sealed guest with a generous
    /// local grant, bridged to this terminal.
    Repl,
    /// The introspection surface: instances, caps, budgets, usage, health.
    Ps,
    /// The rendezvous relay: parked WSS legs paired by label and spliced.
    /// Reads the `relay` block of the config; runs foreground. Inside
    /// `drt start` the same relay also reports presence and bytes to the
    /// root program, and can be asked before it admits a leg.
    #[cfg(feature = "relay")]
    Relay,
    /// The STUN binding server: answer "what address did this datagram
    /// come from?", so a peer can learn its own reflexive address and try
    /// a direct path before falling back to the relay. Reads the `stun`
    /// block of the config; runs foreground. Inside `drt start` the same
    /// server also reports its counters to the root program.
    ///
    /// Run two on separate addresses (the `stun1`/`stun2` pair): one
    /// server reports an address, two report whether it *changed* between
    /// vantage points, which is the fact that decides whether hole
    /// punching can work at all.
    #[cfg(feature = "stun")]
    Stun,
    /// SSH over WSS, as a dumb pipe. With a URL: bridge this process's
    /// stdio to it — the OpenSSH ProxyCommand contract, so
    /// `ssh -o ProxyCommand="drt tunnel wss://gate/fp" user@fp` (and rsync,
    /// sftp, -L/-R through it) works like normal SSH over the WebSocket
    /// carrier. With --listen/--to: accept WebSocket connections and bridge
    /// each to a TCP target, in front of any sshd. With --park/--to: the
    /// device side of the relay.
    #[cfg(feature = "tunnel")]
    Tunnel {
        /// The wss:// or ws:// URL to bridge stdio to.
        url: Option<String>,
        /// Serve the other half: accept WebSockets here…
        #[arg(long, requires = "to", conflicts_with_all = ["url", "park"])]
        listen: Option<String>,
        /// …and bridge each connection to this host:port (used by both
        /// --listen and --park).
        #[arg(long)]
        to: Option<String>,
        /// The device side of the rendezvous relay: hold a parked leg at
        /// this /park URL; when a caller claims it, dial --to lazily and
        /// splice, re-parking a fresh leg immediately. Reconnects forever.
        #[arg(long, requires = "to", conflicts_with = "url")]
        park: Option<String>,
    },
}

/// Wire the connectors this build carries against the root config. Off by
/// default, all of them: only what the config names gets wired.
#[cfg_attr(
    not(any(
        feature = "connector-time",
        feature = "connector-ssh",
        feature = "connector-fs",
        feature = "connector-sql",
        feature = "connector-crypto"
    )),
    allow(unused_mut, unused_variables)
)]
fn wire_connectors(config: &RootConfig) -> Result<Registry, String> {
    let mut registry = Registry::new();
    for (name, wiring) in &config.connectors {
        match name.as_str() {
            #[cfg(feature = "connector-time")]
            "time" => registry
                .wire(
                    "time",
                    std::sync::Arc::new(drt_connector_time::TimeConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // Wired only when the config names it, and only ever to the
            // directory the config grants: the program names files inside
            // that, and nothing wires a default place on its behalf.
            #[cfg(feature = "connector-fs")]
            "fs" => registry
                .wire(
                    "fs",
                    std::sync::Arc::new(drt_connector_fs::FsConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // Same scope discipline as fs: the config grants a directory,
            // the program names its databases inside it.
            #[cfg(feature = "connector-sql")]
            "sql" => registry
                .wire(
                    "sql",
                    std::sync::Arc::new(drt_connector_sql::SqlConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // The one scope whose contents deliberately never reach the
            // program it serves: the key stays in this process, and a
            // grant of `host:crypto/jwt_sign` is the right to ask for a
            // signature, not the key.
            #[cfg(feature = "connector-crypto")]
            "crypto" => registry
                .wire(
                    "crypto",
                    std::sync::Arc::new(drt_connector_crypto::CryptoConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // Not in `run`'s local defaults: `ssh/exec` needs a tokio
            // reactor (`start` brings one) and a deliberate grant.
            #[cfg(feature = "connector-ssh")]
            "ssh" => registry
                .wire(
                    "ssh",
                    std::sync::Arc::new(drt_connector_ssh::SshConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            other => {
                return Err(format!(
                    "config wires connector '{other}', which this build does not carry"
                ))
            }
        }
    }
    Ok(registry)
}

/// With no config file, a local run still gets the connectors this build
/// carries that need no scope of their own — the zero-ceremony case. `fs`
/// is not among them on purpose: it has no default place, and inventing one
/// on the program's behalf is the wrong the scope model exists to fix.
fn local_defaults(config: &mut RootConfig) {
    if cfg!(feature = "connector-time") {
        config.connectors.insert("time".into(), Default::default());
    }
}

fn assemble(cli: &Cli) -> Result<(RootConfig, drt_connector::Dispatcher), String> {
    let mut config = config::load(cli.config.as_deref())?;
    if cli.config.is_none() {
        local_defaults(&mut config);
    }
    let registry = wire_connectors(&config)?;
    // By name, at startup: never a mystifying `denied` at first call.
    config::validate_grants(&config, &registry)?;
    Ok((config, drt_connector::Dispatcher::new(registry)))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (config, dispatcher) = match assemble(&cli) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("drt: {e}");
            return ExitCode::FAILURE;
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
                eprintln!("drt run: name a program, as an argument or as `program` in the config");
                return ExitCode::FAILURE;
            };
            match run::run(
                &path,
                &dispatcher,
                config::ceiling(&config),
                config.root.budget,
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("drt run: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Start => match start::start(&config, dispatcher) {
            // Ok means the swarm drained: every instance exited. For a
            // server-shaped deployment that never happens and foreground-
            // forever is the contract; for a batch-shaped one this is the
            // finish line.
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("drt start: {e}");
                ExitCode::FAILURE
            }
        },
        #[cfg(feature = "relay")]
        Command::Relay => {
            let Some(relay_config) = config.relay.clone() else {
                eprintln!("drt relay: the config names no `relay` block");
                return ExitCode::FAILURE;
            };
            let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
            match runtime.block_on(drt::relay::serve(drt::relay::Relay::new(relay_config))) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("drt relay: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(feature = "stun")]
        Command::Stun => {
            let Some(stun_config) = config.stun.clone() else {
                eprintln!("drt stun: the config names no `stun` block");
                return ExitCode::FAILURE;
            };
            let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
            match runtime.block_on(drt::stun::serve(&stun_config)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("drt stun: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(feature = "tunnel")]
        Command::Tunnel {
            url,
            listen,
            to,
            park,
        } => {
            let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
            let outcome = match (url, listen, park, to) {
                (Some(url), _, _, _) => runtime.block_on(drt::tunnel::stdio_to_ws(&url)),
                (None, Some(listen), None, Some(to)) => {
                    runtime.block_on(drt::tunnel::ws_to_tcp(&listen, &to))
                }
                (None, None, Some(park), Some(to)) => {
                    runtime.block_on(drt::tunnel::park(&park, &to))
                }
                _ => Err(
                    "name a URL to bridge stdio to, --listen with --to, or --park with --to".into(),
                ),
            };
            match outcome {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("drt tunnel: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Repl => {
            match repl::repl(&dispatcher, config::ceiling(&config), config.root.budget) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("drt repl: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Ps => {
            // Unlike the REPL, `ps` has nothing it can do standalone: its
            // whole subject is a deployment already running in another
            // process, which is the control endpoint's to reach.
            eprintln!(
                "drt ps: not built yet — it reaches a running deployment over the \
                 control endpoint, which lands with sshd (SPEC.md §13a)"
            );
            ExitCode::FAILURE
        }
    }
}

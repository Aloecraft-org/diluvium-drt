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
    /// What this binary is and what it carries: version, the dv ABI it
    /// speaks, its feature profile, and the connectors compiled into it.
    ///
    /// A binary that cannot say what it has cannot be checked against a
    /// package that declares what it needs. `--json` is the form a
    /// package manager reads; the release workflow uses it to fill
    /// BUILDINFO.txt from the artifact itself rather than from a guess
    /// made in YAML.
    Buildinfo {
        /// Machine-readable, for a package manager or a release job.
        #[arg(long)]
        json: bool,
    },
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
        feature = "connector-crypto",
        feature = "connector-rest"
    )),
    allow(unused_mut, unused_variables)
)]
/// What this binary carries, computed from the features it was actually
/// built with rather than declared anywhere.
///
/// The connector list is the load-bearing part: `full` and `slim` differ
/// precisely in their connector set, so a package that declares
/// `requires.connectors` can only be admitted or refused by name if the
/// binary will say what it has. The dv ABI numbers are the other half —
/// `library` is what the linked C core reports at runtime, `expected` is
/// what these bindings were built against, and a difference between them
/// is the mismatch `DiluviumEngine::new` refuses on.
fn buildinfo(json: bool) -> String {
    let mut connectors: Vec<&str> = Vec::new();
    if cfg!(feature = "connector-time") {
        connectors.push("time");
    }
    if cfg!(feature = "connector-fs") {
        connectors.push("fs");
    }
    if cfg!(feature = "connector-crypto") {
        connectors.push("crypto");
    }
    if cfg!(feature = "connector-sql") {
        connectors.push("sql");
    }
    if cfg!(feature = "connector-ssh") {
        connectors.push("ssh");
    }
    if cfg!(feature = "connector-rest") {
        connectors.push("rest");
    }
    if cfg!(feature = "listen") {
        connectors.push("listen");
    }

    let mut verbs: Vec<&str> = vec!["run", "start", "repl", "ps", "buildinfo"];
    if cfg!(feature = "relay") {
        verbs.push("relay");
    }
    if cfg!(feature = "stun") {
        verbs.push("stun");
    }
    if cfg!(feature = "tunnel") {
        verbs.push("tunnel");
    }
    verbs.sort_unstable();

    // Named by what the profile actually is, not by what was asked for: a
    // build with an unusual feature set is `custom`, and saying so is more
    // useful than calling it whichever named profile it resembles.
    let profile = match (
        cfg!(feature = "relay") && cfg!(feature = "stun") && cfg!(feature = "tunnel"),
        cfg!(feature = "connector-sql") && cfg!(feature = "connector-ssh"),
    ) {
        (true, true) => "full",
        (false, false) => "slim",
        _ => "custom",
    };

    // Asked of drt-swarm, which owns the engine feature — see the note on
    // `abi_versions` there. `null`/`unknown` is reported honestly rather
    // than as a zero a consumer would read as a real ABI number.
    let abi = drt_swarm::engine::abi_versions();

    if json {
        format!(
            "{{\"version\":\"{}\",\"profile\":\"{}\",\"dv_abi\":{},\
             \"dv_abi_expected\":{},\"connectors\":[{}],\"verbs\":[{}]}}\n",
            env!("CARGO_PKG_VERSION"),
            profile,
            abi.map_or("null".into(), |(l, _)| l.to_string()),
            abi.map_or("null".into(), |(_, e)| e.to_string()),
            connectors
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(","),
            verbs
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(","),
        )
    } else {
        format!(
            "version: {}\nprofile: {}\ndv_abi: {}\ndv_abi_expected: {}\n\
             connectors: {}\nverbs: {}\n",
            env!("CARGO_PKG_VERSION"),
            profile,
            abi.map_or("unknown".into(), |(l, _)| l.to_string()),
            abi.map_or("unknown".into(), |(_, e)| e.to_string()),
            connectors.join(","),
            verbs.join(","),
        )
    }
}

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
            // Like `ssh`, needs a reactor, and like `crypto`, its scope
            // carries secrets the guest must not see: an operator-injected
            // `authorization` lets a program call an authenticated API
            // without ever holding the credential.
            #[cfg(feature = "connector-rest")]
            "rest" => registry
                .wire(
                    "rest",
                    std::sync::Arc::new(drt_connector_rest::RestConnector::new()),
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
            let outcome = runtime.block_on(drt::relay::serve(drt::relay::Relay::new(relay_config)));
            // Leak the runtime rather than drop it. tokio 1.53.1 has a
            // use-after-free in runtime teardown — `BlockingPool::shutdown`
            // racing a worker's `park::Inner::unpark` into a freed Condvar
            // (backtrace in doc/Release.md) — and every one of these verbs
            // resolves a hostname through `lookup_host`, which is a
            // `spawn_blocking`, so there is always a parked blocking worker
            // to race. The process is exiting; the OS reclaims everything
            // drop would have. Leaking costs nothing and removes the whole
            // class from shipped code.
            std::mem::forget(runtime);
            match outcome {
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
            let outcome = runtime.block_on(drt::stun::serve(&stun_config));
            // Leak the runtime rather than drop it. tokio 1.53.1 has a
            // use-after-free in runtime teardown — `BlockingPool::shutdown`
            // racing a worker's `park::Inner::unpark` into a freed Condvar
            // (backtrace in doc/Release.md) — and every one of these verbs
            // resolves a hostname through `lookup_host`, which is a
            // `spawn_blocking`, so there is always a parked blocking worker
            // to race. The process is exiting; the OS reclaims everything
            // drop would have. Leaking costs nothing and removes the whole
            // class from shipped code.
            std::mem::forget(runtime);
            match outcome {
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
            // Leak the runtime rather than drop it. tokio 1.53.1 has a
            // use-after-free in runtime teardown — `BlockingPool::shutdown`
            // racing a worker's `park::Inner::unpark` into a freed Condvar
            // (backtrace in doc/Release.md) — and every one of these verbs
            // resolves a hostname through `lookup_host`, which is a
            // `spawn_blocking`, so there is always a parked blocking worker
            // to race. The process is exiting; the OS reclaims everything
            // drop would have. Leaking costs nothing and removes the whole
            // class from shipped code.
            std::mem::forget(runtime);
            match outcome {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("drt tunnel: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Buildinfo { json } => {
            print!("{}", buildinfo(json));
            ExitCode::SUCCESS
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

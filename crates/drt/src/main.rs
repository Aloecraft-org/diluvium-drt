//! The `drt` binary: `run | start | repl | ps` (SPEC.md §13a settles the
//! command surface — one process per deployment, client commands reaching a
//! running one over its control endpoint, no daemon and no registry).
//!
//! `run` is real: one program, driven to completion with the hostcall pump
//! (see `run.rs`). `start`, `repl` and `ps` are honest stubs until the
//! listener and the ego-proc adapters land. `wire_connectors` is where a
//! profile's feature gates meet the root config, once, for every subcommand.

mod run;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
        /// A `.dlua` or `.lua` file.
        program: PathBuf,
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
}

/// Wire the connectors this build carries against the root config. Off by
/// default, all of them: only what the config names gets wired.
#[cfg_attr(
    not(any(feature = "connector-time", feature = "connector-ssh")),
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

/// A local run's config until the file+flags+env merge lands: the slim
/// profile's promise (time today; fs and stdio to follow), wired explicitly
/// like any other deployment would.
fn local_config() -> RootConfig {
    let mut config = RootConfig::default();
    if cfg!(feature = "connector-time") {
        config.connectors.insert("time".into(), Default::default());
    }
    config
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(path) = &cli.config {
        eprintln!(
            "drt: --config {} is not read yet; using the local-run defaults",
            path.display()
        );
    }
    let config = local_config();
    let registry = match wire_connectors(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("drt: {e}");
            return ExitCode::FAILURE;
        }
    };
    let dispatcher = drt_connector::Dispatcher::new(registry);
    match cli.command {
        Command::Run { program } => match run::run(&program, &dispatcher) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("drt run: {e}");
                ExitCode::FAILURE
            }
        },
        other => {
            let what = match other {
                Command::Start => "start",
                Command::Repl => "repl",
                _ => "ps",
            };
            eprintln!(
                "drt {what}: not built yet — the swarm port and the ego-proc adapters are \
                 the next milestones (SPEC.md §§8–9)"
            );
            ExitCode::FAILURE
        }
    }
}

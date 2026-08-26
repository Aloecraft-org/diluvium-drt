//! The `drt` binary (SPEC.md §3): `run | serve | repl | ps`.
//!
//! Every subcommand is a stub until the Engine's first impl lands (blocked on
//! the `diluvium-sys` transcription upstream, SPEC.md §4) — but the stubs are
//! honest about it, and the config/registry plumbing they will share is
//! already the real thing: `wire_connectors` is where a profile's feature
//! gates meet the root config, once, for every subcommand.

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
    /// Run a deployment with transport listeners.
    Serve,
    /// A REPL is an instance, not a mode: a sealed guest with a generous
    /// local grant, bridged to this terminal.
    Repl,
    /// The introspection surface: instances, caps, budgets, usage, health.
    Ps,
}

/// Wire the connectors this build carries against the root config. Off by
/// default, all of them: only what the config names gets wired.
#[cfg_attr(not(feature = "connector-time"), allow(unused_mut, unused_variables))]
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
            other => {
                return Err(format!(
                    "config wires connector '{other}', which this build does not carry"
                ))
            }
        }
    }
    Ok(registry)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The merge (file + flags + env) lands with the first runnable engine;
    // until then an absent config is the empty root object.
    if let Some(path) = &cli.config {
        eprintln!(
            "drt: --config {} is not read yet; using the empty root object",
            path.display()
        );
    }
    let config = RootConfig::default();
    let _registry = match wire_connectors(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("drt: {e}");
            return ExitCode::FAILURE;
        }
    };
    let what = match cli.command {
        Command::Run { .. } => "run",
        Command::Serve => "serve",
        Command::Repl => "repl",
        Command::Ps => "ps",
    };
    eprintln!(
        "drt {what}: not yet runnable — the Engine's first impl is blocked on the \
         diluvium-sys transcription (SPEC.md §4)"
    );
    ExitCode::FAILURE
}

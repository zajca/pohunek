//! `zagentmesh` — the CLI control plane.
//!
//! Milestone 2 commands: `doctor`, `daemon start`, and `health`/`status`. The
//! grammar is host-aware now (a `--host` flag and `<host>/<session-id>` targets)
//! so Phase 2 adds remote transport without breaking the CLI surface, but only
//! the local form executes in Phase 1 (see `docs/plan-phase-1.md` "CLI Grammar").

#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]
#![forbid(unsafe_code)]

mod client;
mod commands;
mod error;
mod paths;
mod target;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::error::CliError;
use crate::paths::Paths;
use crate::target::{Target, LOCAL_HOST};

/// zagentmesh: durable coding-agent sessions across your own machines.
#[derive(Debug, Parser)]
#[command(name = "zagentmesh", version, about, long_about = None)]
struct Cli {
    /// Target host for the command. Phase 1 accepts only the local host; this
    /// flag is parsed now so Phase 2 can add remote transport without changing
    /// the CLI surface.
    #[arg(long, global = true, default_value = LOCAL_HOST)]
    host: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Check environment health (binaries, socket/state dir writability).
    Doctor {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Manage the host daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Query daemon health over the control socket.
    Health {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Alias for `health`: show daemon status.
    Status {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Start the host daemon (foreground by default).
    Start {
        /// Run the daemon in the background instead of the foreground.
        #[arg(long)]
        detach: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("zagentmesh: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, CliError> {
    // Reject remote targets early: parsing is host-aware, execution is local.
    ensure_local_host(&cli.host)?;

    match cli.command {
        Commands::Doctor { json } => {
            let paths = Paths::resolve()?;
            let healthy = commands::doctor::run(&paths, json)?;
            Ok(if healthy {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Commands::Daemon { action } => match action {
            DaemonAction::Start { detach } => {
                commands::daemon::start(detach)?;
                Ok(ExitCode::SUCCESS)
            }
        },
        Commands::Health { json } | Commands::Status { json } => {
            let paths = Paths::resolve()?;
            commands::health::run(&paths, json).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Ensure the requested host is local; reject remote targets until Phase 2.
///
/// The `--host` flag carries a host name. We normalize it through the host-aware
/// [`Target`] grammar so the same parsing rules apply everywhere: a bare value
/// is the host name, and an accidental `host/session` slip is interpreted
/// consistently. Remote hosts parse fine but are not yet executable.
fn ensure_local_host(host: &str) -> Result<(), CliError> {
    // A bare `--host foo` parses as a session id with no host; for the flag's
    // purpose the bare value *is* the host name. Build a Target accordingly so
    // `is_local` / `host_or_local` carry the host-resolution logic in one place.
    let target = match host.parse::<Target>() {
        Ok(t) if t.host.is_some() => t,
        // Bare value (or parse error): treat the raw string as the host name.
        _ => Target {
            host: Some(host.to_owned()),
            session_id: String::new(),
        },
    };

    if target.is_local() {
        Ok(())
    } else {
        Err(CliError::RemoteNotSupported {
            host: target.host_or_local().to_owned(),
        })
    }
}

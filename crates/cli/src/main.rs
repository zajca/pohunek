//! `zagentmesh` — the CLI control plane.
//!
//! Milestone 2 commands: `doctor`, `daemon start`, and `health`/`status`. The
//! grammar is host-aware now (a `--host` flag and `<host>/<session-id>` targets)
//! so Phase 2 adds remote transport without breaking the CLI surface, but only
//! the local form executes in Phase 1 (see `docs/plan-phase-1.md` "CLI Grammar").

#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]
#![deny(unsafe_code)]

mod client;
mod commands;
mod error;
mod paths;
mod target;

use std::path::PathBuf;
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
    /// Attach this terminal to a local session. Press Ctrl-] to detach.
    Attach {
        /// Session target: `session-id` or `local/session-id`.
        target: Target,
    },

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

    /// Manage local PTY-backed sessions.
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },

    /// Manage agent integrations (session-id capture hooks).
    Integration {
        #[command(subcommand)]
        action: IntegrationAction,
    },
}

#[derive(Debug, Subcommand)]
enum IntegrationAction {
    /// Install the `SessionStart` hook that captures native session ids for
    /// resume. Without `--agent`, installs for every supported agent present.
    Install {
        /// Restrict installation to a single agent.
        #[arg(long, value_enum)]
        agent: Option<commands::integration::HookAgentArg>,
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

#[derive(Debug, Subcommand)]
enum SessionAction {
    /// Start a new session.
    New {
        /// Agent kind to start.
        #[arg(long, value_enum, default_value = "shell")]
        agent: commands::session::AgentArg,
        /// Working directory for the session.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Initial terminal width in columns.
        #[arg(long, default_value_t = 80)]
        cols: u16,
        /// Initial terminal height in rows.
        #[arg(long, default_value_t = 24)]
        rows: u16,
        /// Git repository to bind a dedicated worktree for. Requires `--branch`.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Branch to check out in the bound worktree. Requires `--repo`.
        #[arg(long)]
        branch: Option<String>,
        /// Base branch the worktree's branch is created from. Requires
        /// `--repo` and `--branch`; falls back to the repository's default
        /// branch when missing.
        #[arg(long)]
        base_branch: Option<String>,
    },

    /// List known sessions.
    List {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Inspect one session.
    Inspect {
        /// Session target: `session-id` or `local/session-id`.
        target: Target,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Stop one session.
    Stop {
        /// Session target: `session-id` or `local/session-id`.
        target: Target,
    },

    /// Send text to one session.
    Input {
        /// Session target: `session-id` or `local/session-id`.
        target: Target,
        /// Text to inject into the session.
        text: String,
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
        Commands::Attach { target } => {
            let paths = Paths::resolve()?;
            commands::attach::run_attach(&paths, &target).await?;
            Ok(ExitCode::SUCCESS)
        }
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
        Commands::Session { action } => {
            let paths = Paths::resolve()?;
            match action {
                SessionAction::New {
                    agent,
                    cwd,
                    cols,
                    rows,
                    repo,
                    branch,
                    base_branch,
                } => {
                    commands::session::run_new(
                        &paths,
                        commands::session::NewArgs {
                            agent,
                            cwd,
                            cols,
                            rows,
                            repo,
                            branch,
                            base_branch,
                        },
                    )
                    .await?
                }
                SessionAction::List { json } => commands::session::run_list(&paths, json).await?,
                SessionAction::Inspect { target, json } => {
                    commands::session::run_inspect(&paths, &target, json).await?
                }
                SessionAction::Stop { target } => {
                    commands::session::run_stop(&paths, &target).await?
                }
                SessionAction::Input { target, text } => {
                    commands::session::run_input(&paths, &target, &text).await?
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Integration { action } => {
            let paths = Paths::resolve()?;
            match action {
                IntegrationAction::Install { agent } => {
                    commands::integration::run_install(&paths, agent).await?;
                }
            }
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_session_new_defaults() {
        let cli = Cli::try_parse_from(["zagentmesh", "session", "new"]).expect("parse");

        match cli.command {
            Commands::Session {
                action:
                    SessionAction::New {
                        agent,
                        cwd,
                        cols,
                        rows,
                        repo,
                        branch,
                        base_branch,
                    },
            } => {
                assert_eq!(agent, commands::session::AgentArg::Shell);
                assert_eq!(cwd, None);
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
                assert_eq!(repo, None);
                assert_eq!(branch, None);
                assert_eq!(base_branch, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_new_codex_agent() {
        let cli = Cli::try_parse_from(["zagentmesh", "session", "new", "--agent", "codex"])
            .expect("parse");

        match cli.command {
            Commands::Session {
                action: SessionAction::New { agent, .. },
            } => {
                assert_eq!(agent, commands::session::AgentArg::Codex);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_new_claude_agent() {
        let cli = Cli::try_parse_from(["zagentmesh", "session", "new", "--agent", "claude"])
            .expect("parse");

        match cli.command {
            Commands::Session {
                action: SessionAction::New { agent, .. },
            } => {
                assert_eq!(agent, commands::session::AgentArg::Claude);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_new_worktree_flags() {
        let cli = Cli::try_parse_from([
            "zagentmesh",
            "session",
            "new",
            "--agent",
            "claude",
            "--repo",
            "/workspace/project",
            "--branch",
            "feature/login",
            "--base-branch",
            "main",
        ])
        .expect("parse");

        match cli.command {
            Commands::Session {
                action:
                    SessionAction::New {
                        agent,
                        repo,
                        branch,
                        base_branch,
                        ..
                    },
            } => {
                assert_eq!(agent, commands::session::AgentArg::Claude);
                assert_eq!(repo.as_deref(), Some(std::path::Path::new("/workspace/project")));
                assert_eq!(branch.as_deref(), Some("feature/login"));
                assert_eq!(base_branch.as_deref(), Some("main"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_input_target_and_text() {
        let cli = Cli::try_parse_from([
            "zagentmesh",
            "session",
            "input",
            "local/s-42",
            "write tests first",
        ])
        .expect("parse");

        match cli.command {
            Commands::Session {
                action: SessionAction::Input { target, text },
            } => {
                assert_eq!(target.session_id, "s-42");
                assert_eq!(target.host.as_deref(), Some("local"));
                assert_eq!(text, "write tests first");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_inspect_target_and_json_flag() {
        let cli = Cli::try_parse_from(["zagentmesh", "session", "inspect", "local/s-42", "--json"])
            .expect("parse");

        match cli.command {
            Commands::Session {
                action: SessionAction::Inspect { target, json },
            } => {
                assert_eq!(target.session_id, "s-42");
                assert_eq!(target.host.as_deref(), Some("local"));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_attach_bare_target() {
        let cli = Cli::try_parse_from(["zagentmesh", "attach", "s-42"]).expect("parse");

        match cli.command {
            Commands::Attach { target } => {
                assert_eq!(target.session_id, "s-42");
                assert_eq!(target.host, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_attach_explicit_local_target() {
        let cli = Cli::try_parse_from(["zagentmesh", "attach", "local/s-42"]).expect("parse");

        match cli.command {
            Commands::Attach { target } => {
                assert_eq!(target.session_id, "s-42");
                assert_eq!(target.host.as_deref(), Some("local"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}

//! `pohunek` — the CLI control plane.
//!
//! Commands: `doctor`, `daemon start`, `health`/`status`, `session`, `attach`,
//! `integration`, and `host` (discover/list/inspect). The grammar is host-aware
//! (a `--host` flag and `<host>/<session-id>` targets); the *effective host*
//! selects the transport, so local and remote (over NetBird) execute through one
//! surface. Local behavior is unchanged from the local-only phase.

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
use protocol::{method, Request};

use crate::error::CliError;
use crate::paths::Paths;
use crate::target::{Target, LOCAL_HOST};

/// pohunek: durable coding-agent sessions across your own machines.
#[derive(Debug, Parser)]
#[command(name = "pohunek", version, about, long_about = None)]
struct Cli {
    /// Target host for the command. `local` (the default) uses this machine; any
    /// other name is resolved to a NetBird peer and dialed over the mesh. A
    /// `<host>/<session-id>` target's host overrides this flag for that command.
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

    /// Stream daemon events as newline-delimited JSON.
    #[command(hide = true)]
    Subscribe {
        /// Emit machine-readable JSON event lines. The event stream is already
        /// newline-delimited JSON, so this does not change the streamed output;
        /// it is accepted for forward-compat and to keep error rendering in JSON
        /// (via `wants_json`) consistent with the other commands.
        #[arg(long)]
        json: bool,
    },

    /// Manage agent integrations (session-id capture hooks).
    Integration {
        #[command(subcommand)]
        action: IntegrationAction,
    },

    /// Set up the sway/rofi launcher integration on this machine.
    ///
    /// With no subcommand, runs the full setup (scripts + config + sway
    /// drop-in). Subcommands apply one part at a time. All operations are
    /// local filesystem writes; `--host` is ignored.
    Setup {
        #[command(subcommand)]
        action: Option<SetupAction>,
        /// Emit machine-readable JSON instead of human text (bare `setup`).
        #[arg(long)]
        json: bool,
    },

    /// Discover, list, and inspect remote hosts over NetBird.
    Host {
        #[command(subcommand)]
        action: HostAction,
    },
}

#[derive(Debug, Subcommand)]
enum HostAction {
    /// Enumerate NetBird peers and probe their daemons.
    Discover {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// List known hosts (live NetBird peers) with their classification.
    List {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Inspect one host's live capabilities (a direct daemon query).
    Inspect {
        /// Host name to inspect (a NetBird peer name, or `local`).
        host: String,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
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
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SetupAction {
    /// Materialize the launcher scripts into the data dir's `bin/`.
    Scripts {
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Write a default `launcher.conf` and prompt templates (never overwrites
    /// existing files unless `--force`).
    Config {
        /// Overwrite existing config files instead of skipping them.
        #[arg(long)]
        force: bool,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Write (or print) the sway drop-in that binds a key to the launcher.
    Sway {
        /// Print the snippet to stdout instead of writing the drop-in file.
        #[arg(long)]
        print: bool,
        /// Sway keybind to bind the launcher to.
        #[arg(long, default_value = "$mod+p")]
        keybind: String,
        /// Emit machine-readable JSON instead of human text.
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
        /// Initial text to inject into the session after the PTY is spawned.
        #[arg(long)]
        input: Option<String>,
        /// Skip the confirmation prompt when starting a session on a remote
        /// host. Required on the `--json` path for a remote host (the machine
        /// path must not block on a prompt). Ignored for local sessions.
        #[arg(long)]
        yes: bool,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// List known sessions.
    List {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Emit only session ids, one per line.
        #[arg(short = 'q', long = "quiet", conflicts_with = "json")]
        quiet: bool,
        /// Exact-match filter in key=value form. May be repeated and filters
        /// are ANDed. Supported keys: state, activity, agent, id.
        #[arg(long = "filter", value_name = "key=value", value_parser = commands::session::parse_list_filter)]
        filters: Vec<commands::session::ListFilter>,
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
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Send text to one session.
    Input {
        /// Session target: `session-id` or `local/session-id`.
        target: Target,
        /// Text to inject into the session.
        text: String,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
}

impl Commands {
    /// Whether the active command requested machine-readable `--json` output.
    ///
    /// Lets the top-level error sink render a failure in the same mode the
    /// command would have used on success. `attach` (raw stream) and `daemon
    /// start` (process control) have no `--json` and always report `false`.
    fn wants_json(&self) -> bool {
        match self {
            Commands::Doctor { json } | Commands::Health { json } | Commands::Status { json } => {
                *json
            }
            Commands::Session { action } => action.wants_json(),
            Commands::Integration { action } => action.wants_json(),
            Commands::Setup { action, json } => action.as_ref().map_or(*json, SetupAction::wants_json),
            Commands::Host { action } => action.wants_json(),
            Commands::Subscribe { json } => *json,
            Commands::Attach { .. } | Commands::Daemon { .. } => false,
        }
    }
}

impl SetupAction {
    fn wants_json(&self) -> bool {
        match self {
            SetupAction::Scripts { json }
            | SetupAction::Config { json, .. }
            | SetupAction::Sway { json, .. } => *json,
        }
    }
}

impl HostAction {
    fn wants_json(&self) -> bool {
        match self {
            HostAction::Discover { json }
            | HostAction::List { json }
            | HostAction::Inspect { json, .. } => *json,
        }
    }
}

impl SessionAction {
    fn wants_json(&self) -> bool {
        match self {
            SessionAction::New { json, .. }
            | SessionAction::List { json, .. }
            | SessionAction::Inspect { json, .. }
            | SessionAction::Stop { json, .. }
            | SessionAction::Input { json, .. } => *json,
        }
    }
}

impl IntegrationAction {
    fn wants_json(&self) -> bool {
        match self {
            IntegrationAction::Install { json, .. } => *json,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // Parse manually (not `Cli::parse`) so a clap usage error can be rendered as a
    // structured `--json` document instead of clap's human text + hard process
    // exit. We keep the raw argv to recover the `--json` intent: parsing fails
    // before a typed `Cli` exists, so `wants_json` is unavailable on that path.
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(err) => return error::render_clap_error(err, error::args_request_json(&args)),
    };
    // Capture whether the active command requested `--json` before `run` consumes
    // `cli`, so a failure is rendered in the same mode a success would have been.
    let json = cli.command.wants_json();
    match run(cli).await {
        Ok(code) => code,
        Err(err) => {
            error::render(&err, json);
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, CliError> {
    let global_host = cli.host;

    match cli.command {
        Commands::Attach { target } => {
            let paths = Paths::resolve()?;
            let host = effective_host(&global_host, Some(&target));
            commands::attach::run_attach(&host, &paths, &target).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Doctor { json } => {
            // Doctor is purely a local environment check; it ignores `--host`.
            let paths = Paths::resolve()?;
            let healthy = commands::doctor::run(&paths, json)?;
            Ok(if healthy {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Commands::Daemon { action } => match action {
            // Starting the daemon is inherently local (this machine's process).
            DaemonAction::Start { detach } => {
                commands::daemon::start(detach)?;
                Ok(ExitCode::SUCCESS)
            }
        },
        Commands::Health { json } | Commands::Status { json } => {
            let paths = Paths::resolve()?;
            let host = effective_host(&global_host, None);
            commands::health::run(&host, &paths, json).await?;
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
                    input,
                    yes,
                    json,
                } => {
                    let host = effective_host(&global_host, None);
                    commands::session::run_new(
                        &host,
                        &paths,
                        commands::session::NewArgs {
                            agent,
                            cwd,
                            cols,
                            rows,
                            repo,
                            branch,
                            base_branch,
                            input,
                        },
                        json,
                        yes,
                    )
                    .await?
                }
                SessionAction::List {
                    json,
                    quiet,
                    filters,
                } => {
                    let host = effective_host(&global_host, None);
                    let output_mode = if quiet {
                        commands::session::ListOutputMode::Quiet
                    } else if json {
                        commands::session::ListOutputMode::Json
                    } else {
                        commands::session::ListOutputMode::Human
                    };
                    commands::session::run_list(&host, &paths, &filters, output_mode).await?
                }
                SessionAction::Inspect { target, json } => {
                    let host = effective_host(&global_host, Some(&target));
                    commands::session::run_inspect(&host, &paths, &target, json).await?
                }
                SessionAction::Stop { target, json } => {
                    let host = effective_host(&global_host, Some(&target));
                    commands::session::run_stop(&host, &paths, &target, json).await?
                }
                SessionAction::Input { target, text, json } => {
                    let host = effective_host(&global_host, Some(&target));
                    commands::session::run_input(&host, &paths, &target, &text, json).await?
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Subscribe { json: _ } => {
            // `json` is intentionally not read here: the daemon's event stream is
            // already NDJSON, so success output is the same with or without the
            // flag. It still governs error rendering through `wants_json` above.
            let paths = Paths::resolve()?;
            let host = effective_host(&global_host, None);
            let mut client = crate::client::Client::connect(&host, &paths).await?;
            let request = Request::new(
                commands::request_id(method::SUBSCRIBE),
                method::SUBSCRIBE,
                serde_json::Value::Null,
            );
            client.subscribe(&request).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Integration { action } => {
            // Installing hooks is a local daemon op (writes this machine's agent
            // config); the command stays local regardless of `--host`.
            let paths = Paths::resolve()?;
            match action {
                IntegrationAction::Install { agent, json } => {
                    commands::integration::run_install(&paths, agent, json).await?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Setup { action, json } => {
            // Setup is purely local: it writes this machine's scripts, config,
            // and sway drop-in. It ignores `--host`.
            let paths = Paths::resolve()?;
            match action {
                None => commands::setup::run_all(&paths, json)?,
                Some(SetupAction::Scripts { json }) => {
                    commands::setup::run_scripts(&paths, json)?;
                }
                Some(SetupAction::Config { force, json }) => {
                    commands::setup::run_config(&paths, force, json)?;
                }
                Some(SetupAction::Sway {
                    print,
                    keybind,
                    json,
                }) => {
                    commands::setup::run_sway(&paths, print, &keybind, json)?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Host { action } => match action {
            // discover/list enumerate the local host's mesh view via the local
            // daemon (which caches the probe); they ignore `--host`.
            HostAction::Discover { json } => {
                let paths = Paths::resolve()?;
                commands::host::run_discover(&paths, json).await?;
                Ok(ExitCode::SUCCESS)
            }
            HostAction::List { json } => {
                let paths = Paths::resolve()?;
                commands::host::run_list(&paths, json).await?;
                Ok(ExitCode::SUCCESS)
            }
            HostAction::Inspect { host, json } => {
                // `inspect` uses its positional host arg, not the global flag.
                let paths = Paths::resolve()?;
                commands::host::run_inspect(&host, &paths, json).await?;
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}

/// Resolve the effective host for a command.
///
/// A positional [`Target`]'s host (when present) wins over the global `--host`
/// flag; otherwise the global flag is used. `None` (no target) means "use the
/// global flag" (commands that take only the flag). The returned string is the
/// host name the transport selects on (`local`, or a NetBird peer name).
#[must_use]
fn effective_host(global: &str, target: Option<&Target>) -> String {
    match target.and_then(|t| t.host.as_deref()) {
        Some(host) => host.to_owned(),
        None => global.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_session_new_defaults() {
        let cli = Cli::try_parse_from(["pohunek", "session", "new"]).expect("parse");

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
                        input,
                        yes,
                        json,
                    },
            } => {
                assert_eq!(agent, commands::session::AgentArg::Shell);
                assert_eq!(cwd, None);
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
                assert_eq!(repo, None);
                assert_eq!(branch, None);
                assert_eq!(base_branch, None);
                assert_eq!(input, None);
                assert!(!yes, "yes defaults to false");
                assert!(!json, "json defaults to false");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_new_codex_agent() {
        let cli =
            Cli::try_parse_from(["pohunek", "session", "new", "--agent", "codex"]).expect("parse");

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
        let cli =
            Cli::try_parse_from(["pohunek", "session", "new", "--agent", "claude"]).expect("parse");

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
            "pohunek",
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
                        input,
                        ..
                    },
            } => {
                assert_eq!(agent, commands::session::AgentArg::Claude);
                assert_eq!(
                    repo.as_deref(),
                    Some(std::path::Path::new("/workspace/project"))
                );
                assert_eq!(branch.as_deref(), Some("feature/login"));
                assert_eq!(base_branch.as_deref(), Some("main"));
                assert_eq!(input, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_new_initial_input() {
        let cli = Cli::try_parse_from(["pohunek", "session", "new", "--input", "Fix #1234"])
            .expect("parse");

        match cli.command {
            Commands::Session {
                action: SessionAction::New { input, .. },
            } => assert_eq!(input.as_deref(), Some("Fix #1234")),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_hidden_subscribe_json() {
        let cli = Cli::try_parse_from(["pohunek", "subscribe", "--json"]).expect("parse");

        match cli.command {
            Commands::Subscribe { json } => assert!(json),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_input_target_and_text() {
        let cli = Cli::try_parse_from([
            "pohunek",
            "session",
            "input",
            "local/s-42",
            "write tests first",
        ])
        .expect("parse");

        match cli.command {
            Commands::Session {
                action: SessionAction::Input { target, text, json },
            } => {
                assert_eq!(target.session_id, "s-42");
                assert_eq!(target.host.as_deref(), Some("local"));
                assert_eq!(text, "write tests first");
                assert!(!json, "json defaults to false");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_inspect_target_and_json_flag() {
        let cli = Cli::try_parse_from(["pohunek", "session", "inspect", "local/s-42", "--json"])
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
    fn parses_session_list_repeatable_filters_and_quiet() {
        let cli = Cli::try_parse_from([
            "pohunek",
            "session",
            "list",
            "--filter",
            "state=running",
            "--filter",
            "agent=codex",
            "-q",
        ])
        .expect("parse");

        match cli.command {
            Commands::Session {
                action:
                    SessionAction::List {
                        json,
                        quiet,
                        filters,
                    },
            } => {
                assert!(!json);
                assert!(quiet);
                assert_eq!(
                    filters,
                    vec![
                        commands::session::parse_list_filter("state=running").expect("state"),
                        commands::session::parse_list_filter("agent=codex").expect("agent"),
                    ]
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_session_list_json_and_quiet_together() {
        let err = Cli::try_parse_from(["pohunek", "session", "list", "--json", "-q"])
            .expect_err("json and quiet conflict");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parses_attach_bare_target() {
        let cli = Cli::try_parse_from(["pohunek", "attach", "s-42"]).expect("parse");

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
        let cli = Cli::try_parse_from(["pohunek", "attach", "local/s-42"]).expect("parse");

        match cli.command {
            Commands::Attach { target } => {
                assert_eq!(target.session_id, "s-42");
                assert_eq!(target.host.as_deref(), Some("local"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    // --- setup ----------------------------------------------------------------

    #[test]
    fn parses_bare_setup_as_no_action() {
        let cli = Cli::try_parse_from(["pohunek", "setup"]).expect("parse");

        match cli.command {
            Commands::Setup { action, json } => {
                assert!(action.is_none(), "bare setup has no subcommand");
                assert!(!json, "json defaults to false");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_setup_config_force() {
        let cli = Cli::try_parse_from(["pohunek", "setup", "config", "--force"]).expect("parse");

        match cli.command {
            Commands::Setup {
                action: Some(SetupAction::Config { force, json }),
                ..
            } => {
                assert!(force);
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_setup_sway_keybind_and_print() {
        let cli =
            Cli::try_parse_from(["pohunek", "setup", "sway", "--print", "--keybind", "$mod+a"])
                .expect("parse");

        match cli.command {
            Commands::Setup {
                action: Some(SetupAction::Sway {
                    print,
                    keybind,
                    json,
                }),
                ..
            } => {
                assert!(print);
                assert_eq!(keybind, "$mod+a");
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn setup_sway_keybind_defaults_to_mod_p() {
        let cli = Cli::try_parse_from(["pohunek", "setup", "sway"]).expect("parse");

        match cli.command {
            Commands::Setup {
                action: Some(SetupAction::Sway { keybind, .. }),
                ..
            } => assert_eq!(keybind, "$mod+p"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    // --- effective-host routing -----------------------------------------------

    fn target(host: Option<&str>, id: &str) -> Target {
        Target {
            host: host.map(str::to_owned),
            session_id: id.to_owned(),
        }
    }

    #[test]
    fn effective_host_uses_global_when_no_target() {
        assert_eq!(effective_host(LOCAL_HOST, None), "local");
        assert_eq!(effective_host("host-b", None), "host-b");
    }

    #[test]
    fn effective_host_target_host_wins_over_global() {
        // A `<host>/<id>` target's host overrides the global `--host` flag.
        assert_eq!(
            effective_host("host-b", Some(&target(Some("host-c"), "s-1"))),
            "host-c"
        );
        // An explicit `local/` target forces local even with a remote global.
        assert_eq!(
            effective_host("host-b", Some(&target(Some("local"), "s-1"))),
            "local"
        );
    }

    #[test]
    fn effective_host_bare_target_falls_back_to_global() {
        // A bare `s-1` target (no host) falls back to the global flag.
        assert_eq!(
            effective_host("host-b", Some(&target(None, "s-1"))),
            "host-b"
        );
        assert_eq!(
            effective_host(LOCAL_HOST, Some(&target(None, "s-1"))),
            "local"
        );
    }

    #[test]
    fn parses_session_new_yes_flag() {
        let cli = Cli::try_parse_from(["pohunek", "--host", "host-b", "session", "new", "--yes"])
            .expect("parse");
        match cli.command {
            Commands::Session {
                action: SessionAction::New { yes, .. },
            } => assert!(yes, "--yes sets the flag"),
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(cli.host, "host-b");
    }

    #[test]
    fn parses_host_inspect_with_positional_host() {
        let cli =
            Cli::try_parse_from(["pohunek", "host", "inspect", "host-b", "--json"]).expect("parse");
        match cli.command {
            Commands::Host {
                action: HostAction::Inspect { host, json },
            } => {
                assert_eq!(host, "host-b");
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_host_discover_and_list() {
        let discover =
            Cli::try_parse_from(["pohunek", "host", "discover", "--json"]).expect("parse");
        assert!(matches!(
            discover.command,
            Commands::Host {
                action: HostAction::Discover { json: true }
            }
        ));
        let list = Cli::try_parse_from(["pohunek", "host", "list"]).expect("parse");
        assert!(matches!(
            list.command,
            Commands::Host {
                action: HostAction::List { json: false }
            }
        ));
    }
}

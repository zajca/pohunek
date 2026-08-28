//! `pohunek` — the CLI control plane.
//!
//! Commands: `doctor`, `daemon start`, `health`/`status`, `session`, `attach`,
//! `integration`, and `host` (discover/list/inspect). The grammar is host-aware
//! (a `--host` flag and `<host>/<session-id>` targets); the *effective host*
//! selects the transport, so local and remote (over `NetBird`) execute through one
//! surface. Local behavior is unchanged from the local-only phase.

#![deny(unsafe_code)]

mod client;
mod commands;
mod error;
mod hermes_integration;
mod paths;
mod target;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Command, CommandFactory, Parser, Subcommand};
use protocol::{method, Request};

use crate::error::CliError;
use crate::paths::Paths;
use crate::target::{Target, LOCAL_HOST};

/// pohunek: durable coding-agent sessions across your own machines.
#[derive(Debug, Parser)]
#[command(name = "pohunek", version, about, long_about = None)]
struct Cli {
    /// Target host for the command. `local` (the default) uses this machine; any
    /// other name is resolved to a `NetBird` peer and dialed over the mesh. A
    /// `<host>/<session-id>` target's host overrides this flag for that command.
    #[arg(long, global = true, default_value = LOCAL_HOST)]
    host: String,

    #[command(subcommand)]
    command: Commands,
}

/// Return the complete `pohunek` clap command tree.
#[must_use]
pub fn command() -> Command {
    Cli::command()
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Attach this terminal to a local or remote session. Press Ctrl-] to detach.
    Attach {
        /// Session target: `session-id` or `<host>/<session-id>`.
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

    /// Safely cross the one-time daemon-owned to worker-owned PTY boundary.
    Migration {
        #[command(subcommand)]
        action: MigrationAction,
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

    /// Discover, list, and inspect remote hosts over `NetBird`.
    Host {
        #[command(subcommand)]
        action: HostAction,
    },

    /// List, watch, update, and configure durable agent notifications.
    Notifications {
        #[command(subcommand)]
        action: NotificationsAction,
    },

    /// Render provider prompt templates locally.
    Prompt {
        #[command(subcommand)]
        action: PromptAction,
    },

    /// List, add, show, rename, and forget projects (git-repo awareness) on a host.
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },

    /// Launch the universal assistant: one capable agent session with a
    /// materialized knowledge bundle and a redacted live snapshot.
    ///
    /// With no subcommand the intent defaults to `help`; the intent wrappers
    /// (`setup`/`project`/`update`/`debug`/`help`) only steer navigation.
    #[command(disable_help_subcommand = true)]
    Assistant {
        #[command(subcommand)]
        action: Option<AssistantAction>,
        /// Intent for the default form (overridden by an intent subcommand).
        #[arg(long, value_enum)]
        intent: Option<commands::assistant::Intent>,
        #[command(flatten)]
        args: AssistantArgs,
    },
}

/// Options shared by the default `assistant` form and every intent wrapper.
#[derive(Debug, Clone, clap::Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "clap flag bag of independent boolean switches; an enum would not model them better"
)]
struct AssistantArgs {
    /// Free-form request for the assistant (joined into one line).
    request: Vec<String>,
    /// Override agent selection with a specific agent name or host profile.
    #[arg(long)]
    agent: Option<String>,
    /// Target project by `<id|label>`. Mutually exclusive with `--repo` (both
    /// name the target repository).
    #[arg(long, conflicts_with = "repo")]
    project: Option<String>,
    /// Repository to bind a worktree for. Mutually exclusive with `--project`.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Branch to check out in the bound worktree.
    #[arg(long)]
    branch: Option<String>,
    /// Base branch the worktree's branch is created from.
    #[arg(long)]
    base_branch: Option<String>,
    /// Skip the remote start confirmation prompt.
    #[arg(long)]
    yes: bool,
    /// Emit machine-readable JSON instead of human text.
    #[arg(long)]
    json: bool,
    /// Print the exact navigational prompt and resolved paths, then exit.
    #[arg(long)]
    print_prompt: bool,
    /// Skip live-state snapshot collection (privacy/speed).
    #[arg(long)]
    no_snapshot: bool,
    /// Launch without a readable knowledge bundle (snapshot + source map only).
    #[arg(long)]
    degraded: bool,
    /// Do not auto-start the local daemon if it is down.
    #[arg(long)]
    no_start_daemon: bool,
}

/// Intent wrappers for `assistant`. Each only sets the intent; all share
/// [`AssistantArgs`] and call the same launcher.
#[derive(Debug, Subcommand)]
enum AssistantAction {
    /// Steer the assistant toward host setup.
    Setup {
        #[command(flatten)]
        args: AssistantArgs,
    },
    /// Steer the assistant toward project configuration.
    Project {
        #[command(flatten)]
        args: AssistantArgs,
    },
    /// Steer the assistant toward reconciling an update.
    Update {
        #[command(flatten)]
        args: AssistantArgs,
    },
    /// Steer the assistant toward debugging a failure.
    Debug {
        #[command(flatten)]
        args: AssistantArgs,
    },
    /// Steer the assistant toward general help.
    Help {
        #[command(flatten)]
        args: AssistantArgs,
    },
}

impl AssistantAction {
    /// The intent this wrapper selects and the args it carries.
    fn parts(&self) -> (commands::assistant::Intent, &AssistantArgs) {
        use commands::assistant::Intent;
        match self {
            AssistantAction::Setup { args } => (Intent::Setup, args),
            AssistantAction::Project { args } => (Intent::Project, args),
            AssistantAction::Update { args } => (Intent::Update, args),
            AssistantAction::Debug { args } => (Intent::Debug, args),
            AssistantAction::Help { args } => (Intent::Help, args),
        }
    }
}

#[derive(Debug, Subcommand)]
enum ProjectAction {
    /// List known projects on the target host.
    List {
        /// Exact-match filter in key=value form. May be repeated and filters are
        /// `ANDed`. Supported keys: source, label, id.
        #[arg(long = "filter", value_name = "key=value", value_parser = commands::project::parse_project_filter)]
        filters: Vec<protocol::ProjectListFilter>,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Register a project by path (on the target host; defaults to the cwd locally).
    Add {
        /// Path to register, valid on the target host. Omit to use the current
        /// directory (local only).
        path: Option<PathBuf>,
        /// Custom display name to set on the project.
        #[arg(long)]
        name: Option<String>,
        /// Default base branch for worktrees created against this project.
        #[arg(long)]
        base_branch: Option<String>,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Show a project and its live worktrees.
    Show {
        /// Project reference: `<id|label>` on the target host.
        reference: String,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Set a project's custom display name.
    Rename {
        /// Project reference: `<id|label>` on the target host.
        reference: String,
        /// The new display name.
        name: String,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Forget a project record (never deletes the repository).
    Rm {
        /// Project reference: `<id|label>` on the target host.
        reference: String,
        /// Also remove the worktrees pohunek created for this project (never the
        /// main checkout or worktrees it did not create).
        #[arg(long)]
        prune_worktrees: bool,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Resolve one prompt by name to its template content (which layer wins), or
    /// error if neither the repo's `.pohunek/` nor the host config defines it.
    Prompt {
        /// Project reference: `<id|label>` on the target host.
        reference: String,
        /// The prompt name to resolve (a single path segment).
        name: String,
        /// Emit machine-readable JSON instead of the raw template text.
        #[arg(long)]
        json: bool,
    },

    /// Resolve one action to its full recipe (agent, base branch, branch rule,
    /// prompt name + resolved prompt content). The command the launcher calls.
    Action {
        /// Project reference: `<id|label>` on the target host.
        reference: String,
        /// The action name to resolve.
        name: String,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// List the actions resolvable for a project (with the template each uses).
    Actions {
        /// Project reference: `<id|label>` on the target host.
        reference: String,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum HostAction {
    /// Enumerate `NetBird` peers and probe their daemons.
    Discover {
        /// Re-probe peers even when a fresh standalone discovery cache exists.
        #[arg(long)]
        refresh: bool,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// List known hosts (live `NetBird` peers) with their classification.
    List {
        /// Re-probe peers even when a fresh standalone discovery cache exists.
        #[arg(long)]
        refresh: bool,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Inspect one host's live capabilities (a direct daemon query).
    Inspect {
        /// Host name to inspect (a `NetBird` peer name, or `local`).
        host: String,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum NotificationsAction {
    /// List durable notifications on one host or across reachable hosts.
    List {
        /// Include local plus all reachable hosts discovered by the local daemon.
        #[arg(long, conflicts_with = "host")]
        all_hosts: bool,
        /// Alias for `--status unread`.
        #[arg(long, conflicts_with = "status")]
        unread: bool,
        /// Filter by lifecycle status.
        #[arg(long, value_parser = commands::notifications::parse_notification_status)]
        status: Option<protocol::NotificationStatus>,
        /// Filter by notification kind.
        #[arg(long, value_parser = commands::notifications::parse_notification_kind)]
        kind: Option<protocol::NotificationKind>,
        /// Filter by severity.
        #[arg(long, value_parser = commands::notifications::parse_notification_severity)]
        severity: Option<protocol::NotificationSeverity>,
        /// Filter by agent kind. Applied client-side.
        #[arg(long, value_parser = commands::notifications::parse_agent_kind)]
        agent: Option<protocol::AgentKind>,
        /// Filter by notification producer provider.
        #[arg(long)]
        provider: Option<String>,
        /// Filter by linked session id.
        #[arg(long)]
        session: Option<String>,
        /// Maximum number of records to return.
        #[arg(long)]
        limit: Option<u32>,
        /// Pagination cursor returned by a previous list call.
        #[arg(long)]
        cursor: Option<String>,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Watch notification create/update/delete events.
    Watch {
        /// Include local plus all reachable hosts discovered by the local daemon.
        #[arg(long, conflicts_with = "host")]
        all_hosts: bool,
        /// Emit machine-readable JSON event lines instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Mark one notification as read.
    Read {
        /// Notification target: `id` or `host/id`.
        target: commands::notifications::NotificationTarget,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Acknowledge one notification.
    Ack {
        /// Notification target: `id` or `host/id`.
        target: commands::notifications::NotificationTarget,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Archive one notification.
    Archive {
        /// Notification target: `id` or `host/id`.
        target: commands::notifications::NotificationTarget,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Delete one notification record.
    Delete {
        /// Notification target: `id` or `host/id`.
        target: commands::notifications::NotificationTarget,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Read or update notification policy.
    Policy {
        #[command(subcommand)]
        action: NotificationPolicyAction,
    },

    /// Run notification retention maintenance.
    Retention {
        #[command(subcommand)]
        action: NotificationRetentionAction,
    },
}

#[derive(Debug, Subcommand)]
enum NotificationPolicyAction {
    /// Show notification policy.
    Get {
        /// Include local plus all reachable hosts discovered by the local daemon.
        #[arg(long, conflicts_with = "host")]
        all_hosts: bool,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Enable or disable one provider/kind policy flag.
    #[command(group(
        clap::ArgGroup::new("policy_value")
            .required(true)
            .args(["enabled", "disabled"])
    ))]
    Set {
        /// Provider policy namespace to update.
        #[arg(long, value_parser = commands::notifications::parse_policy_provider)]
        provider: commands::notifications::PolicyProvider,
        /// Notification kind to update.
        #[arg(long, value_parser = commands::notifications::parse_notification_kind)]
        kind: protocol::NotificationKind,
        /// Enable the selected provider/kind flag.
        #[arg(long, group = "policy_value")]
        enabled: bool,
        /// Disable the selected provider/kind flag.
        #[arg(long, group = "policy_value")]
        disabled: bool,
        /// Include local plus all reachable hosts discovered by the local daemon.
        #[arg(long, conflicts_with = "host")]
        all_hosts: bool,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum NotificationRetentionAction {
    /// Prune notification records selected by retention filters.
    #[command(group(
        clap::ArgGroup::new("retention_mode")
            .required(true)
            .args(["dry_run", "apply"])
    ))]
    Prune {
        /// Report matching records without deleting them.
        #[arg(long, group = "retention_mode")]
        dry_run: bool,
        /// Delete matching records.
        #[arg(long, group = "retention_mode")]
        apply: bool,
        /// Include local plus all reachable hosts discovered by the local daemon.
        #[arg(long, conflicts_with = "host")]
        all_hosts: bool,
        /// Restrict pruning to this lifecycle status.
        #[arg(long, value_parser = commands::notifications::parse_notification_status)]
        status: Option<protocol::NotificationStatus>,
        /// Prune records created before this RFC3339 timestamp.
        #[arg(long)]
        before: Option<String>,
        /// Maximum number of records to prune.
        #[arg(long)]
        limit: Option<u32>,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PromptAction {
    /// Render a provider prompt template using context JSON from stdin.
    Render {
        /// Provider context shape to build.
        #[arg(long, value_parser = parse_prompt_provider)]
        provider: pohunek_prompt::Provider,
        /// Provider item identifier (Linear key or GitHub PR number).
        #[arg(long)]
        item_id: String,
        /// Template file to render.
        #[arg(long)]
        template_file: PathBuf,
    },
    /// Print provider link metadata using context JSON from stdin.
    Link {
        /// Provider context shape to read.
        #[arg(long, value_parser = parse_prompt_provider)]
        provider: pohunek_prompt::Provider,
        /// Provider item identifier (Linear key or GitHub PR number).
        #[arg(long)]
        item_id: String,
        /// Provider item URL.
        #[arg(long)]
        url: String,
        /// Emit machine-readable JSON errors instead of human text.
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
        #[command(flatten)]
        hermes: HermesCliOptions,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Inspect local Hermes or daemon-backed Codex/Claude integration status.
    Status {
        /// Restrict status; Hermes requires an explicit local target.
        #[arg(long, value_enum)]
        agent: Option<commands::integration::HookAgentArg>,
        #[command(flatten)]
        hermes: HermesStatusCliOptions,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Run payload-free diagnostics for the managed Hermes operator plugin.
    Doctor {
        /// Hermes is the only agent with a local integration lifecycle.
        #[arg(long, value_enum)]
        agent: commands::integration::HookAgentArg,
        #[command(flatten)]
        hermes: HermesDoctorCliOptions,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Atomically update the managed Hermes plugin and its policy.
    Update {
        /// Hermes is the only agent with a local integration lifecycle.
        #[arg(long, value_enum)]
        agent: commands::integration::HookAgentArg,
        #[command(flatten)]
        hermes: HermesUpdateCliOptions,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Remove only the marker-owned Hermes plugin and its policy.
    Uninstall {
        /// Hermes is the only agent with a local integration lifecycle.
        #[arg(long, value_enum)]
        agent: commands::integration::HookAgentArg,
        #[command(flatten)]
        hermes: HermesUninstallCliOptions,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
}

/// Explicit local inputs for one Hermes operator-plugin lifecycle command.
#[derive(Debug, Args)]
struct HermesCliOptions {
    /// Select the default or one named Hermes profile.
    #[arg(long, conflicts_with = "hermes_home")]
    hermes_profile: Option<String>,
    /// Select one absolute, owner-private Hermes home.
    #[arg(long)]
    hermes_home: Option<PathBuf>,
    /// Use one absolute Hermes executable instead of a bounded absolute PATH lookup.
    #[arg(long)]
    hermes_bin: Option<PathBuf>,
    /// Use one absolute Pohunek executable in the generated policy.
    #[arg(long)]
    pohunek_bin: Option<PathBuf>,
    /// Explicit plugin tool access mode for install or a policy replacement update.
    #[arg(long, value_enum)]
    access_mode: Option<commands::integration::AccessModeArg>,
    /// Explicit host allowed by the generated or replacement policy; repeatable.
    #[arg(long = "allow-host")]
    allow_host: Vec<String>,
    /// Bound one plugin tool invocation in milliseconds.
    #[arg(long)]
    tool_timeout_ms: Option<u32>,
    /// Bound one session-creation daemon response wait in milliseconds.
    #[arg(long)]
    request_timeout_ms: Option<u32>,
    /// Bound one plugin tool result in bytes.
    #[arg(long)]
    max_output_bytes: Option<u32>,
    /// Bound one rendered terminal screen in bytes.
    #[arg(long)]
    max_screen_bytes: Option<u32>,
    /// Bound concurrent plugin tool invocations.
    #[arg(long)]
    max_concurrency: Option<u8>,
    /// Acknowledge a newly supplied wildcard host entry.
    #[arg(long)]
    confirm_wildcard: bool,
    /// Acknowledge a modified managed plugin before update or uninstall.
    #[arg(long)]
    confirm_modified: bool,
}

impl From<HermesCliOptions> for commands::integration::HermesOptions {
    fn from(value: HermesCliOptions) -> Self {
        Self {
            profile: value.hermes_profile,
            home: value.hermes_home,
            hermes_bin: value.hermes_bin,
            pohunek_bin: value.pohunek_bin,
            access_mode: value.access_mode,
            allowed_hosts: value.allow_host,
            tool_timeout_ms: value.tool_timeout_ms,
            request_timeout_ms: value.request_timeout_ms,
            max_output_bytes: value.max_output_bytes,
            max_screen_bytes: value.max_screen_bytes,
            max_concurrency: value.max_concurrency,
            confirm_wildcard: value.confirm_wildcard,
            confirm_modified: value.confirm_modified,
        }
    }
}

/// Exactly one explicit target for a Hermes-only lifecycle verb.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct HermesRequiredTarget {
    /// Select the default or one named Hermes profile.
    #[arg(long)]
    hermes_profile: Option<String>,
    /// Select one absolute, owner-private Hermes home.
    #[arg(long)]
    hermes_home: Option<PathBuf>,
}

/// Read-only Hermes status options.
#[derive(Debug, Args)]
struct HermesStatusCliOptions {
    /// Select the default or one named Hermes profile.
    #[arg(long = "hermes-profile", conflicts_with = "home")]
    profile: Option<String>,
    /// Select one absolute, owner-private Hermes home.
    #[arg(long = "hermes-home")]
    home: Option<PathBuf>,
    /// Use one absolute Hermes executable instead of a bounded absolute PATH lookup.
    #[arg(long = "hermes-bin")]
    binary: Option<PathBuf>,
}

impl From<HermesStatusCliOptions> for commands::integration::HermesOptions {
    fn from(value: HermesStatusCliOptions) -> Self {
        Self {
            profile: value.profile,
            home: value.home,
            hermes_bin: value.binary,
            pohunek_bin: None,
            access_mode: None,
            allowed_hosts: vec![],
            tool_timeout_ms: None,
            request_timeout_ms: None,
            max_output_bytes: None,
            max_screen_bytes: None,
            max_concurrency: None,
            confirm_wildcard: false,
            confirm_modified: false,
        }
    }
}

/// Read-only Hermes doctor options.
#[derive(Debug, Args)]
struct HermesDoctorCliOptions {
    #[command(flatten)]
    target: HermesRequiredTarget,
    /// Use one absolute Hermes executable instead of a bounded absolute PATH lookup.
    #[arg(long)]
    hermes_bin: Option<PathBuf>,
}

impl From<HermesDoctorCliOptions> for commands::integration::HermesOptions {
    fn from(value: HermesDoctorCliOptions) -> Self {
        Self {
            profile: value.target.hermes_profile,
            home: value.target.hermes_home,
            hermes_bin: value.hermes_bin,
            pohunek_bin: None,
            access_mode: None,
            allowed_hosts: vec![],
            tool_timeout_ms: None,
            request_timeout_ms: None,
            max_output_bytes: None,
            max_screen_bytes: None,
            max_concurrency: None,
            confirm_wildcard: false,
            confirm_modified: false,
        }
    }
}

/// Hermes update options, including optional policy replacements.
#[derive(Debug, Args)]
struct HermesUpdateCliOptions {
    #[command(flatten)]
    target: HermesRequiredTarget,
    /// Use one absolute Hermes executable instead of a bounded absolute PATH lookup.
    #[arg(long)]
    hermes_bin: Option<PathBuf>,
    /// Replace the fixed Pohunek executable in the installed policy.
    #[arg(long)]
    pohunek_bin: Option<PathBuf>,
    /// Replace the installed plugin access mode.
    #[arg(long, value_enum)]
    access_mode: Option<commands::integration::AccessModeArg>,
    /// Replace the host allowlist; repeatable.
    #[arg(long = "allow-host")]
    allow_host: Vec<String>,
    /// Replace the per-tool timeout in milliseconds.
    #[arg(long)]
    tool_timeout_ms: Option<u32>,
    /// Replace the session-creation response timeout in milliseconds.
    #[arg(long)]
    request_timeout_ms: Option<u32>,
    /// Replace the maximum tool-result size in bytes.
    #[arg(long)]
    max_output_bytes: Option<u32>,
    /// Replace the maximum rendered-screen size in bytes.
    #[arg(long)]
    max_screen_bytes: Option<u32>,
    /// Replace the maximum concurrent plugin tool count.
    #[arg(long)]
    max_concurrency: Option<u8>,
    /// Acknowledge a newly supplied wildcard host entry.
    #[arg(long)]
    confirm_wildcard: bool,
    /// Acknowledge modified managed assets before replacement.
    #[arg(long)]
    confirm_modified: bool,
}

impl From<HermesUpdateCliOptions> for commands::integration::HermesOptions {
    fn from(value: HermesUpdateCliOptions) -> Self {
        Self {
            profile: value.target.hermes_profile,
            home: value.target.hermes_home,
            hermes_bin: value.hermes_bin,
            pohunek_bin: value.pohunek_bin,
            access_mode: value.access_mode,
            allowed_hosts: value.allow_host,
            tool_timeout_ms: value.tool_timeout_ms,
            request_timeout_ms: value.request_timeout_ms,
            max_output_bytes: value.max_output_bytes,
            max_screen_bytes: value.max_screen_bytes,
            max_concurrency: value.max_concurrency,
            confirm_wildcard: value.confirm_wildcard,
            confirm_modified: value.confirm_modified,
        }
    }
}

/// Hermes uninstall options.
#[derive(Debug, Args)]
struct HermesUninstallCliOptions {
    #[command(flatten)]
    target: HermesRequiredTarget,
    /// Use one absolute Hermes executable instead of a bounded absolute PATH lookup.
    #[arg(long)]
    hermes_bin: Option<PathBuf>,
    /// Acknowledge modified managed assets before removal.
    #[arg(long)]
    confirm_modified: bool,
}

impl From<HermesUninstallCliOptions> for commands::integration::HermesOptions {
    fn from(value: HermesUninstallCliOptions) -> Self {
        Self {
            profile: value.target.hermes_profile,
            home: value.target.hermes_home,
            hermes_bin: value.hermes_bin,
            pohunek_bin: None,
            access_mode: None,
            allowed_hosts: vec![],
            tool_timeout_ms: None,
            request_timeout_ms: None,
            max_output_bytes: None,
            max_screen_bytes: None,
            max_concurrency: None,
            confirm_wildcard: false,
            confirm_modified: value.confirm_modified,
        }
    }
}

#[derive(Debug, Subcommand)]
enum MigrationAction {
    /// Snapshot legacy sessions and refuse replacement while live PTYs exist.
    Preflight {
        /// Record informed consent to import live legacy sessions as runtime-lost.
        #[arg(long)]
        accept_runtime_loss: bool,
        /// Emit the sanitized migration manifest as JSON.
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

    /// Write (or print) the sway drop-in that binds keys to the launchers.
    Sway {
        /// Print the snippet to stdout instead of writing the drop-in file.
        #[arg(long)]
        print: bool,
        /// Sway keybind to bind the session switcher to.
        #[arg(long, default_value = "$mod+p")]
        keybind: String,
        /// Sway keybind to bind the Linear issue picker to.
        #[arg(long, default_value = "$mod+i")]
        issue_keybind: String,
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
        /// Agent name to start: a base kind (`shell`/`codex`/`claude`/`hermes`) or a host
        /// profile, resolved daemon-side on the target host.
        #[arg(long, default_value = "shell")]
        agent: String,
        /// Owner-set display name for the session (cosmetic). Shown in the GUI
        /// and `session list`; the daemon trims it and rejects a control
        /// character or an over-long name.
        #[arg(long)]
        name: Option<String>,
        /// Working directory for the session.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Initial terminal width in columns.
        #[arg(long, default_value_t = 80)]
        cols: u16,
        /// Initial terminal height in rows.
        #[arg(long, default_value_t = 24)]
        rows: u16,
        /// Project to run in, by `<id|label>` on the target host (resolved
        /// daemon-side). The everyday way to target a host without sending a
        /// filesystem path; required for a remote host (with `--repo` as the
        /// first-introduction alternative). Mutually exclusive with `--repo`.
        #[arg(long, conflicts_with = "repo")]
        project: Option<String>,
        /// Git repository to bind a dedicated worktree for (with `--branch`), or
        /// to run in-place / register as a project (without `--branch`). Mutually
        /// exclusive with `--project` (both name the target repository).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Branch to check out in a dedicated bound worktree. Requires a source
        /// repository: either an explicit `--repo` or a resolvable `--project`
        /// (whose checkout is used). Without it the session runs in-place.
        #[arg(long)]
        branch: Option<String>,
        /// Base branch the worktree's branch is created from. Requires
        /// `--branch`; falls back to the project's configured default base
        /// branch, then the repository's HEAD, when missing.
        #[arg(long)]
        base_branch: Option<String>,
        /// Initial text to inject into the session after the PTY is spawned.
        #[arg(long)]
        input: Option<String>,
        /// Read initial text from stdin. Alias: `--stdin`.
        #[arg(long = "input-stdin", alias = "stdin", conflicts_with = "input")]
        input_stdin: bool,
        /// Override the daemon response timeout for this creation request.
        #[arg(long, value_parser = commands::session::parse_request_timeout_ms)]
        request_timeout_ms: Option<u32>,
        /// Session metadata in key=value form (repeatable). Split on the first
        /// `=` only, so a value may itself contain `=`. Each key may be set at
        /// most once; the daemon enforces size limits on the values.
        #[arg(long = "meta", value_name = "key=value", value_parser = commands::session::parse_meta_pair)]
        meta: Vec<(String, String)>,
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
        /// are `ANDed`. Supported keys: state, activity, agent, id.
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

    /// Fork an agent conversation into a new session.
    Fork {
        /// Source session target: `session-id` or `local/session-id`.
        target: Target,
        /// Owner-set display name for the forked session.
        #[arg(long)]
        name: Option<String>,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Remove one session from the daemon, stopping it first if still live.
    Rm {
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
        #[arg(
            required_unless_present = "input_stdin",
            conflicts_with = "input_stdin"
        )]
        text: Option<String>,
        /// Read text from stdin instead of argv. Alias: `--input-stdin`.
        #[arg(long = "stdin", alias = "input-stdin")]
        input_stdin: bool,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Read the current rendered terminal screen.
    Screen {
        target: Target,
        #[arg(long)]
        json: bool,
    },

    /// Read bounded retained terminal output.
    Output {
        target: Target,
        #[arg(long, requires = "runtime_generation")]
        runtime_id: Option<String>,
        #[arg(long, requires = "runtime_id")]
        runtime_generation: Option<u64>,
        #[arg(long)]
        after_offset: Option<u64>,
        #[arg(long, default_value_t = commands::session::DEFAULT_OUTPUT_BYTES, value_parser = commands::session::parse_output_bytes)]
        max_bytes: u32,
        #[arg(long)]
        wait_ms: Option<u32>,
        #[arg(long)]
        json: bool,
    },

    /// Wait for one bounded session change.
    Wait {
        target: Target,
        #[arg(long, requires = "runtime_generation")]
        runtime_id: Option<String>,
        #[arg(long, requires = "runtime_id")]
        runtime_generation: Option<u64>,
        #[arg(long)]
        after_updated_at: Option<String>,
        #[arg(long)]
        after_terminal_watermark: Option<u64>,
        #[arg(long)]
        after_output_offset: Option<u64>,
        #[arg(long = "state", value_parser = commands::session::parse_state_filter)]
        states: Vec<protocol::SessionState>,
        #[arg(long = "activity", value_parser = commands::session::parse_activity_filter)]
        activities: Vec<protocol::AgentActivity>,
        #[arg(long, value_parser = commands::session::parse_wait_timeout_ms)]
        timeout_ms: u32,
        #[arg(long)]
        json: bool,
    },

    /// Resume a stopped logical session.
    Resume {
        target: Target,
        #[arg(long)]
        json: bool,
    },

    /// Resize a managed session terminal.
    Resize {
        target: Target,
        #[arg(long)]
        cols: u16,
        #[arg(long)]
        rows: u16,
        #[arg(long)]
        json: bool,
    },

    /// Merge or clear owner-controlled session metadata.
    Metadata {
        target: Target,
        #[arg(long = "set", value_name = "key=value", value_parser = commands::session::parse_meta_pair)]
        set: Vec<(String, String)>,
        #[arg(long = "clear", value_name = "key")]
        clear: Vec<String>,
        #[arg(long)]
        json: bool,
    },

    /// List durable worker runtime endpoints.
    RuntimeInventory {
        #[arg(long)]
        json: bool,
    },

    /// Set or clear a session's display name.
    Rename {
        /// Session target: `session-id` or `local/session-id`.
        target: Target,
        /// New display name. Omit together with `--clear` to clear the name.
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        name: Option<String>,
        /// Clear the display name instead of setting one.
        #[arg(long)]
        clear: bool,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Show a unified diff of a session's worktree against its base.
    Diff {
        /// Session target: `session-id` or `local/session-id`.
        target: Target,
        /// Explicit base ref to diff against. Defaults to the worktree
        /// binding's recorded base branch, then the repository default.
        #[arg(long)]
        base: Option<String>,
        /// Emit the structured result as JSON instead of raw diff text.
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
            Commands::Migration { action } => match action {
                MigrationAction::Preflight { json, .. } => *json,
            },
            Commands::Setup { action, json } => {
                action.as_ref().map_or(*json, SetupAction::wants_json)
            }
            Commands::Host { action } => action.wants_json(),
            Commands::Notifications { action } => action.wants_json(),
            Commands::Project { action } => action.wants_json(),
            Commands::Assistant { action, args, .. } => {
                action.as_ref().map_or(args.json, |a| a.parts().1.json)
            }
            Commands::Subscribe { json } => *json,
            Commands::Prompt { action } => action.wants_json(),
            Commands::Attach { .. } | Commands::Daemon { .. } => false,
        }
    }

    fn uses_all_hosts(&self) -> bool {
        match self {
            Commands::Notifications { action } => action.uses_all_hosts(),
            Commands::Attach { .. }
            | Commands::Doctor { .. }
            | Commands::Daemon { .. }
            | Commands::Health { .. }
            | Commands::Status { .. }
            | Commands::Session { .. }
            | Commands::Subscribe { .. }
            | Commands::Integration { .. }
            | Commands::Migration { .. }
            | Commands::Setup { .. }
            | Commands::Host { .. }
            | Commands::Prompt { .. }
            | Commands::Project { .. }
            | Commands::Assistant { .. } => false,
        }
    }
}

impl ProjectAction {
    fn wants_json(&self) -> bool {
        match self {
            ProjectAction::List { json, .. }
            | ProjectAction::Add { json, .. }
            | ProjectAction::Show { json, .. }
            | ProjectAction::Rename { json, .. }
            | ProjectAction::Rm { json, .. }
            | ProjectAction::Prompt { json, .. }
            | ProjectAction::Action { json, .. }
            | ProjectAction::Actions { json, .. } => *json,
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
            HostAction::Discover { json, .. }
            | HostAction::List { json, .. }
            | HostAction::Inspect { json, .. } => *json,
        }
    }
}

impl NotificationsAction {
    fn wants_json(&self) -> bool {
        match self {
            NotificationsAction::List { json, .. }
            | NotificationsAction::Watch { json, .. }
            | NotificationsAction::Read { json, .. }
            | NotificationsAction::Ack { json, .. }
            | NotificationsAction::Archive { json, .. }
            | NotificationsAction::Delete { json, .. } => *json,
            NotificationsAction::Policy { action } => action.wants_json(),
            NotificationsAction::Retention { action } => action.wants_json(),
        }
    }

    fn uses_all_hosts(&self) -> bool {
        match self {
            NotificationsAction::List { all_hosts, .. }
            | NotificationsAction::Watch { all_hosts, .. } => *all_hosts,
            NotificationsAction::Policy { action } => action.uses_all_hosts(),
            NotificationsAction::Retention { action } => action.uses_all_hosts(),
            NotificationsAction::Read { .. }
            | NotificationsAction::Ack { .. }
            | NotificationsAction::Archive { .. }
            | NotificationsAction::Delete { .. } => false,
        }
    }
}

impl NotificationPolicyAction {
    fn wants_json(&self) -> bool {
        match self {
            NotificationPolicyAction::Get { json, .. }
            | NotificationPolicyAction::Set { json, .. } => *json,
        }
    }

    fn uses_all_hosts(&self) -> bool {
        match self {
            NotificationPolicyAction::Get { all_hosts, .. }
            | NotificationPolicyAction::Set { all_hosts, .. } => *all_hosts,
        }
    }
}

impl NotificationRetentionAction {
    fn wants_json(&self) -> bool {
        match self {
            NotificationRetentionAction::Prune { json, .. } => *json,
        }
    }

    fn uses_all_hosts(&self) -> bool {
        match self {
            NotificationRetentionAction::Prune { all_hosts, .. } => *all_hosts,
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
            | SessionAction::Fork { json, .. }
            | SessionAction::Rm { json, .. }
            | SessionAction::Input { json, .. }
            | SessionAction::Screen { json, .. }
            | SessionAction::Output { json, .. }
            | SessionAction::Wait { json, .. }
            | SessionAction::Resume { json, .. }
            | SessionAction::Resize { json, .. }
            | SessionAction::Metadata { json, .. }
            | SessionAction::RuntimeInventory { json }
            | SessionAction::Rename { json, .. }
            | SessionAction::Diff { json, .. } => *json,
        }
    }
}

impl IntegrationAction {
    fn wants_json(&self) -> bool {
        match self {
            IntegrationAction::Install { json, .. }
            | IntegrationAction::Status { json, .. }
            | IntegrationAction::Doctor { json, .. }
            | IntegrationAction::Update { json, .. }
            | IntegrationAction::Uninstall { json, .. } => *json,
        }
    }
}

fn run_integration_hermes_action(
    action: commands::integration::HermesAction,
    agent: commands::integration::HookAgentArg,
    options: &commands::integration::HermesOptions,
    json: bool,
) -> Result<ExitCode, CliError> {
    if agent != commands::integration::HookAgentArg::Hermes {
        return Err(commands::integration::unsupported_action(Some(agent)));
    }
    let healthy = commands::integration::run_hermes(action, options, json)?;
    Ok(if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

impl PromptAction {
    fn wants_json(&self) -> bool {
        match self {
            PromptAction::Render { .. } => false,
            PromptAction::Link { json, .. } => *json,
        }
    }
}

pub async fn run_cli() -> ExitCode {
    // Parse manually (not `Cli::parse`) so a clap usage error can be rendered as a
    // structured `--json` document instead of clap's human text + hard process
    // exit. We keep the raw argv to recover the `--json` intent: parsing fails
    // before a typed `Cli` exists, so `wants_json` is unavailable on that path.
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(err) => return error::render_clap_error(&err, error::args_request_json(&args)),
    };
    if cli.command.uses_all_hosts() && args_request_host(&args) {
        let err = clap::Error::raw(
            clap::error::ErrorKind::ArgumentConflict,
            "--host cannot be used together with --all-hosts",
        );
        return error::render_clap_error(&err, error::args_request_json(&args));
    }
    // Capture whether the active command requested `--json` before `run` consumes
    // `cli`, so a failure is rendered in the same mode a success would have been.
    let json = cli.command.wants_json();
    // Box the large dispatch future so this entrypoint future stays small.
    match Box::pin(run(cli)).await {
        Ok(code) => code,
        Err(err) => {
            error::render(&err, json);
            ExitCode::FAILURE
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "flat one-arm-per-subcommand dispatch match; splitting would only scatter it"
)]
async fn run(cli: Cli) -> Result<ExitCode, CliError> {
    let global_host = cli.host;

    match cli.command {
        Commands::Attach { target } => {
            let paths = Paths::resolve()?;
            let host = effective_host(&global_host, Some(&target));
            // Box the large attach future to keep this dispatch arm small.
            Box::pin(commands::attach::run_attach(&host, &paths, &target)).await?;
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
                    name,
                    cwd,
                    cols,
                    rows,
                    project,
                    repo,
                    branch,
                    base_branch,
                    input,
                    input_stdin,
                    request_timeout_ms,
                    meta,
                    yes,
                    json,
                } => {
                    let host = effective_host(&global_host, None);
                    let input = if input_stdin {
                        Some(commands::session::read_stdin_input()?)
                    } else {
                        input
                    };
                    commands::session::run_new(
                        &host,
                        &paths,
                        commands::session::NewArgs {
                            agent,
                            name,
                            cwd,
                            cols,
                            rows,
                            project,
                            repo,
                            branch,
                            base_branch,
                            input,
                            request_timeout_ms,
                            meta,
                        },
                        json,
                        yes,
                    )
                    .await?;
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
                    commands::session::run_list(&host, &paths, &filters, output_mode).await?;
                }
                SessionAction::Inspect { target, json } => {
                    let host = effective_host(&global_host, Some(&target));
                    let target = commands::session::resolve_target(&host, &paths, &target).await?;
                    commands::session::run_inspect(&host, &paths, &target, json).await?;
                }
                SessionAction::Stop { target, json } => {
                    let host = effective_host(&global_host, Some(&target));
                    let target = commands::session::resolve_target(&host, &paths, &target).await?;
                    commands::session::run_stop(&host, &paths, &target, json).await?;
                }
                SessionAction::Fork { target, name, json } => {
                    let host = effective_host(&global_host, Some(&target));
                    let target = commands::session::resolve_target(&host, &paths, &target).await?;
                    commands::session::run_fork(&host, &paths, &target, name, json).await?;
                }
                SessionAction::Rm { target, json } => {
                    let host = effective_host(&global_host, Some(&target));
                    let target = commands::session::resolve_target(&host, &paths, &target).await?;
                    commands::session::run_remove(&host, &paths, &target, json).await?;
                }
                SessionAction::Input {
                    target,
                    text,
                    input_stdin,
                    json,
                } => {
                    let host = effective_host(&global_host, Some(&target));
                    let text = if input_stdin {
                        commands::session::read_stdin_input()?
                    } else {
                        text.expect("clap requires positional text or --stdin")
                    };
                    let target = commands::session::resolve_target(&host, &paths, &target).await?;
                    commands::session::run_input(&host, &paths, &target, &text, json).await?;
                }
                SessionAction::Screen { target, json } => {
                    let host = effective_host(&global_host, Some(&target));
                    let target = commands::session::resolve_target(&host, &paths, &target).await?;
                    commands::session::run_screen(&host, &paths, &target, json).await?;
                }
                SessionAction::Output {
                    target,
                    runtime_id,
                    runtime_generation,
                    after_offset,
                    max_bytes,
                    wait_ms,
                    json,
                } => {
                    let host = effective_host(&global_host, Some(&target));
                    commands::session::run_output(
                        &host,
                        &paths,
                        &target,
                        commands::session::OutputArgs {
                            runtime_id,
                            runtime_generation,
                            after_offset,
                            max_bytes,
                            wait_ms,
                        },
                        json,
                    )
                    .await?;
                }
                SessionAction::Wait {
                    target,
                    runtime_id,
                    runtime_generation,
                    after_updated_at,
                    after_terminal_watermark,
                    after_output_offset,
                    states,
                    activities,
                    timeout_ms,
                    json,
                } => {
                    let host = effective_host(&global_host, Some(&target));
                    commands::session::run_wait(
                        &host,
                        &paths,
                        &target,
                        commands::session::WaitArgs {
                            runtime_id,
                            runtime_generation,
                            after_updated_at,
                            after_terminal_watermark,
                            after_output_offset,
                            states,
                            activities,
                            timeout_ms,
                        },
                        json,
                    )
                    .await?;
                }
                SessionAction::Resume { target, json } => {
                    let host = effective_host(&global_host, Some(&target));
                    let target = commands::session::resolve_target(&host, &paths, &target).await?;
                    commands::session::run_resume(&host, &paths, &target, json).await?;
                }
                SessionAction::Resize {
                    target,
                    cols,
                    rows,
                    json,
                } => {
                    let host = effective_host(&global_host, Some(&target));
                    let target = commands::session::resolve_target(&host, &paths, &target).await?;
                    commands::session::run_resize(&host, &paths, &target, cols, rows, json).await?;
                }
                SessionAction::Metadata {
                    target,
                    set,
                    clear,
                    json,
                } => {
                    let host = effective_host(&global_host, Some(&target));
                    let target = commands::session::resolve_target(&host, &paths, &target).await?;
                    let mut metadata = std::collections::BTreeMap::new();
                    for (key, value) in set {
                        if metadata.insert(key.clone(), Some(value)).is_some() {
                            return Err(CliError::DuplicateMetaKey { key });
                        }
                    }
                    for key in clear {
                        if metadata.insert(key.clone(), None).is_some() {
                            return Err(CliError::DuplicateMetaKey { key });
                        }
                    }
                    commands::session::run_metadata(&host, &paths, &target, metadata, json).await?;
                }
                SessionAction::RuntimeInventory { json } => {
                    let host = effective_host(&global_host, None);
                    commands::session::run_runtime_inventory(&host, &paths, json).await?;
                }
                SessionAction::Rename {
                    target,
                    name,
                    clear,
                    json,
                } => {
                    let host = effective_host(&global_host, Some(&target));
                    let target = commands::session::resolve_target(&host, &paths, &target).await?;
                    // `--clear` (or no name) clears it; a positional name sets it.
                    let new_name = if clear { None } else { name };
                    commands::session::run_rename(&host, &paths, &target, new_name, json).await?;
                }
                SessionAction::Diff { target, base, json } => {
                    let host = effective_host(&global_host, Some(&target));
                    let target = commands::session::resolve_target(&host, &paths, &target).await?;
                    commands::session::run_diff(&host, &paths, &target, base, json).await?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Subscribe { .. } => {
            // `json` is intentionally not read here: the daemon's event stream is
            // already NDJSON, so success output is the same with or without the
            // flag. It still governs error rendering through `wants_json` above.
            let paths = Paths::resolve()?;
            let host = effective_host(&global_host, None);
            let client = crate::client::Client::connect(&host, &paths).await?;
            let request = Request::new(
                commands::request_id(method::SUBSCRIBE),
                method::SUBSCRIBE,
                serde_json::Value::Null,
            )
            .map_err(pohunek_client::ClientError::from)?;
            let mut subscription = client.into_sdk().subscribe(&request).await?;
            while let Some(line) = subscription.next_line().await? {
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Integration { action } => {
            match action {
                IntegrationAction::Install {
                    agent,
                    hermes,
                    json,
                } => {
                    let hermes = commands::integration::HermesOptions::from(hermes);
                    match agent {
                        Some(commands::integration::HookAgentArg::Hermes) => {
                            commands::integration::run_hermes(
                                commands::integration::HermesAction::Install,
                                &hermes,
                                json,
                            )?;
                        }
                        _ if hermes.is_explicit() => {
                            return Err(commands::integration::hermes_options_require_hermes());
                        }
                        agent => {
                            // Codex and Claude hook installation remains a local daemon
                            // operation and retains its existing request and output shape.
                            let paths = Paths::resolve()?;
                            commands::integration::run_install(&paths, agent, json).await?;
                        }
                    }
                }
                IntegrationAction::Status {
                    agent,
                    hermes,
                    json,
                } => {
                    let hermes = commands::integration::HermesOptions::from(hermes);
                    if agent == Some(commands::integration::HookAgentArg::Hermes) {
                        return run_integration_hermes_action(
                            commands::integration::HermesAction::Status,
                            commands::integration::HookAgentArg::Hermes,
                            &hermes,
                            json,
                        );
                    }
                    if hermes.is_explicit() {
                        return Err(commands::integration::hermes_options_require_hermes());
                    }
                    let paths = Paths::resolve()?;
                    commands::integration::run_status(&paths, agent, json).await?;
                    return Ok(ExitCode::SUCCESS);
                }
                IntegrationAction::Doctor {
                    agent,
                    hermes,
                    json,
                } => {
                    return run_integration_hermes_action(
                        commands::integration::HermesAction::Doctor,
                        agent,
                        &hermes.into(),
                        json,
                    );
                }
                IntegrationAction::Update {
                    agent,
                    hermes,
                    json,
                } => {
                    return run_integration_hermes_action(
                        commands::integration::HermesAction::Update,
                        agent,
                        &hermes.into(),
                        json,
                    );
                }
                IntegrationAction::Uninstall {
                    agent,
                    hermes,
                    json,
                } => {
                    return run_integration_hermes_action(
                        commands::integration::HermesAction::Uninstall,
                        agent,
                        &hermes.into(),
                        json,
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Migration { action } => {
            let paths = Paths::resolve()?;
            let host = effective_host(&global_host, None);
            match action {
                MigrationAction::Preflight {
                    accept_runtime_loss,
                    json,
                } => {
                    commands::migration::run_preflight(&host, &paths, accept_runtime_loss, json)
                        .await?;
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
                    issue_keybind,
                    json,
                }) => {
                    commands::setup::run_sway(&paths, print, &keybind, &issue_keybind, json)?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Host { action } => match action {
            // Discovery is local NetBird state plus the CLI cache; no local
            // daemon connection is needed.
            HostAction::Discover { refresh, json } => {
                commands::host::run_discover(refresh, json).await?;
                Ok(ExitCode::SUCCESS)
            }
            HostAction::List { refresh, json } => {
                commands::host::run_list(refresh, json).await?;
                Ok(ExitCode::SUCCESS)
            }
            HostAction::Inspect { host, json } => {
                // `inspect` uses its positional host arg, not the global flag.
                let paths = Paths::resolve()?;
                commands::host::run_inspect(&host, &paths, json).await?;
                Ok(ExitCode::SUCCESS)
            }
        },
        Commands::Notifications { action } => {
            let paths = Paths::resolve()?;
            let host = effective_host(&global_host, None);
            match action {
                NotificationsAction::List {
                    all_hosts,
                    unread,
                    status,
                    kind,
                    severity,
                    agent,
                    provider,
                    session,
                    limit,
                    cursor,
                    json,
                } => {
                    commands::notifications::run_list(
                        &host,
                        &paths,
                        commands::notifications::ListFilters {
                            unread,
                            status,
                            kind,
                            severity,
                            provider,
                            agent,
                            session,
                            limit,
                            cursor,
                        },
                        all_hosts,
                        json,
                    )
                    .await?;
                }
                NotificationsAction::Watch { all_hosts, json } => {
                    commands::notifications::run_watch(&host, &paths, all_hosts, json).await?;
                }
                NotificationsAction::Read { target, json } => {
                    let host = effective_notification_host(&host, &target);
                    commands::notifications::run_read(&host, &paths, &target, json).await?;
                }
                NotificationsAction::Ack { target, json } => {
                    let host = effective_notification_host(&host, &target);
                    commands::notifications::run_ack(&host, &paths, &target, json).await?;
                }
                NotificationsAction::Archive { target, json } => {
                    let host = effective_notification_host(&host, &target);
                    commands::notifications::run_archive(&host, &paths, &target, json).await?;
                }
                NotificationsAction::Delete { target, json } => {
                    let host = effective_notification_host(&host, &target);
                    commands::notifications::run_delete(&host, &paths, &target, json).await?;
                }
                NotificationsAction::Policy { action } => match action {
                    NotificationPolicyAction::Get { all_hosts, json } => {
                        commands::notifications::run_policy_get(&host, &paths, all_hosts, json)
                            .await?;
                    }
                    NotificationPolicyAction::Set {
                        provider,
                        kind,
                        enabled,
                        all_hosts,
                        json,
                        ..
                    } => {
                        commands::notifications::run_policy_set(
                            &host, &paths, provider, kind, enabled, all_hosts, json,
                        )
                        .await?;
                    }
                },
                NotificationsAction::Retention { action } => match action {
                    NotificationRetentionAction::Prune {
                        dry_run,
                        apply,
                        all_hosts,
                        status,
                        before,
                        limit,
                        json,
                    } => {
                        commands::notifications::run_retention_prune(
                            &host,
                            &paths,
                            commands::notifications::RetentionArgs {
                                dry_run,
                                apply,
                                status,
                                before,
                                limit,
                            },
                            all_hosts,
                            json,
                        )
                        .await?;
                    }
                },
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Prompt { action } => {
            match action {
                PromptAction::Render {
                    provider,
                    item_id,
                    template_file,
                } => {
                    let rendered =
                        commands::prompt::render_prompt(provider, &item_id, &template_file)?;
                    print!("{rendered}");
                }
                PromptAction::Link {
                    provider,
                    item_id,
                    url,
                    ..
                } => {
                    let output = commands::prompt::link_metadata(provider, &item_id, &url)?;
                    print!("{output}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Project { action } => {
            // Projects are per-host; references resolve daemon-side, so the whole
            // surface routes through the effective host like `session`.
            let paths = Paths::resolve()?;
            let host = effective_host(&global_host, None);
            match action {
                ProjectAction::List { filters, json } => {
                    commands::project::run_list(&host, &paths, &filters, json).await?;
                }
                ProjectAction::Add {
                    path,
                    name,
                    base_branch,
                    json,
                } => {
                    commands::project::run_add(&host, &paths, path, name, base_branch, json)
                        .await?;
                }
                ProjectAction::Show { reference, json } => {
                    commands::project::run_show(&host, &paths, &reference, json).await?;
                }
                ProjectAction::Rename {
                    reference,
                    name,
                    json,
                } => {
                    commands::project::run_rename(&host, &paths, &reference, &name, json).await?;
                }
                ProjectAction::Rm {
                    reference,
                    prune_worktrees,
                    json,
                } => {
                    commands::project::run_rm(&host, &paths, &reference, prune_worktrees, json)
                        .await?;
                }
                ProjectAction::Prompt {
                    reference,
                    name,
                    json,
                } => {
                    commands::project::run_prompt(&host, &paths, &reference, &name, json).await?;
                }
                ProjectAction::Action {
                    reference,
                    name,
                    json,
                } => {
                    commands::project::run_action(&host, &paths, &reference, &name, json).await?;
                }
                ProjectAction::Actions { reference, json } => {
                    commands::project::run_actions(&host, &paths, &reference, json).await?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Assistant {
            action,
            intent,
            args,
        } => {
            let paths = Paths::resolve()?;
            let opts = resolve_assistant_options(&global_host, action.as_ref(), intent, args);
            commands::assistant::run(opts, &paths).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Collapse the assistant CLI surface (default form or intent wrapper) into the
/// single internal launch model. Wrappers only set the intent; the default form
/// uses `--intent` (defaulting to `help`).
fn resolve_assistant_options(
    global_host: &str,
    action: Option<&AssistantAction>,
    intent: Option<commands::assistant::Intent>,
    default_args: AssistantArgs,
) -> commands::assistant::AssistantOptions {
    use commands::assistant::Intent;

    let (intent, args) = match action {
        Some(wrapper) => {
            let (intent, args) = wrapper.parts();
            (intent, args.clone())
        }
        None => (intent.unwrap_or(Intent::Help), default_args),
    };

    let request = if args.request.is_empty() {
        None
    } else {
        Some(args.request.join(" "))
    };

    commands::assistant::AssistantOptions {
        intent,
        request,
        agent: args.agent,
        host: effective_host(global_host, None),
        project: args.project,
        repo: args.repo,
        branch: args.branch,
        base_branch: args.base_branch,
        yes: args.yes,
        json: args.json,
        print_prompt: args.print_prompt,
        no_snapshot: args.no_snapshot,
        degraded: args.degraded,
        no_start_daemon: args.no_start_daemon,
    }
}

/// Resolve the effective host for a command.
///
/// A positional [`Target`]'s host (when present) wins over the global `--host`
/// flag; otherwise the global flag is used. `None` (no target) means "use the
/// global flag" (commands that take only the flag). The returned string is the
/// host name the transport selects on (`local`, or a `NetBird` peer name).
#[must_use]
fn effective_host(global: &str, target: Option<&Target>) -> String {
    match target.and_then(|t| t.host.as_deref()) {
        Some(host) => host.to_owned(),
        None => global.to_owned(),
    }
}

fn effective_notification_host(
    global: &str,
    target: &commands::notifications::NotificationTarget,
) -> String {
    target.host.as_deref().unwrap_or(global).to_owned()
}

fn parse_prompt_provider(value: &str) -> Result<pohunek_prompt::Provider, String> {
    value
        .parse()
        .map_err(|err: pohunek_prompt::Error| err.to_string())
}

fn args_request_host(args: &[std::ffi::OsString]) -> bool {
    let host_flag = std::ffi::OsStr::new("--host");
    let end_of_opts = std::ffi::OsStr::new("--");
    args.iter()
        .skip(1)
        .take_while(|arg| arg.as_os_str() != end_of_opts)
        .any(|arg| arg.as_os_str() == host_flag || arg.to_string_lossy().starts_with("--host="))
}

#[cfg(test)]
mod tests {
    use crate as pohunek_cli;

    use super::*;

    #[test]
    fn command_tree_is_internally_consistent() {
        // clap validates `conflicts_with`/`requires` id references (and other
        // structural invariants) in `debug_assert`; a typo'd reference panics
        // here rather than at runtime.
        pohunek_cli::command().debug_assert();
    }

    #[test]
    fn integration_parses_explicit_hermes_install_policy() {
        let cli = Cli::try_parse_from([
            "pohunek",
            "integration",
            "install",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--access-mode",
            "read_only",
            "--allow-host",
            "local",
            "--tool-timeout-ms",
            "8000",
            "--max-output-bytes",
            "262144",
            "--max-screen-bytes",
            "65536",
            "--max-concurrency",
            "1",
            "--json",
        ])
        .expect("parse explicit Hermes install");

        match cli.command {
            Commands::Integration {
                action:
                    IntegrationAction::Install {
                        agent: Some(commands::integration::HookAgentArg::Hermes),
                        hermes,
                        json,
                    },
            } => {
                assert_eq!(hermes.hermes_profile.as_deref(), Some("default"));
                assert_eq!(hermes.allow_host, ["local"]);
                assert_eq!(hermes.tool_timeout_ms, Some(8_000));
                assert_eq!(hermes.max_output_bytes, Some(262_144));
                assert_eq!(hermes.max_screen_bytes, Some(65_536));
                assert_eq!(hermes.max_concurrency, Some(1));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn integration_parses_explicit_hermes_update_policy_bounds() {
        let cli = Cli::try_parse_from([
            "pohunek",
            "integration",
            "update",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--tool-timeout-ms",
            "8000",
            "--max-output-bytes",
            "262144",
            "--max-screen-bytes",
            "65536",
            "--max-concurrency",
            "1",
        ])
        .expect("parse explicit Hermes update bounds");

        match cli.command {
            Commands::Integration {
                action:
                    IntegrationAction::Update {
                        agent: commands::integration::HookAgentArg::Hermes,
                        hermes,
                        json: false,
                    },
            } => {
                assert_eq!(hermes.tool_timeout_ms, Some(8_000));
                assert_eq!(hermes.max_output_bytes, Some(262_144));
                assert_eq!(hermes.max_screen_bytes, Some(65_536));
                assert_eq!(hermes.max_concurrency, Some(1));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn integration_rejects_hermes_policy_bounds_for_other_actions() {
        for action in ["status", "doctor", "uninstall"] {
            for (flag, value) in [
                ("--tool-timeout-ms", "8000"),
                ("--max-output-bytes", "262144"),
                ("--max-screen-bytes", "65536"),
                ("--max-concurrency", "1"),
            ] {
                let error = Cli::try_parse_from([
                    "pohunek",
                    "integration",
                    action,
                    "--agent",
                    "hermes",
                    "--hermes-profile",
                    "default",
                    flag,
                    value,
                ])
                .expect_err("policy bound must be rejected for this action");
                assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
            }
        }
    }

    #[test]
    fn integration_preserves_status_target_conflicts_and_other_required_targets() {
        let conflict = Cli::try_parse_from([
            "pohunek",
            "integration",
            "status",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-home",
            "/private/hermes",
        ])
        .expect_err("targets conflict");
        assert_eq!(conflict.kind(), clap::error::ErrorKind::ArgumentConflict);

        Cli::try_parse_from(["pohunek", "integration", "status"])
            .expect("bare daemon-backed status remains reachable");
        for agent in ["codex", "claude"] {
            Cli::try_parse_from(["pohunek", "integration", "status", "--agent", agent])
                .expect("explicit daemon-backed status remains reachable");
        }
        Cli::try_parse_from([
            "pohunek",
            "integration",
            "status",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            "/opt/hermes/bin/hermes",
        ])
        .expect("explicit local Hermes status preserves target flags");

        Cli::try_parse_from(["pohunek", "integration", "status", "--agent", "hermes"])
            .expect("Hermes target validation runs before local filesystem access");

        for action in ["doctor", "update", "uninstall"] {
            let missing_target =
                Cli::try_parse_from(["pohunek", "integration", action, "--agent", "hermes"])
                    .expect_err("Hermes-only action requires an explicit target");
            assert_eq!(
                missing_target.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{action}"
            );
        }

        Cli::try_parse_from(["pohunek", "integration", "install"])
            .expect("legacy no-agent install remains valid");
    }

    #[test]
    fn integration_rejects_non_hermes_lifecycle_before_dispatch() {
        let error = run_integration_hermes_action(
            commands::integration::HermesAction::Doctor,
            commands::integration::HookAgentArg::Codex,
            &commands::integration::HermesOptions {
                profile: None,
                home: None,
                hermes_bin: None,
                pohunek_bin: None,
                access_mode: None,
                allowed_hosts: vec![],
                tool_timeout_ms: None,
                request_timeout_ms: None,
                max_output_bytes: None,
                max_screen_bytes: None,
                max_concurrency: None,
                confirm_wildcard: false,
                confirm_modified: false,
            },
            true,
        )
        .expect_err("Codex lifecycle is unsupported");
        assert_eq!(
            error.to_protocol_error().code,
            "integration_action_unsupported"
        );
    }

    #[test]
    fn assistant_rejects_project_and_repo_together() {
        let err = Cli::try_parse_from([
            "pohunek",
            "assistant",
            "--project",
            "ui",
            "--repo",
            "/srv/repo",
        ])
        .expect_err("--project and --repo are mutually exclusive");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn exported_command_matches_root_subcommands() {
        let command = pohunek_cli::command();

        assert_eq!(command.get_name(), "pohunek");
        for expected in [
            "attach",
            "doctor",
            "daemon",
            "health",
            "status",
            "session",
            "integration",
            "setup",
            "host",
            "notifications",
            "prompt",
            "project",
            "assistant",
        ] {
            assert!(
                command
                    .get_subcommands()
                    .any(|subcommand| subcommand.get_name() == expected),
                "missing {expected} subcommand"
            );
        }
    }

    #[test]
    fn parses_session_new_defaults() {
        let cli = Cli::try_parse_from(["pohunek", "session", "new"]).expect("parse");

        match cli.command {
            Commands::Session {
                action:
                    SessionAction::New {
                        agent,
                        name,
                        cwd,
                        cols,
                        rows,
                        project,
                        repo,
                        branch,
                        base_branch,
                        input,
                        input_stdin,
                        request_timeout_ms,
                        meta,
                        yes,
                        json,
                    },
            } => {
                assert_eq!(agent, "shell");
                assert_eq!(name, None);
                assert_eq!(cwd, None);
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
                assert_eq!(project, None);
                assert_eq!(repo, None);
                assert_eq!(branch, None);
                assert_eq!(base_branch, None);
                assert_eq!(input, None);
                assert!(!input_stdin);
                assert_eq!(request_timeout_ms, None);
                assert!(meta.is_empty(), "meta defaults to empty");
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
                assert_eq!(agent, "codex");
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
                assert_eq!(agent, "claude");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_new_hermes_agent() {
        let cli =
            Cli::try_parse_from(["pohunek", "session", "new", "--agent", "hermes"]).expect("parse");

        match cli.command {
            Commands::Session {
                action: SessionAction::New { agent, .. },
            } => assert_eq!(agent, "hermes"),
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
                assert_eq!(agent, "claude");
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
    fn rejects_session_new_project_and_repo_together() {
        // --project and --repo both name the target repository; clap rejects them
        // together so the daemon never has to reconcile an incoherent pair.
        let err = Cli::try_parse_from([
            "pohunek",
            "session",
            "new",
            "--project",
            "ui",
            "--repo",
            "/x",
        ])
        .expect_err("project and repo conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parses_session_new_project_reference() {
        let cli =
            Cli::try_parse_from(["pohunek", "session", "new", "--project", "ui"]).expect("parse");
        match cli.command {
            Commands::Session {
                action: SessionAction::New { project, repo, .. },
            } => {
                assert_eq!(project.as_deref(), Some("ui"));
                assert_eq!(repo, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_new_repeated_meta_flags() {
        let cli = Cli::try_parse_from([
            "pohunek",
            "session",
            "new",
            "--meta",
            "link.provider=github",
            "--meta",
            "link.kind=pull_request",
        ])
        .expect("parse");
        match cli.command {
            Commands::Session {
                action: SessionAction::New { meta, .. },
            } => {
                assert_eq!(
                    meta,
                    vec![
                        ("link.provider".to_owned(), "github".to_owned()),
                        ("link.kind".to_owned(), "pull_request".to_owned()),
                    ]
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_session_new_malformed_meta_flag() {
        // A `--meta` value missing `=` fails at parse time (clap usage error),
        // not silently or after a daemon round-trip.
        let err = Cli::try_parse_from(["pohunek", "session", "new", "--meta", "no-equals-sign"])
            .expect_err("malformed --meta must fail to parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
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
                action:
                    SessionAction::Input {
                        target,
                        text,
                        input_stdin,
                        json,
                    },
            } => {
                assert_eq!(target.session_id, "s-42");
                assert_eq!(target.host.as_deref(), Some("local"));
                assert_eq!(text.as_deref(), Some("write tests first"));
                assert!(!input_stdin);
                assert!(!json, "json defaults to false");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_session_input_from_stdin_without_argv_payload() {
        let cli = Cli::try_parse_from([
            "pohunek",
            "session",
            "input",
            "host-a/s-42",
            "--stdin",
            "--json",
        ])
        .expect("parse stdin input");

        match cli.command {
            Commands::Session {
                action:
                    SessionAction::Input {
                        target,
                        text,
                        input_stdin,
                        json,
                    },
            } => {
                assert_eq!(target.host.as_deref(), Some("host-a"));
                assert!(text.is_none());
                assert!(input_stdin);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn input_rejects_mixed_positional_and_stdin_sources() {
        let error =
            Cli::try_parse_from(["pohunek", "session", "input", "s-42", "secret", "--stdin"])
                .expect_err("mixed sources");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn new_rejects_mixed_option_and_stdin_sources() {
        let error = Cli::try_parse_from([
            "pohunek",
            "session",
            "new",
            "--input",
            "secret",
            "--input-stdin",
        ])
        .expect_err("mixed sources");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parses_observation_commands_and_runtime_cursors() {
        let output = Cli::try_parse_from([
            "pohunek",
            "session",
            "output",
            "s-42",
            "--runtime-id",
            "r-1",
            "--runtime-generation",
            "7",
            "--after-offset",
            "11",
            "--wait-ms",
            "500",
            "--json",
        ])
        .expect("parse output");
        assert!(matches!(
            output.command,
            Commands::Session {
                action: SessionAction::Output {
                    runtime_generation: Some(7),
                    after_offset: Some(11),
                    wait_ms: Some(500),
                    json: true,
                    ..
                }
            }
        ));

        let wait = Cli::try_parse_from([
            "pohunek",
            "session",
            "wait",
            "s-42",
            "--state",
            "done",
            "--activity",
            "blocked",
            "--timeout-ms",
            "1000",
        ])
        .expect("parse wait");
        assert!(matches!(
            wait.command,
            Commands::Session {
                action: SessionAction::Wait {
                    timeout_ms: 1000,
                    ..
                }
            }
        ));
    }

    #[test]
    fn session_wait_requires_timeout_and_enforces_shared_maximum() {
        let missing =
            Cli::try_parse_from(["pohunek", "session", "wait", "s-42", "--state", "done"])
                .expect_err("timeout is required");
        assert_eq!(
            missing.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let above = (protocol::MAX_SESSION_WAIT_MS + 1).to_string();
        let invalid = Cli::try_parse_from([
            "pohunek",
            "session",
            "wait",
            "s-42",
            "--state",
            "done",
            "--timeout-ms",
            &above,
        ])
        .expect_err("timeout above shared maximum");
        assert_eq!(invalid.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn parses_session_fork_target_name_and_json_flag() {
        let cli = Cli::try_parse_from([
            "pohunek",
            "session",
            "fork",
            "host-a/s-42",
            "--name",
            "forked review",
            "--json",
        ])
        .expect("parse");

        match cli.command {
            Commands::Session {
                action: SessionAction::Fork { target, name, json },
            } => {
                assert_eq!(target.session_id, "s-42");
                assert_eq!(target.host.as_deref(), Some("host-a"));
                assert_eq!(name.as_deref(), Some("forked review"));
                assert!(json);
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
        let cli = Cli::try_parse_from([
            "pohunek",
            "setup",
            "sway",
            "--print",
            "--keybind",
            "$mod+a",
            "--issue-keybind",
            "$mod+b",
        ])
        .expect("parse");

        match cli.command {
            Commands::Setup {
                action:
                    Some(SetupAction::Sway {
                        print,
                        keybind,
                        issue_keybind,
                        json,
                    }),
                ..
            } => {
                assert!(print);
                assert_eq!(keybind, "$mod+a");
                assert_eq!(issue_keybind, "$mod+b");
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn setup_sway_keybinds_default_to_mod_p_and_mod_i() {
        let cli = Cli::try_parse_from(["pohunek", "setup", "sway"]).expect("parse");

        match cli.command {
            Commands::Setup {
                action:
                    Some(SetupAction::Sway {
                        keybind,
                        issue_keybind,
                        ..
                    }),
                ..
            } => {
                assert_eq!(keybind, "$mod+p");
                assert_eq!(issue_keybind, "$mod+i");
            }
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
        let discover = Cli::try_parse_from(["pohunek", "host", "discover", "--refresh", "--json"])
            .expect("parse");
        assert!(matches!(
            discover.command,
            Commands::Host {
                action: HostAction::Discover {
                    refresh: true,
                    json: true,
                }
            }
        ));
        let list = Cli::try_parse_from(["pohunek", "host", "list"]).expect("parse");
        assert!(matches!(
            list.command,
            Commands::Host {
                action: HostAction::List { json: false, .. }
            }
        ));
    }

    // --- project ----------------------------------------------------------------

    #[test]
    fn parses_project_list_with_filters_and_json() {
        let cli = Cli::try_parse_from([
            "pohunek",
            "project",
            "list",
            "--filter",
            "source=manual",
            "--filter",
            "label=ui",
            "--json",
        ])
        .expect("parse");
        match cli.command {
            Commands::Project {
                action: ProjectAction::List { filters, json },
            } => {
                assert!(json);
                assert_eq!(
                    filters,
                    vec![
                        protocol::ProjectListFilter::Source(protocol::ProjectSource::Manual),
                        protocol::ProjectListFilter::Label("ui".to_owned()),
                    ]
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_project_add_with_path_name_and_base_branch() {
        let cli = Cli::try_parse_from([
            "pohunek",
            "project",
            "add",
            "/code/ui",
            "--name",
            "dashboard",
            "--base-branch",
            "develop",
        ])
        .expect("parse");
        match cli.command {
            Commands::Project {
                action:
                    ProjectAction::Add {
                        path,
                        name,
                        base_branch,
                        json,
                    },
            } => {
                assert_eq!(path.as_deref(), Some(std::path::Path::new("/code/ui")));
                assert_eq!(name.as_deref(), Some("dashboard"));
                assert_eq!(base_branch.as_deref(), Some("develop"));
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_project_add_without_path() {
        let cli = Cli::try_parse_from(["pohunek", "project", "add"]).expect("parse");
        match cli.command {
            Commands::Project {
                action: ProjectAction::Add { path, .. },
            } => assert_eq!(path, None, "no PATH means the cwd (filled by the command)"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_project_show_rename_rm_references() {
        let show = Cli::try_parse_from(["pohunek", "project", "show", "ui", "--json"])
            .expect("parse show");
        assert!(matches!(
            show.command,
            Commands::Project { action: ProjectAction::Show { reference, json: true } } if reference == "ui"
        ));

        let rename = Cli::try_parse_from(["pohunek", "project", "rename", "p-abc", "dashboard"])
            .expect("parse rename");
        match rename.command {
            Commands::Project {
                action:
                    ProjectAction::Rename {
                        reference, name, ..
                    },
            } => {
                assert_eq!(reference, "p-abc");
                assert_eq!(name, "dashboard");
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let rm = Cli::try_parse_from(["pohunek", "--host", "host-b", "project", "rm", "ui"])
            .expect("parse rm");
        assert!(matches!(
            rm.command,
            Commands::Project {
                action: ProjectAction::Rm { reference, prune_worktrees: false, json: false }
            } if reference == "ui"
        ));
        assert_eq!(rm.host, "host-b", "project rm honors the global --host");

        let prune = Cli::try_parse_from(["pohunek", "project", "rm", "ui", "--prune-worktrees"])
            .expect("parse rm --prune-worktrees");
        assert!(matches!(
            prune.command,
            Commands::Project {
                action: ProjectAction::Rm {
                    prune_worktrees: true,
                    ..
                }
            }
        ));
    }

    #[test]
    fn parses_project_prompt_with_required_name() {
        let cli = Cli::try_parse_from([
            "pohunek", "--host", "host-b", "project", "prompt", "ui", "issue", "--json",
        ])
        .expect("parse prompt");
        match cli.command {
            Commands::Project {
                action:
                    ProjectAction::Prompt {
                        reference,
                        name,
                        json,
                    },
            } => {
                assert_eq!(reference, "ui");
                assert_eq!(name, "issue");
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(
            cli.host, "host-b",
            "project prompt honors the global --host"
        );

        // The prompt name is required: `project prompt <ref>` with no name is a
        // usage error (there is no "default prompt").
        assert!(
            Cli::try_parse_from(["pohunek", "project", "prompt", "ui"]).is_err(),
            "missing prompt name must be a usage error"
        );
    }

    #[test]
    fn parses_project_action_with_required_name() {
        let cli = Cli::try_parse_from([
            "pohunek",
            "--host",
            "host-b",
            "project",
            "action",
            "ui",
            "review-pr",
            "--json",
        ])
        .expect("parse action");
        match cli.command {
            Commands::Project {
                action:
                    ProjectAction::Action {
                        reference,
                        name,
                        json,
                    },
            } => {
                assert_eq!(reference, "ui");
                assert_eq!(name, "review-pr");
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(
            cli.host, "host-b",
            "project action honors the global --host"
        );

        assert!(
            Cli::try_parse_from(["pohunek", "project", "action", "ui"]).is_err(),
            "missing action name must be a usage error"
        );
    }

    #[test]
    fn parses_project_actions() {
        let cli = Cli::try_parse_from([
            "pohunek", "--host", "host-b", "project", "actions", "ui", "--json",
        ])
        .expect("parse actions");
        match cli.command {
            Commands::Project {
                action: ProjectAction::Actions { reference, json },
            } => {
                assert_eq!(reference, "ui");
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(
            cli.host, "host-b",
            "project actions honors the global --host"
        );
    }
}

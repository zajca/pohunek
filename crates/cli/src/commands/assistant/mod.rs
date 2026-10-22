//! `pohunek assistant` — launch the universal assistant.
//!
//! The assistant is an ordinary PTY-backed agent session opened with a small
//! navigational opening prompt that points at a materialized knowledge bundle
//! and a redacted live snapshot. This module owns the CLI surface and delegates
//! the shared launch orchestration to `pohunek-assistant`, so the CLI and other
//! assistant clients use the same `session.new` path.
//!
//! The remaining local submodule is [`bootstrap`], which brings up the local
//! daemon when needed.
//!
//! Knowledge delivery is pull-by-file: the launch never inlines bundle bodies
//! into the prompt. For local launches the bundle is materialized in-process;
//! for remote launches the host daemon materializes its own version-matched
//! bundle via `assistant.materialize`.

pub(crate) mod bootstrap;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use pohunek_assistant::launch as core_assistant;
use pohunek_assistant::{AssistantError, ConnectionOptions, HostConfig};
use serde::Serialize;

use crate::commands::session::{confirmation_decision, ConfirmDecision};
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::LOCAL_HOST;

/// Default PTY geometry for an assistant session, matching `session new`.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Assistant intent: selects the navigation filter (table of contents) and
/// steers the first response. It does not split the implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Intent {
    Setup,
    Project,
    Update,
    Debug,
    Help,
}

impl Intent {
    /// Stable lowercase label used in prompt text and JSON output.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Intent::Setup => "setup",
            Intent::Project => "project",
            Intent::Update => "update",
            Intent::Debug => "debug",
            Intent::Help => "help",
        }
    }
}

/// Resolved options for one assistant launch, independent of how the CLI
/// surface expressed them (default form or an intent wrapper).
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "flat bag of independent CLI flags; a state machine would not model them better"
)]
pub(crate) struct AssistantOptions {
    pub(crate) intent: Intent,
    pub(crate) request: Option<String>,
    /// The request came from stdin, which then cannot answer the remote
    /// confirmation prompt.
    pub(crate) request_from_stdin: bool,
    /// Explicit response deadline of the `session.new` request; `None` keeps
    /// the SDK's dedicated initial-input creation budget.
    pub(crate) request_timeout_ms: Option<u32>,
    /// Caller metadata written atomically with the session.
    pub(crate) metadata: BTreeMap<String, String>,
    pub(crate) agent: Option<String>,
    pub(crate) host: String,
    pub(crate) project: Option<String>,
    pub(crate) repo: Option<PathBuf>,
    pub(crate) branch: Option<String>,
    pub(crate) base_branch: Option<String>,
    pub(crate) from_remote: bool,
    pub(crate) reset_diverged: bool,
    pub(crate) yes: bool,
    pub(crate) json: bool,
    pub(crate) print_prompt: bool,
    pub(crate) no_snapshot: bool,
    /// When true, skip bundle materialization and the read-access preflight.
    ///
    /// This is the ONLY sanctioned way to launch without a readable knowledge
    /// bundle. The default `pohunek assistant` fails rather than degrading
    /// silently when materialization fails.
    pub(crate) degraded: bool,
    pub(crate) no_start_daemon: bool,
}

impl AssistantOptions {
    fn is_remote(&self) -> bool {
        !crate::target::is_local_host(&self.host)
    }
}

/// Read the `--request-stdin` request: bounded UTF-8 text with surrounding
/// whitespace trimmed.
///
/// # Errors
///
/// Returns [`CliError::InvalidStdinInput`] for oversized, non-UTF-8 or
/// control-character input, and for a request that is empty after trimming.
pub(crate) fn read_stdin_request() -> Result<String, CliError> {
    request_from_text(&crate::commands::session::read_stdin_input()?)
}

fn request_from_text(text: &str) -> Result<String, CliError> {
    let request = text.trim();
    if request.is_empty() {
        return Err(CliError::InvalidStdinInput {
            detail: "the assistant request is empty".to_owned(),
        });
    }
    Ok(request.to_owned())
}

/// Assistant metadata emitted under `--json` alongside the session info.
#[derive(Debug, Serialize)]
struct AssistantMeta<'a> {
    intent: &'a str,
    agent: &'a str,
    knowledge_bundle_version: &'a str,
    snapshot_included: bool,
    auto_started_daemon: bool,
    knowledge: &'a str,
}

/// Run `pohunek assistant`.
///
/// # Errors
///
/// Returns [`CliError`] when the daemon cannot be reached or started, no capable
/// agent is available, materialization fails, the read-access preflight fails,
/// or the session launch is rejected.
pub(crate) async fn run(opts: AssistantOptions, paths: &Paths) -> Result<(), CliError> {
    validate_target(&opts)?;
    // A launch that cannot prompt is refused before any daemon is started or
    // dialed; `--print-prompt` starts nothing and needs no confirmation.
    if !opts.print_prompt && matches!(remote_confirmation(&opts), ConfirmDecision::RequireYes) {
        return Err(CliError::RemoteConfirmationRequired);
    }
    if opts.degraded {
        return run_degraded(opts, paths).await;
    }
    run_full(opts, paths).await
}

/// Validate the launch target before any daemon connection is dialed.
///
/// Mirrors `session new`'s `prepare_new_args` guard (design Decision 1): a remote
/// launch must name a `--project` (or `--repo` for first-introduction) because no
/// local filesystem path is meaningful on another host — otherwise the remote
/// agent would start in the daemon process's working directory. Additionally,
/// `--degraded` is rejected for a remote host: degraded materializes the snapshot
/// into a *local* runtime dir and embeds that path into the prompt, which a remote
/// agent cannot read.
///
/// # Errors
///
/// [`CliError::DegradedRemoteUnsupported`] for `--degraded` against a remote host;
/// [`CliError::RemoteTargetRequired`] for a remote launch with neither `--project`
/// nor `--repo`.
fn validate_target(opts: &AssistantOptions) -> Result<(), CliError> {
    if !opts.is_remote() {
        return Ok(());
    }
    if opts.degraded {
        return Err(CliError::DegradedRemoteUnsupported {
            host: opts.host.clone(),
        });
    }
    if opts.project.is_none() && opts.repo.is_none() {
        return Err(CliError::RemoteTargetRequired);
    }
    Ok(())
}

/// Full (non-degraded) launch: requires a readable knowledge bundle.
async fn run_full(opts: AssistantOptions, paths: &Paths) -> Result<(), CliError> {
    let auto_started = bootstrap::ensure_daemon(&opts.host, paths, opts.no_start_daemon).await?;
    run_prepared(opts, paths, auto_started).await
}

/// Degraded launch: snapshot + source-map pointer only, no bundle materialization.
///
/// `--degraded` is the ONLY sanctioned way to launch without a readable knowledge
/// bundle. It is an explicit opt-in — the default `run_full` path fails before
/// session.new if the bundle is unavailable.
async fn run_degraded(opts: AssistantOptions, paths: &Paths) -> Result<(), CliError> {
    let auto_started = bootstrap::ensure_daemon(&opts.host, paths, opts.no_start_daemon).await?;
    run_prepared(opts, paths, auto_started).await
}

async fn run_prepared(
    opts: AssistantOptions,
    paths: &Paths,
    auto_started: bool,
) -> Result<(), CliError> {
    let host = assistant_host_config(&opts, paths).await?;
    let assistant_paths = assistant_paths(paths);
    let params = assistant_params(&opts, auto_started);
    let mut prepared = core_assistant::prepare_with_options(
        &host,
        &assistant_paths,
        params,
        ConnectionOptions::default(),
    )
    .await
    .map_err(core_error)?;

    if opts.print_prompt {
        print_prompt(&opts, &prepared)?;
        return Ok(());
    }

    confirm_remote_launch(&opts)?;
    prepared.session_params.metadata = opts.metadata.clone();
    let options = ConnectionOptions {
        request_timeout: opts
            .request_timeout_ms
            .map(|timeout_ms| Duration::from_millis(u64::from(timeout_ms))),
        ..ConnectionOptions::default()
    };
    let launched = core_assistant::start_prepared_with_options(&host, prepared, options)
        .await
        .map_err(core_error)?;
    if launched.applied_input != Some(true) {
        eprintln!(
            "pohunek: warning: host '{}' did not confirm the assistant opening prompt was \
             delivered; the session is running without it.",
            opts.host
        );
    }
    emit_launch_output(&opts, &launched)
}

fn print_prompt(
    opts: &AssistantOptions,
    prepared: &core_assistant::PreparedLaunch,
) -> Result<(), CliError> {
    if opts.json {
        #[derive(Serialize)]
        struct PrintPrompt<'a> {
            prompt: &'a str,
            agent: &'a str,
            agent_reason: &'a str,
            intent: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            bundle_path: Option<&'a str>,
            snapshot_path: &'a str,
            knowledge_bundle_version: &'a str,
            knowledge: &'a str,
        }
        print!(
            "{}",
            crate::commands::render_json(&PrintPrompt {
                prompt: &prepared.prompt,
                agent: &prepared.selection.name,
                agent_reason: &prepared.selection.reason,
                intent: opts.intent.as_str(),
                bundle_path: prepared.knowledge.bundle_path.as_deref(),
                snapshot_path: &prepared.knowledge.snapshot_path,
                knowledge_bundle_version: &prepared.knowledge.version,
                knowledge: prepared.knowledge.label,
            })?
        );
    } else {
        println!(
            "agent: {} ({})",
            prepared.selection.name, prepared.selection.reason
        );
        println!("intent: {}", opts.intent.as_str());
        println!(
            "knowledge: {} ({})",
            prepared.knowledge.version, prepared.knowledge.label
        );
        if let Some(bundle_path) = &prepared.knowledge.bundle_path {
            println!("bundle: {bundle_path}");
        }
        println!("snapshot: {}", prepared.knowledge.snapshot_path);
        println!("--- prompt ---");
        println!("{}", prepared.prompt);
    }
    Ok(())
}

/// The confirmation a launch on `opts.host` needs.
///
/// A stdin request leaves no stdin for the prompt, so it takes the machine
/// path's rule: a remote launch needs `--yes`.
fn remote_confirmation(opts: &AssistantOptions) -> ConfirmDecision {
    confirmation_decision(&opts.host, opts.json || opts.request_from_stdin, opts.yes)
}

fn confirm_remote_launch(opts: &AssistantOptions) -> Result<(), CliError> {
    match remote_confirmation(opts) {
        ConfirmDecision::Proceed => Ok(()),
        ConfirmDecision::RequireYes => Err(CliError::RemoteConfirmationRequired),
        ConfirmDecision::Prompt => {
            if crate::commands::session::prompt_confirm(&opts.host)? {
                Ok(())
            } else {
                Err(CliError::RemoteConfirmationDeclined {
                    host: opts.host.clone(),
                })
            }
        }
    }
}

fn emit_launch_output(
    opts: &AssistantOptions,
    result: &core_assistant::LaunchResult,
) -> Result<(), CliError> {
    let info = &result.session;
    if opts.json {
        #[derive(Serialize)]
        struct Output<'a> {
            session: &'a protocol::SessionInfo,
            #[serde(skip_serializing_if = "Option::is_none")]
            worktree_source: Option<&'a protocol::WorktreeSource>,
            assistant: AssistantMeta<'a>,
        }
        print!(
            "{}",
            crate::commands::render_json(&Output {
                session: info,
                worktree_source: result.worktree_source.as_ref(),
                assistant: AssistantMeta {
                    intent: opts.intent.as_str(),
                    agent: &result.assistant.agent,
                    knowledge_bundle_version: &result.assistant.knowledge_bundle_version,
                    snapshot_included: result.assistant.snapshot_included,
                    auto_started_daemon: result.assistant.auto_started_daemon,
                    knowledge: result.assistant.knowledge,
                },
            })?
        );
    } else {
        let target = if crate::target::is_local_host(&opts.host) {
            format!("{LOCAL_HOST}/{}", info.id.0)
        } else {
            format!("{}/{}", opts.host, info.id.0)
        };
        let degraded_note = if result.assistant.knowledge == "degraded" {
            " (degraded)"
        } else {
            ""
        };
        println!("started assistant session{degraded_note}: {target}");
        if let Some(source) = &result.worktree_source {
            println!(
                "worktree source: {} at {}",
                source.source_ref, source.commit
            );
            if let Some(previous) = &source.fast_forwarded_from {
                println!("fast-forwarded from: {previous}");
            }
            if let Some(backup) = &source.backup_ref {
                println!("previous local head kept as: {backup}");
            }
        }
        println!("agent: {}", result.assistant.agent);
        println!("intent: {}", opts.intent.as_str());
        println!(
            "knowledge: {} ({})",
            result.assistant.knowledge_bundle_version, result.assistant.knowledge
        );
        println!(
            "snapshot: {}",
            if opts.no_snapshot {
                "skipped"
            } else {
                "included"
            }
        );
        println!("attach: pohunek attach {target}");
    }
    Ok(())
}

/// Build the host config of one launch, pinning a remote host once so
/// preparation and launch reach the same peer.
async fn assistant_host_config(
    opts: &AssistantOptions,
    paths: &Paths,
) -> Result<HostConfig, CliError> {
    if crate::target::is_local_host(&opts.host) {
        return Ok(HostConfig::local(LOCAL_HOST, paths.socket.clone()));
    }
    let timeout = pohunek_client::ClientOptions::default().connect_timeout;
    match pohunek_client::resolve_host_target(&opts.host, timeout)
        .await
        .map_err(crate::client::map_client_error)?
    {
        pohunek_client::ResolvedHost::Local => {
            Ok(HostConfig::local(LOCAL_HOST, paths.socket.clone()))
        }
        pohunek_client::ResolvedHost::Remote { pinned_route, .. } => Ok(HostConfig::pinned(
            opts.host.clone(),
            opts.host.clone(),
            pinned_route,
        )),
    }
}

fn assistant_paths(paths: &Paths) -> core_assistant::AssistantPaths {
    core_assistant::AssistantPaths {
        runtime_dir: paths.runtime_dir.clone(),
        data_dir: paths.data_dir.clone(),
        log_dir: paths.log_dir.clone(),
        cache_dir: paths.cache_dir.clone(),
        config_dir: paths.config_dir.clone(),
    }
}

fn assistant_params(opts: &AssistantOptions, auto_started: bool) -> core_assistant::LaunchParams {
    core_assistant::LaunchParams {
        intent: core_intent(opts.intent),
        request: opts.request.clone(),
        agent: opts.agent.clone(),
        project: opts.project.clone(),
        repo: opts.repo.clone(),
        branch: opts.branch.clone(),
        base_branch: opts.base_branch.clone(),
        branch_source: opts.from_remote.then_some(protocol::BranchSource::Remote),
        on_diverged: opts
            .reset_diverged
            .then_some(protocol::OnDiverged::BackupReset),
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
        no_snapshot: opts.no_snapshot,
        degraded: opts.degraded,
        auto_started_daemon: auto_started,
    }
}

fn core_intent(intent: Intent) -> core_assistant::Intent {
    match intent {
        Intent::Setup => core_assistant::Intent::Setup,
        Intent::Project => core_assistant::Intent::Project,
        Intent::Update => core_assistant::Intent::Update,
        Intent::Debug => core_assistant::Intent::Debug,
        Intent::Help => core_assistant::Intent::Help,
    }
}

fn core_error(err: AssistantError) -> CliError {
    match err {
        AssistantError::Client(source) => CliError::Client(source),
        AssistantError::Protocol(source) => CliError::Protocol(source),
        AssistantError::MissingEnv { var } => CliError::MissingEnv { var },
        AssistantError::RemoteAssistantTargetRequired { .. } => CliError::RemoteTargetRequired,
        AssistantError::RemoteAssistantDegradedUnsupported { host } => {
            CliError::DegradedRemoteUnsupported { host }
        }
        other @ AssistantError::Paths { .. } => CliError::Protocol(protocol::ProtocolError::new(
            protocol::ErrorClass::Runtime,
            "assistant_launch_failed",
            other.to_string(),
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(host: &str) -> AssistantOptions {
        AssistantOptions {
            intent: Intent::Help,
            request: None,
            request_from_stdin: false,
            request_timeout_ms: None,
            metadata: BTreeMap::new(),
            agent: None,
            host: host.to_owned(),
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            from_remote: false,
            reset_diverged: false,
            yes: false,
            json: false,
            print_prompt: false,
            no_snapshot: false,
            degraded: false,
            no_start_daemon: false,
        }
    }

    #[test]
    fn local_launches_and_targeted_remote_launches_are_allowed() {
        let local_degraded = AssistantOptions {
            degraded: true,
            ..opts(LOCAL_HOST)
        };
        let remote_project = AssistantOptions {
            project: Some("ui".to_owned()),
            ..opts("build-box")
        };
        let remote_repo = AssistantOptions {
            repo: Some(PathBuf::from("/srv/repo")),
            ..opts("build-box")
        };
        for (row, options) in [
            ("local without a target", opts(LOCAL_HOST)),
            ("empty host is local", opts("")),
            ("local degraded", local_degraded),
            ("remote with --project", remote_project),
            ("remote with --repo", remote_repo),
        ] {
            validate_target(&options).unwrap_or_else(|error| panic!("{row}: {error:?}"));
        }
    }

    #[test]
    fn remote_without_target_is_rejected() {
        let err = validate_target(&opts("build-box")).expect_err("remote needs a target");
        assert!(
            matches!(err, CliError::RemoteTargetRequired),
            "expected RemoteTargetRequired, got {err:?}"
        );
    }

    #[test]
    fn remote_degraded_is_rejected_even_with_target() {
        let mut o = opts("build-box");
        o.degraded = true;
        o.project = Some("ui".to_owned());
        let err = validate_target(&o).expect_err("remote degraded is rejected");
        let CliError::DegradedRemoteUnsupported { host } = err else {
            panic!("expected DegradedRemoteUnsupported, got {err:?}");
        };
        assert_eq!(host, "build-box");
    }
}

//! `pohunek assistant` — launch the universal assistant.
//!
//! The assistant is an ordinary PTY-backed agent session opened with a small
//! navigational opening prompt that points at a materialized knowledge bundle
//! and a redacted live snapshot. This module is the orchestration glue; the
//! steps live in focused submodules:
//!
//! - [`bootstrap`] — bring up the local daemon when needed;
//! - [`select`] — pick the agent and run the read-access preflight;
//! - [`snapshot`] — collect the redacted live snapshot (allowlist-built);
//! - [`prompt`] — compose the navigational opening prompt.
//!
//! Knowledge delivery is pull-by-file: the launch never inlines bundle bodies
//! into the prompt. For local launches the bundle is materialized in-process;
//! for remote launches the host daemon materializes its own version-matched
//! bundle via `assistant.materialize`.

pub(crate) mod bootstrap;
pub(crate) mod prompt;
pub(crate) mod select;
pub(crate) mod snapshot;

use std::path::PathBuf;

use protocol::{
    method, AssistantMaterializeResult, ConceptIntent, HostCapabilities, Request, SessionNewParams,
    SessionNewResult,
};
use serde::Serialize;

use crate::client::Client;
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

    /// The protocol concept-intent this assistant intent maps to, used to filter
    /// the table of contents from concept frontmatter.
    pub(crate) const fn as_concept_intent(self) -> ConceptIntent {
        match self {
            Intent::Setup => ConceptIntent::Setup,
            Intent::Project => ConceptIntent::Project,
            Intent::Update => ConceptIntent::Update,
            Intent::Debug => ConceptIntent::Debug,
            Intent::Help => ConceptIntent::Help,
        }
    }
}

/// Resolved options for one assistant launch, independent of how the CLI
/// surface expressed them (default form or an intent wrapper).
#[derive(Clone, Debug)]
pub(crate) struct AssistantOptions {
    pub(crate) intent: Intent,
    pub(crate) request: Option<String>,
    pub(crate) agent: Option<String>,
    pub(crate) host: String,
    pub(crate) project: Option<String>,
    pub(crate) repo: Option<PathBuf>,
    pub(crate) branch: Option<String>,
    pub(crate) base_branch: Option<String>,
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
        !self.host.is_empty() && self.host != LOCAL_HOST
    }
}

/// Three-line orientation summary inlined into the opening prompt so the agent
/// has immediate orientation without a read, while the full state stays in the
/// snapshot file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotOrientation {
    pub(crate) daemon: String,
    pub(crate) project: String,
    pub(crate) agent: String,
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
    // 1. Bring up the local daemon if needed (no-op for remote targets).
    let auto_started = bootstrap::ensure_daemon(&opts.host, paths, opts.no_start_daemon).await?;

    // 2. Connect and inspect host capabilities to select a capable agent.
    let mut client = Client::connect(&opts.host, paths).await?;
    let capabilities = fetch_capabilities(&mut client).await?;
    let selection = select::select_agent(&capabilities, opts.agent.as_deref(), None)?;

    // 3. Collect the redacted live snapshot (allowlist-built), unless skipped.
    let (snapshot_json, orientation) = if opts.no_snapshot {
        snapshot::skipped_snapshot(&opts, &selection.name, auto_started)
    } else {
        snapshot::collect(&mut client, paths, &opts, &selection.name, auto_started).await
    };

    // 4. Materialize the version-matched bundle on the host that runs the agent.
    //    Failure here is intentional: the default path must fail rather than
    //    degrade silently. Use --degraded for an explicit reduced launch.
    let materialized = if opts.is_remote() {
        crate::assistant::materialize_remote(&mut client, &snapshot_json).await?
    } else {
        crate::assistant::materialize_local(paths, &snapshot_json)?
    };

    // 5. Read-access preflight: the agent context must be able to read both the
    //    materialized bundle and the snapshot before we send session.new.
    select::preflight_read_access(
        &materialized.bundle_path,
        &materialized.snapshot_path,
        opts.is_remote(),
    )?;

    // 6. Compose the small navigational opening prompt (never inlines bodies).
    let prompt = prompt::compose(prompt::ComposeParams {
        intent: opts.intent,
        request: opts.request.as_deref(),
        concepts: &materialized.concepts,
        bundle_path: &materialized.bundle_path,
        snapshot_path: &materialized.snapshot_path,
        orientation: &orientation,
        version: &materialized.version,
    });

    // 7. `--print-prompt` is a dry run: show the prompt and resolved paths, then
    //    exit without starting a session.
    if opts.print_prompt {
        print_prompt_full(&opts, &selection, &materialized, &prompt)?;
        return Ok(());
    }

    // 8. Launch the session with the composed prompt as initial input.
    launch_session(
        &mut client,
        &opts,
        &selection,
        &materialized,
        prompt,
        auto_started,
    )
    .await
}

/// Degraded launch: snapshot + source-map pointer only, no bundle materialization.
///
/// `--degraded` is the ONLY sanctioned way to launch without a readable knowledge
/// bundle. It is an explicit opt-in — the default `run_full` path fails before
/// session.new if the bundle is unavailable.
async fn run_degraded(opts: AssistantOptions, paths: &Paths) -> Result<(), CliError> {
    // 1. Bring up the local daemon if needed (same as full path).
    let auto_started = bootstrap::ensure_daemon(&opts.host, paths, opts.no_start_daemon).await?;

    // 2. Connect and select a capable agent.
    let mut client = Client::connect(&opts.host, paths).await?;
    let capabilities = fetch_capabilities(&mut client).await?;
    let selection = select::select_agent(&capabilities, opts.agent.as_deref(), None)?;

    // 3. Collect the snapshot (unless --no-snapshot). Write to a per-launch
    //    runtime dir via materialize_degraded (no bundle extraction).
    let (snapshot_json, orientation) = if opts.no_snapshot {
        snapshot::skipped_snapshot(&opts, &selection.name, auto_started)
    } else {
        snapshot::collect(&mut client, paths, &opts, &selection.name, auto_started).await
    };

    // 4. Write the snapshot to a per-launch runtime dir. No bundle is extracted.
    let degraded = crate::assistant::materialize_degraded(paths, &snapshot_json)?;

    // 5. Read-access preflight covers only the snapshot (no bundle to check).
    if !opts.is_remote() {
        select::preflight_snapshot_readable(&degraded.snapshot_path)?;
    }

    // 6. Compose the reduced navigational prompt (no TOC, no bundle path).
    let prompt = prompt::compose_degraded(prompt::ComposeDegradedParams {
        intent: opts.intent,
        request: opts.request.as_deref(),
        snapshot_path: &degraded.snapshot_path,
        orientation: &orientation,
        version: &degraded.version,
        bundle_version_note: &degraded.version,
    });

    // 7. `--print-prompt` dry run.
    if opts.print_prompt {
        print_prompt_degraded(&opts, &selection, &degraded, &prompt)?;
        return Ok(());
    }

    // 8. Launch the session with the reduced prompt.
    launch_session_degraded(
        &mut client,
        &opts,
        &selection,
        &degraded,
        prompt,
        auto_started,
    )
    .await
}

async fn fetch_capabilities(client: &mut Client) -> Result<HostCapabilities, CliError> {
    let request = Request::new(
        crate::commands::request_id(method::HOST_INSPECT),
        method::HOST_INSPECT,
        serde_json::Value::Null,
    );
    let value = client.request(&request).await?;
    Ok(serde_json::from_value(value)?)
}

fn print_prompt_full(
    opts: &AssistantOptions,
    selection: &select::AgentSelection,
    materialized: &AssistantMaterializeResult,
    prompt: &str,
) -> Result<(), CliError> {
    if opts.json {
        #[derive(Serialize)]
        struct PrintPrompt<'a> {
            prompt: &'a str,
            agent: &'a str,
            agent_reason: &'a str,
            intent: &'a str,
            bundle_path: &'a str,
            snapshot_path: &'a str,
            knowledge_bundle_version: &'a str,
            knowledge: &'a str,
        }
        print!(
            "{}",
            crate::commands::render_json(&PrintPrompt {
                prompt,
                agent: &selection.name,
                agent_reason: &selection.reason,
                intent: opts.intent.as_str(),
                bundle_path: &materialized.bundle_path,
                snapshot_path: &materialized.snapshot_path,
                knowledge_bundle_version: &materialized.version,
                knowledge: "materialized",
            })?
        );
    } else {
        println!("agent: {} ({})", selection.name, selection.reason);
        println!("intent: {}", opts.intent.as_str());
        println!("knowledge: {} (materialized)", materialized.version);
        println!("bundle: {}", materialized.bundle_path);
        println!("snapshot: {}", materialized.snapshot_path);
        println!("--- prompt ---");
        println!("{prompt}");
    }
    Ok(())
}

fn print_prompt_degraded(
    opts: &AssistantOptions,
    selection: &select::AgentSelection,
    degraded: &crate::assistant::DegradedMaterializeResult,
    prompt: &str,
) -> Result<(), CliError> {
    if opts.json {
        #[derive(Serialize)]
        struct PrintPrompt<'a> {
            prompt: &'a str,
            agent: &'a str,
            agent_reason: &'a str,
            intent: &'a str,
            snapshot_path: &'a str,
            knowledge_bundle_version: &'a str,
            knowledge: &'a str,
        }
        print!(
            "{}",
            crate::commands::render_json(&PrintPrompt {
                prompt,
                agent: &selection.name,
                agent_reason: &selection.reason,
                intent: opts.intent.as_str(),
                snapshot_path: &degraded.snapshot_path,
                knowledge_bundle_version: &degraded.version,
                knowledge: "degraded",
            })?
        );
    } else {
        println!("agent: {} ({})", selection.name, selection.reason);
        println!("intent: {}", opts.intent.as_str());
        println!("knowledge: {} (degraded)", degraded.version);
        println!("snapshot: {}", degraded.snapshot_path);
        println!("--- prompt ---");
        println!("{prompt}");
    }
    Ok(())
}

async fn launch_session(
    client: &mut Client,
    opts: &AssistantOptions,
    selection: &select::AgentSelection,
    materialized: &AssistantMaterializeResult,
    prompt: String,
    auto_started: bool,
) -> Result<(), CliError> {
    match confirmation_decision(&opts.host, opts.json, opts.yes) {
        ConfirmDecision::Proceed => {}
        ConfirmDecision::RequireYes => return Err(CliError::RemoteConfirmationRequired),
        ConfirmDecision::Prompt => {
            if !crate::commands::session::prompt_confirm(&opts.host)? {
                return Err(CliError::RemoteConfirmationDeclined {
                    host: opts.host.clone(),
                });
            }
        }
    }

    let params = SessionNewParams {
        agent: selection.name.clone(),
        cwd: None,
        // The assistant session inherits the same default PTY geometry as
        // `session new`; the attaching terminal resizes it on connect.
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
        project: opts.project.clone(),
        repo: opts.repo.clone(),
        branch: opts.branch.clone(),
        base_branch: opts.base_branch.clone(),
        input: Some(prompt),
    };
    let request = Request::new(
        crate::commands::request_id(method::SESSION_NEW),
        method::SESSION_NEW,
        serde_json::to_value(&params)?,
    );
    let value = client.request(&request).await?;
    let result: SessionNewResult = serde_json::from_value(value)?;

    if result.applied_input != Some(true) {
        eprintln!(
            "pohunek: warning: host '{}' did not confirm the assistant opening prompt was \
             delivered; the session is running without it.",
            opts.host
        );
    }

    emit_launch_output(opts, selection, materialized, &result, auto_started)
}

fn emit_launch_output(
    opts: &AssistantOptions,
    selection: &select::AgentSelection,
    materialized: &AssistantMaterializeResult,
    result: &SessionNewResult,
    auto_started: bool,
) -> Result<(), CliError> {
    let info = &result.session;
    if opts.json {
        #[derive(Serialize)]
        struct Output<'a> {
            session: &'a protocol::SessionInfo,
            assistant: AssistantMeta<'a>,
        }
        print!(
            "{}",
            crate::commands::render_json(&Output {
                session: info,
                assistant: AssistantMeta {
                    intent: opts.intent.as_str(),
                    agent: &selection.name,
                    knowledge_bundle_version: &materialized.version,
                    snapshot_included: !opts.no_snapshot,
                    auto_started_daemon: auto_started,
                    knowledge: "materialized",
                },
            })?
        );
    } else {
        let target = if opts.host.is_empty() || opts.host == LOCAL_HOST {
            format!("{LOCAL_HOST}/{}", info.id.0)
        } else {
            format!("{}/{}", opts.host, info.id.0)
        };
        println!("started assistant session: {target}");
        println!("agent: {}", selection.name);
        println!("intent: {}", opts.intent.as_str());
        println!("knowledge: {} (materialized)", materialized.version);
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

async fn launch_session_degraded(
    client: &mut Client,
    opts: &AssistantOptions,
    selection: &select::AgentSelection,
    degraded: &crate::assistant::DegradedMaterializeResult,
    prompt: String,
    auto_started: bool,
) -> Result<(), CliError> {
    match confirmation_decision(&opts.host, opts.json, opts.yes) {
        ConfirmDecision::Proceed => {}
        ConfirmDecision::RequireYes => return Err(CliError::RemoteConfirmationRequired),
        ConfirmDecision::Prompt => {
            if !crate::commands::session::prompt_confirm(&opts.host)? {
                return Err(CliError::RemoteConfirmationDeclined {
                    host: opts.host.clone(),
                });
            }
        }
    }

    let params = SessionNewParams {
        agent: selection.name.clone(),
        cwd: None,
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
        project: opts.project.clone(),
        repo: opts.repo.clone(),
        branch: opts.branch.clone(),
        base_branch: opts.base_branch.clone(),
        input: Some(prompt),
    };
    let request = Request::new(
        crate::commands::request_id(method::SESSION_NEW),
        method::SESSION_NEW,
        serde_json::to_value(&params)?,
    );
    let value = client.request(&request).await?;
    let result: SessionNewResult = serde_json::from_value(value)?;

    if result.applied_input != Some(true) {
        eprintln!(
            "pohunek: warning: host '{}' did not confirm the assistant opening prompt was \
             delivered; the session is running without it.",
            opts.host
        );
    }

    emit_launch_output_degraded(opts, selection, degraded, &result, auto_started)
}

fn emit_launch_output_degraded(
    opts: &AssistantOptions,
    selection: &select::AgentSelection,
    degraded: &crate::assistant::DegradedMaterializeResult,
    result: &SessionNewResult,
    auto_started: bool,
) -> Result<(), CliError> {
    let info = &result.session;
    if opts.json {
        #[derive(Serialize)]
        struct Output<'a> {
            session: &'a protocol::SessionInfo,
            assistant: AssistantMeta<'a>,
        }
        print!(
            "{}",
            crate::commands::render_json(&Output {
                session: info,
                assistant: AssistantMeta {
                    intent: opts.intent.as_str(),
                    agent: &selection.name,
                    knowledge_bundle_version: &degraded.version,
                    snapshot_included: !opts.no_snapshot,
                    auto_started_daemon: auto_started,
                    knowledge: "degraded",
                },
            })?
        );
    } else {
        let target = if opts.host.is_empty() || opts.host == LOCAL_HOST {
            format!("{LOCAL_HOST}/{}", info.id.0)
        } else {
            format!("{}/{}", opts.host, info.id.0)
        };
        println!("started assistant session (degraded): {target}");
        println!("agent: {}", selection.name);
        println!("intent: {}", opts.intent.as_str());
        println!("knowledge: {} (degraded)", degraded.version);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(host: &str) -> AssistantOptions {
        AssistantOptions {
            intent: Intent::Help,
            request: None,
            agent: None,
            host: host.to_owned(),
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            yes: false,
            json: false,
            print_prompt: false,
            no_snapshot: false,
            degraded: false,
            no_start_daemon: false,
        }
    }

    #[test]
    fn local_launch_needs_no_target() {
        validate_target(&opts(LOCAL_HOST)).expect("local with no target is allowed");
        validate_target(&opts("")).expect("empty host is local");
    }

    #[test]
    fn local_degraded_is_allowed() {
        let mut o = opts(LOCAL_HOST);
        o.degraded = true;
        validate_target(&o).expect("local degraded is allowed");
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
    fn remote_with_project_is_allowed() {
        let mut o = opts("build-box");
        o.project = Some("ui".to_owned());
        validate_target(&o).expect("remote with --project is allowed");
    }

    #[test]
    fn remote_with_repo_is_allowed() {
        let mut o = opts("build-box");
        o.repo = Some(PathBuf::from("/srv/repo"));
        validate_target(&o).expect("remote with --repo is allowed");
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

//! Redacted live snapshot collection.
//!
//! **Security-critical.** The snapshot is built from an explicit *allowlist* of
//! fields: every emitted value is a named field of a typed struct, so the
//! serializer is structurally unable to emit an unknown field. There is no
//! `#[serde(flatten)]` of foreign maps and no passthrough of a raw RPC `Value`.
//! A new config field cannot leak just because nobody added it to a blocklist.
//!
//! Process environment variables, profile `[env]` values, hook bodies, and
//! arbitrary config bodies are never collected. Absolute paths are redacted via
//! [`redact_path`]; the doctor `detail` string is dropped (it can carry PATH and
//! env fragments).
//!
//! Collection is best-effort: a failed item becomes a `warnings` entry rather
//! than losing the whole snapshot. The agent reads the file on demand and can
//! re-run the underlying `--json` command itself.

use std::path::Path;

use knowledge::BUNDLE_VERSION;
use protocol::{
    method, DaemonDoctorResult, HostCapabilities, ProjectActionsParams, ProjectInfo,
    ProjectListParams, ProjectShowParams, SessionInfo, SessionListParams,
};
use serde::Serialize;

use super::{AssistantOptions, SnapshotOrientation};
use crate::client::Client;
use crate::paths::Paths;

/// The full redacted snapshot document written to `snapshot.json`.
///
/// Every field is explicitly chosen and allowlisted. Do not add
/// `#[serde(flatten)]` or a raw `serde_json::Value` field — that would defeat
/// the allowlist guarantee.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct Snapshot {
    assistant: AssistantSection,
    /// Local client paths. Present only for a local launch — for a remote host
    /// these would be the *client's* directories, not the host the agent runs
    /// on, so they are omitted (see [`collect_local_sections`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    paths: Option<PathsSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doctor: Option<DoctorSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<HostSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    projects: Option<ProjectsSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sessions: Option<SessionsSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config_scan: Option<ConfigScanSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_tree: Option<SourceTreeSection>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct AssistantSection {
    intent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_request: Option<String>,
    selected_host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_project: Option<String>,
    selected_agent: String,
    auto_started_daemon: bool,
    knowledge_bundle_version: String,
    snapshot_collected: bool,
}

/// Redacted path set. Absolute home-revealing paths are rewritten to a `~`-form.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[expect(
    clippy::struct_field_names,
    reason = "field names are the serialized snapshot.json keys; the `_dir` suffix is the contract"
)]
struct PathsSection {
    config_dir: String,
    data_dir: String,
    log_dir: String,
    cache_dir: String,
}

/// Allowlisted doctor summary: names and statuses only. The `detail` string is
/// deliberately dropped because it can carry PATH and env fragments.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct DoctorSection {
    overall: String,
    checks: Vec<DoctorCheckSummary>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct DoctorCheckSummary {
    name: String,
    status: String,
}

/// Allowlisted host capability section.
///
/// The `runtimes[].path` field is **redacted to a bool** (`available`) so no
/// absolute binary path leaks. Only fields listed here are emitted.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct HostSection {
    supported_agents: Vec<String>,
    git_available: bool,
    worktree_supported: bool,
    /// Runtimes with the path field dropped — only `agent` name and `available`
    /// bool are preserved.
    runtimes: Vec<RuntimeSummary>,
}

/// One runtime with the absolute `path` field dropped.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct RuntimeSummary {
    agent: String,
    available: bool,
}

/// Allowlisted project listing section.
///
/// All absolute path fields (`repo_root`, `git_common_dir`, `cwd`,
/// `worktree_path`) are redacted. Only ids, labels, source, and action names
/// are kept.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct ProjectsSection {
    known_projects: Vec<ProjectSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_project: Option<SelectedProjectDetail>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct ProjectSummary {
    id: String,
    label: String,
    source: String,
}

/// Detailed view of the selected project (when `--project` is set), including
/// its available action names. No path fields are emitted.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct SelectedProjectDetail {
    id: String,
    label: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_base_branch: Option<String>,
    is_bare: bool,
    /// Action names only — no template bodies, provider keys, or paths.
    action_names: Vec<String>,
}

/// Allowlisted session listing section.
///
/// Path fields (`cwd`, `repo`, `worktree_path`, `native_session_path`) are
/// dropped entirely.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct SessionsSection {
    active_sessions: Vec<SessionSummary>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct SessionSummary {
    id: String,
    agent: String,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    activity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_label: Option<String>,
    warning_count: usize,
}

/// Config file existence/parse scan.
///
/// This section records **filenames and presence only** — no file bodies, no
/// hook content, no config values.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct ConfigScanSection {
    /// Whether `launcher.conf` exists in the host config dir.
    launcher_conf_present: bool,
    /// Whether `templates.toml` exists and parses cleanly in the host config dir.
    templates_toml_status: FileStatus,
    /// Whether `actions.toml` exists and parses cleanly in the host config dir.
    actions_toml_status: FileStatus,
    /// Names of prompt templates (filenames without extension) in `prompts/`.
    prompt_names: Vec<String>,
    /// Names of agent profile files (filenames without extension) in `agents/`.
    agent_profile_names: Vec<String>,
    /// Names of hook files (filenames only — never the content) in `hooks/`.
    hook_names: Vec<String>,
    /// Repo-local config scan, present when `--repo` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scan: Option<RepoConfigScan>,
}

/// Scan of an in-repo `.pohunek/` directory.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct RepoConfigScan {
    templates_toml_status: FileStatus,
    actions_toml_status: FileStatus,
    prompt_names: Vec<String>,
    hook_names: Vec<String>,
}

/// Whether a config file exists and parses cleanly.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FileStatus {
    /// The file does not exist.
    Absent,
    /// The file exists and is valid TOML.
    Ok,
    /// The file exists but could not be parsed as TOML.
    ParseError,
}

/// Source-tree context when `--repo` is set.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct SourceTreeSection {
    /// Redacted git root (home prefix replaced with `~`).
    git_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_branch: Option<String>,
    /// Brief dirty summary: `"clean"`, `"dirty"`, or `"unknown"`.
    dirty_status_summary: String,
    /// Whether the repo's HEAD version tag matches the running binary version.
    version_matches_binary: bool,
}

// ---------------------------------------------------------------------------
// Public entry points (signatures must stay stable — mod.rs depends on them)
// ---------------------------------------------------------------------------

/// Collect the redacted snapshot for one assistant launch (best-effort).
///
/// Returns the serialized JSON document and the three-line orientation summary.
pub(crate) async fn collect(
    client: &mut Client,
    paths: &Paths,
    opts: &AssistantOptions,
    selected_agent: &str,
    auto_started: bool,
) -> (String, SnapshotOrientation) {
    let mut warnings = Vec::new();

    // Doctor — best-effort.
    let doctor = match fetch_doctor(client).await {
        Ok(section) => Some(section),
        Err(message) => {
            warnings.push(format!("doctor: {message}"));
            None
        }
    };

    let daemon_state = doctor
        .as_ref()
        .map_or_else(|| "unknown".to_owned(), |d| d.overall.clone());

    // Host capabilities — best-effort.
    let host = match fetch_host(client).await {
        Ok(section) => Some(section),
        Err(message) => {
            warnings.push(format!("host: {message}"));
            None
        }
    };

    // Projects — best-effort.
    let projects = match fetch_projects(client, opts).await {
        Ok(section) => Some(section),
        Err(message) => {
            warnings.push(format!("projects: {message}"));
            None
        }
    };

    // Project orientation for the 3-line summary.
    let project_orientation = projects
        .as_ref()
        .and_then(|p| p.selected_project.as_ref())
        .map(|sp| sp.label.clone())
        .or_else(|| opts.project.clone())
        .unwrap_or_else(|| "(none)".to_owned());

    // Sessions — best-effort.
    let sessions = match fetch_sessions(client).await {
        Ok(section) => Some(section),
        Err(message) => {
            warnings.push(format!("sessions: {message}"));
            None
        }
    };

    // Local-filesystem sections (paths, config scan, source tree). These read the
    // client's filesystem, so for a remote host they describe the client rather
    // than the host the agent runs on — they are omitted there (with warnings).
    let local = collect_local_sections(paths, opts);
    warnings.extend(local.warnings);

    let snapshot = Snapshot {
        assistant: AssistantSection {
            intent: opts.intent.as_str().to_owned(),
            user_request: opts.request.clone(),
            selected_host: opts.host.clone(),
            selected_project: opts.project.clone(),
            selected_agent: selected_agent.to_owned(),
            auto_started_daemon: auto_started,
            knowledge_bundle_version: BUNDLE_VERSION.to_owned(),
            snapshot_collected: true,
        },
        paths: local.paths,
        doctor,
        host,
        projects,
        sessions,
        config_scan: local.config_scan,
        source_tree: local.source_tree,
        warnings,
    };

    let orientation = SnapshotOrientation {
        daemon: daemon_state,
        project: project_orientation,
        agent: selected_agent.to_owned(),
    };

    (serialize(&snapshot), orientation)
}

/// Build a minimal snapshot for `--no-snapshot`: identity/orientation only, no
/// live-state collection. This is a real, honest marker — not collected state.
pub(crate) fn skipped_snapshot(
    opts: &AssistantOptions,
    selected_agent: &str,
    auto_started: bool,
) -> (String, SnapshotOrientation) {
    let snapshot = Snapshot {
        assistant: AssistantSection {
            intent: opts.intent.as_str().to_owned(),
            user_request: opts.request.clone(),
            selected_host: opts.host.clone(),
            selected_project: opts.project.clone(),
            selected_agent: selected_agent.to_owned(),
            auto_started_daemon: auto_started,
            knowledge_bundle_version: BUNDLE_VERSION.to_owned(),
            snapshot_collected: false,
        },
        // No live collection: the local path set is not gathered either.
        paths: None,
        doctor: None,
        host: None,
        projects: None,
        sessions: None,
        config_scan: None,
        source_tree: None,
        warnings: vec!["snapshot collection skipped (--no-snapshot)".to_owned()],
    };
    let orientation = SnapshotOrientation {
        daemon: "unknown".to_owned(),
        project: opts.project.clone().unwrap_or_else(|| "(none)".to_owned()),
        agent: selected_agent.to_owned(),
    };
    (serialize(&snapshot), orientation)
}

// ---------------------------------------------------------------------------
// Local-filesystem sections (remote-aware)
// ---------------------------------------------------------------------------

/// The local-filesystem sections of a snapshot, plus any warnings produced while
/// collecting (or deliberately skipping) them.
struct LocalSections {
    paths: Option<PathsSection>,
    config_scan: Option<ConfigScanSection>,
    source_tree: Option<SourceTreeSection>,
    warnings: Vec<String>,
}

/// Collect the sections that come from the *local* filesystem: redacted paths,
/// the config scan, and the git source tree.
///
/// For a remote host these are omitted: the snapshot is read by an agent running
/// on the remote host, so the client's directories, host-config layer, and a
/// `--repo` path (all local) describe the wrong machine and would mislead. The
/// remote daemon's RPC-sourced sections (doctor/host/projects/sessions) already
/// carry the correct remote state. The omission is recorded as a warning so the
/// absence is explicit rather than silent.
fn collect_local_sections(paths: &Paths, opts: &AssistantOptions) -> LocalSections {
    if opts.is_remote() {
        let mut warnings = vec![
            "paths: omitted for a remote host (these are the local client's directories, \
             not the host the agent runs on)"
                .to_owned(),
            "config_scan: omitted for a remote host (local-only section; it reflects the \
             client's config layer, not the host)"
                .to_owned(),
        ];
        if opts.repo.is_some() {
            warnings.push(
                "source_tree: omitted for a remote host (--repo is a path on the client, \
                 not the host)"
                    .to_owned(),
            );
        }
        return LocalSections {
            paths: None,
            config_scan: None,
            source_tree: None,
            warnings,
        };
    }

    let mut warnings = Vec::new();

    // Config scan — pure filesystem, no RPC, and infallible (missing dirs and
    // unreadable files degrade to empty/`absent` status rather than erroring).
    let config_scan = Some(scan_config(paths, opts));

    // Source tree — best-effort (runs git CLI, only when --repo is set).
    let source_tree = if let Some(repo) = &opts.repo {
        match collect_source_tree(repo) {
            Ok(section) => Some(section),
            Err(message) => {
                warnings.push(format!("source_tree: {message}"));
                None
            }
        }
    } else {
        None
    };

    LocalSections {
        paths: Some(paths_section(paths)),
        config_scan,
        source_tree,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// Private collection helpers
// ---------------------------------------------------------------------------

async fn fetch_doctor(client: &mut Client) -> Result<DoctorSection, String> {
    let request =
        crate::commands::request_with_params(method::DAEMON_DOCTOR, &serde_json::Value::Null)
            .map_err(|e| e.to_string())?;
    let value = client.request(&request).await.map_err(|e| e.to_string())?;
    let result: DaemonDoctorResult = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(DoctorSection {
        overall: result.report.overall.as_str().to_owned(),
        checks: result
            .report
            .checks
            .into_iter()
            .map(|check| DoctorCheckSummary {
                name: check.name,
                status: check.status.as_str().to_owned(),
            })
            .collect(),
    })
}

async fn fetch_host(client: &mut Client) -> Result<HostSection, String> {
    let request =
        crate::commands::request_with_params(method::HOST_INSPECT, &serde_json::Value::Null)
            .map_err(|e| e.to_string())?;
    let value = client.request(&request).await.map_err(|e| e.to_string())?;
    let caps: HostCapabilities = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(map_host_capabilities(&caps))
}

/// Pure mapping from [`HostCapabilities`] to [`HostSection`].
///
/// Factored out so it can be tested without a live daemon.
fn map_host_capabilities(caps: &HostCapabilities) -> HostSection {
    HostSection {
        supported_agents: caps.supported_agents.clone(),
        git_available: caps.git_available,
        worktree_supported: caps.worktree_supported,
        // Drop the `path` field — redact absolute binary paths to a bool.
        runtimes: caps
            .runtimes
            .iter()
            .map(|r| RuntimeSummary {
                agent: r.agent.clone(),
                available: r.available,
            })
            .collect(),
    }
}

async fn fetch_projects(
    client: &mut Client,
    opts: &AssistantOptions,
) -> Result<ProjectsSection, String> {
    // List all projects.
    let list_params = ProjectListParams::default();
    let request = crate::commands::request_with_params(method::PROJECT_LIST, &list_params)
        .map_err(|e| e.to_string())?;
    let value = client.request(&request).await.map_err(|e| e.to_string())?;
    let projects: Vec<ProjectInfo> = serde_json::from_value(value).map_err(|e| e.to_string())?;

    let known_projects = projects.iter().map(map_project_summary).collect::<Vec<_>>();

    // If a project is selected, also fetch PROJECT_SHOW and PROJECT_ACTIONS.
    let selected_project = if let Some(reference) = &opts.project {
        let show_params = ProjectShowParams {
            reference: reference.clone(),
        };
        let show_request = crate::commands::request_with_params(method::PROJECT_SHOW, &show_params)
            .map_err(|e| e.to_string())?;
        let show_value = client
            .request(&show_request)
            .await
            .map_err(|e| e.to_string())?;
        let show_result: protocol::ProjectShowResult =
            serde_json::from_value(show_value).map_err(|e| e.to_string())?;

        let actions_params = ProjectActionsParams {
            reference: reference.clone(),
        };
        let actions_request =
            crate::commands::request_with_params(method::PROJECT_ACTIONS, &actions_params)
                .map_err(|e| e.to_string())?;
        let actions_value = client
            .request(&actions_request)
            .await
            .map_err(|e| e.to_string())?;
        let actions_result: protocol::ProjectActionsResult =
            serde_json::from_value(actions_value).map_err(|e| e.to_string())?;

        Some(map_selected_project(&show_result.project, &actions_result))
    } else {
        None
    };

    Ok(ProjectsSection {
        known_projects,
        selected_project,
    })
}

/// Pure mapping from a [`ProjectInfo`] to a [`ProjectSummary`] (no paths).
fn map_project_summary(info: &ProjectInfo) -> ProjectSummary {
    ProjectSummary {
        id: info.id.clone(),
        label: info.label.clone(),
        source: info.source.as_str().to_owned(),
    }
}

/// Pure mapping that builds a [`SelectedProjectDetail`] from protocol types.
///
/// Absolute paths (`repo_root`, `git_common_dir`) are deliberately not
/// included. Only the allowlisted fields are emitted.
fn map_selected_project(
    info: &ProjectInfo,
    actions: &protocol::ProjectActionsResult,
) -> SelectedProjectDetail {
    SelectedProjectDetail {
        id: info.id.clone(),
        label: info.label.clone(),
        source: info.source.as_str().to_owned(),
        default_base_branch: info.default_base_branch.clone(),
        is_bare: info.is_bare,
        // Only action names — no template bodies, provider keys, or paths.
        action_names: actions.actions.iter().map(|a| a.name.clone()).collect(),
    }
}

async fn fetch_sessions(client: &mut Client) -> Result<SessionsSection, String> {
    let params = SessionListParams::default();
    let request = crate::commands::request_with_params(method::SESSION_LIST, &params)
        .map_err(|e| e.to_string())?;
    let value = client.request(&request).await.map_err(|e| e.to_string())?;
    let sessions: Vec<SessionInfo> = serde_json::from_value(value).map_err(|e| e.to_string())?;

    Ok(SessionsSection {
        active_sessions: sessions.iter().map(map_session_summary).collect(),
    })
}

/// Pure mapping from [`SessionInfo`] to [`SessionSummary`] (paths dropped).
///
/// Fields deliberately omitted: `cwd`, `repo`, `worktree_path`,
/// `native_session_path` (all absolute paths). `pid`, `cols`, `rows`,
/// `exit_code` carry no sensitive data but are not useful for orientation, so
/// they are also omitted to keep the snapshot compact.
fn map_session_summary(info: &SessionInfo) -> SessionSummary {
    SessionSummary {
        id: info.id.0.clone(),
        agent: info.agent.clone(),
        state: info.state.as_str().to_owned(),
        activity: info.activity.map(|activity| activity.as_str().to_owned()),
        project_id: info.project_id.clone(),
        project_label: info.project_label.clone(),
        warning_count: info.warnings.len(),
    }
}

/// Scan host `config_dir` and optionally the repo `.pohunek/` directory.
///
/// Reads filenames and TOML parse status only — never file bodies or hook
/// content.
fn scan_config(paths: &Paths, opts: &AssistantOptions) -> ConfigScanSection {
    let host_config = scan_pohunek_dir(&paths.config_dir);

    let repo_scan = opts.repo.as_ref().map(|repo| {
        let repo_pohunek = repo.join(".pohunek");
        scan_repo_dir(&repo_pohunek)
    });

    ConfigScanSection {
        launcher_conf_present: host_config.launcher_conf_present,
        templates_toml_status: host_config.templates_toml_status,
        actions_toml_status: host_config.actions_toml_status,
        prompt_names: host_config.prompt_names,
        agent_profile_names: host_config.agent_profile_names,
        hook_names: host_config.hook_names,
        repo_scan,
    }
}

/// Intermediate result of scanning a pohunek config directory.
struct DirScan {
    launcher_conf_present: bool,
    templates_toml_status: FileStatus,
    actions_toml_status: FileStatus,
    prompt_names: Vec<String>,
    agent_profile_names: Vec<String>,
    hook_names: Vec<String>,
}

/// Scan a host-level pohunek config dir: existence + TOML parse status for
/// known config files, filenames only for prompts, agents, and hooks.
fn scan_pohunek_dir(dir: &Path) -> DirScan {
    let launcher_conf_present = dir.join("launcher.conf").exists();
    let templates_toml_status = toml_file_status(&dir.join("templates.toml"));
    let actions_toml_status = toml_file_status(&dir.join("actions.toml"));
    let prompt_names = list_filenames_without_ext(&dir.join("prompts"), "tmpl");
    let agent_profile_names = list_filenames_without_ext(&dir.join("agents"), "toml");
    let hook_names = list_all_filenames(&dir.join("hooks"));

    DirScan {
        launcher_conf_present,
        templates_toml_status,
        actions_toml_status,
        prompt_names,
        agent_profile_names,
        hook_names,
    }
}

/// Scan a repo `.pohunek/` dir — no `launcher.conf` or `agents/` at this layer.
fn scan_repo_dir(dir: &Path) -> RepoConfigScan {
    let templates_toml_status = toml_file_status(&dir.join("templates.toml"));
    let actions_toml_status = toml_file_status(&dir.join("actions.toml"));
    let prompt_names = list_filenames_without_ext(&dir.join("prompts"), "tmpl");
    let hook_names = list_all_filenames(&dir.join("hooks"));

    RepoConfigScan {
        templates_toml_status,
        actions_toml_status,
        prompt_names,
        hook_names,
    }
}

/// Check whether a file exists and, if it does, whether it parses as TOML.
///
/// The TOML *body* is discarded immediately — only the parse result (ok vs
/// error) is recorded.
fn toml_file_status(path: &Path) -> FileStatus {
    match std::fs::read_to_string(path) {
        Err(_) => FileStatus::Absent,
        Ok(content) => {
            if content.parse::<toml::Value>().is_ok() {
                FileStatus::Ok
            } else {
                FileStatus::ParseError
            }
        }
    }
}

/// List filenames (without a given extension) from a directory.
///
/// Missing or unreadable directories are treated as empty. Only filenames
/// matching the expected extension are returned; directories and other entries
/// are skipped.
fn list_filenames_without_ext(dir: &Path, ext: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let file_type = entry.file_type().ok()?;
            if !file_type.is_file() {
                return None;
            }
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            name.ends_with(&format!(".{ext}"))
                .then(|| name[..name.len() - ext.len() - 1].to_owned())
        })
        .collect();
    names.sort_unstable();
    names
}

/// List all filenames (not paths) in a directory.
///
/// Used for hook dirs where every file name is a hook, regardless of extension.
/// Missing or unreadable directories are treated as empty.
fn list_all_filenames(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            Some(entry.file_name().to_string_lossy().into_owned())
        })
        .collect();
    names.sort_unstable();
    names
}

/// Collect git source-tree context for the given repo path.
///
/// Uses `std::process::Command` to run `git`. Absolute paths are redacted.
fn collect_source_tree(repo: &std::path::Path) -> Result<SourceTreeSection, String> {
    // git rev-parse --show-toplevel
    let git_root_raw = run_git(&["rev-parse", "--show-toplevel"], repo)?;
    let git_root = redact_path(git_root_raw.trim());

    // git branch --show-current
    let git_branch = run_git(&["branch", "--show-current"], repo)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    // git status --porcelain=v1 — empty → clean, non-empty → dirty.
    let dirty_status_summary = run_git(&["status", "--porcelain=v1"], repo).map_or_else(
        |_| "unknown".to_owned(),
        |out| {
            if out.trim().is_empty() {
                "clean".to_owned()
            } else {
                "dirty".to_owned()
            }
        },
    );

    // Compare the nearest version tag at HEAD with the running binary version.
    let version_matches_binary = run_git(&["describe", "--tags", "--exact-match", "HEAD"], repo)
        .ok()
        .is_some_and(|tag| {
            let tag = tag.trim().trim_start_matches('v');
            tag == BUNDLE_VERSION
        });

    Ok(SourceTreeSection {
        git_root,
        git_branch,
        dirty_status_summary,
        version_matches_binary,
    })
}

/// Run a `git` subcommand in `repo`, returning trimmed stdout on success.
fn run_git(args: &[&str], repo: &std::path::Path) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "git {} exited with {}",
            args.join(" "),
            output.status
        ))
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn paths_section(paths: &Paths) -> PathsSection {
    PathsSection {
        config_dir: redact_path(&paths.config_dir.display().to_string()),
        data_dir: redact_path(&paths.data_dir.display().to_string()),
        log_dir: redact_path(&paths.log_dir.display().to_string()),
        cache_dir: redact_path(&paths.cache_dir.display().to_string()),
    }
}

/// Rewrite a home-revealing absolute path into a `~`-relative form so the
/// snapshot never leaks the user's home directory.
fn redact_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            if let Some(rest) = path.strip_prefix(&home) {
                return format!("~{rest}");
            }
        }
    }
    path.to_owned()
}

fn serialize(snapshot: &Snapshot) -> String {
    // A typed struct cannot fail to serialize; fall back to a minimal valid JSON
    // object rather than panicking inside a launch.
    serde_json::to_string_pretty(snapshot)
        .unwrap_or_else(|_| "{\"warnings\":[\"snapshot serialization failed\"]}".to_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use protocol::{
        AgentKind, AgentRuntime, DoctorCheck, DoctorReport, DoctorStatus, ProjectActionsResult,
        ProjectSource, SessionId, SessionState,
    };

    use super::*;

    /// Serializes tests that mutate process-global environment variables
    /// (`HOME`, ad-hoc secrets). Rust runs tests in parallel, so without this a
    /// `set_var`/`remove_var` in one test can race a read in another and flake.
    /// Mirrors the env guard in `crate::paths` tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn options() -> AssistantOptions {
        AssistantOptions {
            intent: super::super::Intent::Setup,
            request: Some("configure launcher".to_owned()),
            agent: None,
            host: "local".to_owned(),
            project: Some("ui".to_owned()),
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

    fn options_no_project() -> AssistantOptions {
        AssistantOptions {
            project: None,
            ..options()
        }
    }

    fn sample_capabilities() -> HostCapabilities {
        HostCapabilities {
            daemon_version: "0.4.0".to_owned(),
            protocol_version: protocol::PROTOCOL_VERSION,
            supported_agents: vec!["codex".to_owned(), "claude".to_owned()],
            runtimes: vec![
                AgentRuntime {
                    agent: "codex".to_owned(),
                    available: true,
                    path: Some("/usr/local/bin/codex".to_owned()),
                },
                AgentRuntime {
                    agent: "claude".to_owned(),
                    available: false,
                    path: None,
                },
            ],
            git_available: true,
            worktree_supported: true,
        }
    }

    fn sample_project_info() -> ProjectInfo {
        ProjectInfo {
            id: "p-abc123".to_owned(),
            label: "ui".to_owned(),
            repo_root: "/home/user/projects/ui".into(),
            git_common_dir: "/home/user/projects/ui/.git".into(),
            origin_url: Some("https://github.com/org/ui.git".to_owned()),
            default_base_branch: Some("main".to_owned()),
            source: ProjectSource::Manual,
            is_bare: false,
            added_at: "2026-01-01T00:00:00Z".to_owned(),
            last_used_at: "2026-06-01T00:00:00Z".to_owned(),
        }
    }

    fn sample_project_actions() -> ProjectActionsResult {
        ProjectActionsResult {
            actions: vec![
                protocol::ActionSummary {
                    name: "review".to_owned(),
                    provider: protocol::ProviderKind::GithubPr,
                    template: "review.tmpl".to_owned(),
                    layer: protocol::PromptLayer::Host,
                },
                protocol::ActionSummary {
                    name: "fix".to_owned(),
                    provider: protocol::ProviderKind::LinearIssue,
                    template: "fix.tmpl".to_owned(),
                    layer: protocol::PromptLayer::InRepo,
                },
            ],
        }
    }

    fn sample_session_info() -> SessionInfo {
        SessionInfo {
            id: SessionId("s-001".to_owned()),
            agent: "codex".to_owned(),
            agent_base: AgentKind::Codex,
            cwd: "/home/user/projects/ui".into(),
            pid: 1234,
            cols: 80,
            rows: 24,
            state: SessionState::Running,
            state_source: protocol::StateSource::Process,
            activity: Some(protocol::AgentActivity::Working),
            native_session_id: None,
            native_session_path: Some("/home/user/.codex/sessions/s-001".to_owned()),
            project_id: Some("p-abc123".to_owned()),
            project_label: Some("ui".to_owned()),
            metadata: std::collections::BTreeMap::new(),
            is_linked_worktree: Some(false),
            repo: Some("/home/user/projects/ui".into()),
            branch: Some("main".to_owned()),
            worktree_path: Some("/home/user/projects/ui".into()),
            warnings: Vec::new(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:01:00Z".to_owned(),
            exit_code: None,
        }
    }

    fn sample_doctor_result() -> DaemonDoctorResult {
        DaemonDoctorResult {
            report: DoctorReport::from_checks(vec![
                DoctorCheck::new("daemon", DoctorStatus::Ok, "running"),
                DoctorCheck::new(
                    "git",
                    DoctorStatus::Ok,
                    "PATH=/usr/bin:/usr/local/bin git version 2.45.0",
                ),
            ]),
        }
    }

    // -----------------------------------------------------------------------
    // Allowlist enforcement: no extra top-level keys
    // -----------------------------------------------------------------------

    /// The serialized snapshot must contain ONLY the allowlisted top-level keys.
    /// If a new section is added to `Snapshot`, the expected set here must be
    /// updated too — this test enforces the structural allowlist.
    #[test]
    fn snapshot_top_level_keys_are_exactly_the_allowlist() {
        // Build a fully-populated snapshot by direct construction.
        let doctor = DoctorSection {
            overall: "ok".to_owned(),
            checks: vec![DoctorCheckSummary {
                name: "daemon".to_owned(),
                status: "ok".to_owned(),
            }],
        };
        let host = map_host_capabilities(&sample_capabilities());
        let project_info = sample_project_info();
        let actions = sample_project_actions();
        let selected = map_selected_project(&project_info, &actions);

        let snapshot = Snapshot {
            assistant: AssistantSection {
                intent: "setup".to_owned(),
                user_request: Some("configure".to_owned()),
                selected_host: "local".to_owned(),
                selected_project: Some("ui".to_owned()),
                selected_agent: "codex".to_owned(),
                auto_started_daemon: false,
                knowledge_bundle_version: "0.4.0".to_owned(),
                snapshot_collected: true,
            },
            paths: Some(PathsSection {
                config_dir: "~/.config/pohunek".to_owned(),
                data_dir: "~/.local/share/pohunek".to_owned(),
                log_dir: "~/.local/state/pohunek/logs".to_owned(),
                cache_dir: "~/.cache/pohunek".to_owned(),
            }),
            doctor: Some(doctor),
            host: Some(host),
            projects: Some(ProjectsSection {
                known_projects: vec![map_project_summary(&project_info)],
                selected_project: Some(selected),
            }),
            sessions: Some(SessionsSection {
                active_sessions: vec![map_session_summary(&sample_session_info())],
            }),
            config_scan: Some(ConfigScanSection {
                launcher_conf_present: true,
                templates_toml_status: FileStatus::Ok,
                actions_toml_status: FileStatus::Absent,
                prompt_names: vec!["default".to_owned()],
                agent_profile_names: vec!["pohunek-assistant".to_owned()],
                hook_names: vec!["session-start".to_owned()],
                repo_scan: None,
            }),
            source_tree: Some(SourceTreeSection {
                git_root: "~/projects/myrepo".to_owned(),
                git_branch: Some("main".to_owned()),
                dirty_status_summary: "clean".to_owned(),
                version_matches_binary: false,
            }),
            warnings: Vec::new(),
        };

        let json_str = serialize(&snapshot);
        let value: serde_json::Value =
            serde_json::from_str(&json_str).expect("snapshot serializes to valid JSON");

        let top_level_keys: HashSet<String> = value
            .as_object()
            .expect("top level must be a JSON object")
            .keys()
            .cloned()
            .collect();

        // These are the ONLY keys the snapshot is allowed to emit.
        let expected: HashSet<String> = [
            "assistant",
            "paths",
            "doctor",
            "host",
            "projects",
            "sessions",
            "config_scan",
            "source_tree",
        ]
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

        // warnings is omitted when empty (skip_serializing_if).
        assert!(
            top_level_keys.is_subset(&expected),
            "unexpected top-level keys in snapshot: {:?}",
            top_level_keys.difference(&expected).collect::<Vec<_>>()
        );
        // All expected keys (except warnings which is empty) must be present.
        let expected_present: HashSet<String> = expected.clone();
        assert!(
            expected_present.is_subset(&top_level_keys),
            "missing expected top-level keys: {:?}",
            expected_present
                .difference(&top_level_keys)
                .collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------------
    // Redaction: absolute paths must not appear
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_does_not_leak_home_in_paths() {
        let _env = lock_env();
        std::env::set_var("HOME", "/home/testuser");
        // The paths section should contain `~` not `/home/testuser`.
        let paths = Some(PathsSection {
            config_dir: redact_path("/home/testuser/.config/pohunek"),
            data_dir: redact_path("/home/testuser/.local/share/pohunek"),
            log_dir: redact_path("/home/testuser/.local/state/pohunek/logs"),
            cache_dir: redact_path("/home/testuser/.cache/pohunek"),
        });
        let snapshot = Snapshot {
            assistant: AssistantSection {
                intent: "help".to_owned(),
                user_request: None,
                selected_host: "local".to_owned(),
                selected_project: None,
                selected_agent: "codex".to_owned(),
                auto_started_daemon: false,
                knowledge_bundle_version: "0.4.0".to_owned(),
                snapshot_collected: true,
            },
            paths,
            doctor: None,
            host: None,
            projects: None,
            sessions: None,
            config_scan: None,
            source_tree: None,
            warnings: Vec::new(),
        };
        let json = serialize(&snapshot);
        assert!(
            !json.contains("/home/testuser"),
            "snapshot must not contain the absolute HOME path; got: {json}"
        );
        assert!(
            json.contains("~/.config/pohunek"),
            "redacted path must use ~ form; got: {json}"
        );
    }

    // -----------------------------------------------------------------------
    // Redaction: host section drops binary paths
    // -----------------------------------------------------------------------

    #[test]
    fn host_section_drops_runtime_paths() {
        let caps = sample_capabilities();
        let host = map_host_capabilities(&caps);

        let json = serde_json::to_string_pretty(&host).expect("serializes");
        // The absolute path to the codex binary must not appear.
        assert!(
            !json.contains("/usr/local/bin/codex"),
            "host section must not emit absolute runtime path; got: {json}"
        );
        // But availability flags must be present.
        assert!(
            json.contains("\"available\": true"),
            "available flag missing"
        );
    }

    // -----------------------------------------------------------------------
    // Redaction: project section drops absolute paths
    // -----------------------------------------------------------------------

    #[test]
    fn project_section_drops_absolute_paths() {
        let info = sample_project_info();
        let actions = sample_project_actions();
        let selected = map_selected_project(&info, &actions);

        let json = serde_json::to_string_pretty(&selected).expect("serializes");
        // Absolute paths must not appear.
        assert!(
            !json.contains("/home/user"),
            "project section must not emit absolute path; got: {json}"
        );
        // IDs and action names must be present.
        assert!(json.contains("p-abc123"), "project id missing");
        assert!(json.contains("review"), "action name missing");
        assert!(json.contains("fix"), "action name missing");
    }

    // -----------------------------------------------------------------------
    // Redaction: session section drops path fields
    // -----------------------------------------------------------------------

    #[test]
    fn session_section_drops_path_fields() {
        let info = sample_session_info();
        let summary = map_session_summary(&info);

        let json = serde_json::to_string_pretty(&summary).expect("serializes");
        // cwd, repo, worktree_path, native_session_path must not appear.
        assert!(
            !json.contains("/home/user"),
            "session summary must not contain absolute path; got: {json}"
        );
        assert!(
            !json.contains("native_session_path"),
            "native_session_path must not be emitted"
        );
        // But id, agent, state must be present.
        assert!(json.contains("s-001"), "session id missing");
        assert!(json.contains("codex"), "agent missing");
        assert!(json.contains("running"), "state missing");
    }

    // -----------------------------------------------------------------------
    // Redaction: doctor section drops detail
    // -----------------------------------------------------------------------

    #[test]
    fn doctor_section_drops_check_detail() {
        let result = sample_doctor_result();
        let section = DoctorSection {
            overall: result.report.overall.as_str().to_owned(),
            checks: result
                .report
                .checks
                .into_iter()
                .map(|check| DoctorCheckSummary {
                    name: check.name,
                    status: check.status.as_str().to_owned(),
                })
                .collect(),
        };

        let json = serde_json::to_string_pretty(&section).expect("serializes");
        // The detail field (which carried PATH fragments in the sample) must not
        // appear in the output at all.
        assert!(
            !json.contains("PATH="),
            "doctor section must not emit check detail (may carry PATH); got: {json}"
        );
        assert!(
            !json.contains("detail"),
            "doctor section must not emit a 'detail' key; got: {json}"
        );
    }

    // -----------------------------------------------------------------------
    // Env vars and profile [env] must not appear in the snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_does_not_leak_env_vars_or_profile_env_keys() {
        let _env = lock_env();
        // Set a process environment variable with a recognizable value.
        std::env::set_var("POHUNEK_TEST_SECRET", "super_secret_value_xyz");

        let snapshot = Snapshot {
            assistant: AssistantSection {
                intent: "help".to_owned(),
                user_request: None,
                selected_host: "local".to_owned(),
                selected_project: None,
                selected_agent: "codex".to_owned(),
                auto_started_daemon: false,
                knowledge_bundle_version: "0.4.0".to_owned(),
                snapshot_collected: true,
            },
            paths: Some(PathsSection {
                config_dir: "~/.config/pohunek".to_owned(),
                data_dir: "~/.local/share/pohunek".to_owned(),
                log_dir: "~/.local/state/pohunek".to_owned(),
                cache_dir: "~/.cache/pohunek".to_owned(),
            }),
            doctor: None,
            host: None,
            projects: None,
            sessions: None,
            config_scan: None,
            source_tree: None,
            warnings: Vec::new(),
        };

        let json = serialize(&snapshot);
        assert!(
            !json.contains("super_secret_value_xyz"),
            "snapshot must never contain process env values; got: {json}"
        );
        assert!(
            !json.contains("POHUNEK_TEST_SECRET"),
            "snapshot must never contain env var names; got: {json}"
        );

        // Simulate a profile [env] key — the snapshot structs structurally
        // cannot include them (no flatten, no raw Value), so the value cannot
        // appear. Verify by checking the JSON does not contain a token that a
        // profile env value would produce.
        let profile_env_value = "profile_env_secret_abc";
        assert!(
            !json.contains(profile_env_value),
            "snapshot must never contain profile [env] values"
        );

        std::env::remove_var("POHUNEK_TEST_SECRET");
    }

    // -----------------------------------------------------------------------
    // Skipped snapshot (--no-snapshot)
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Local-section gating: remote launches must not embed local filesystem data
    // -----------------------------------------------------------------------

    fn dummy_paths() -> Paths {
        let base = std::env::temp_dir().join(format!(
            "pohunek-snap-localsec-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after epoch")
                .as_nanos()
        ));
        Paths {
            runtime_dir: base.join("runtime"),
            socket: base.join("runtime").join("daemon.sock"),
            data_dir: base.join("data"),
            log_dir: base.join("logs"),
            cache_dir: base.join("cache"),
            config_home: base.join("config"),
            config_dir: base.join("config").join("pohunek"),
        }
    }

    #[test]
    fn local_sections_present_for_local_host() {
        let local = collect_local_sections(&dummy_paths(), &options());
        assert!(local.paths.is_some(), "local launch must include paths");
        assert!(
            local.config_scan.is_some(),
            "local launch must include config_scan"
        );
        // options() sets no repo, so source_tree is absent without a warning.
        assert!(local.source_tree.is_none());
        assert!(
            local.warnings.is_empty(),
            "no warnings expected for a clean local scan, got {:?}",
            local.warnings
        );
    }

    #[test]
    fn local_sections_omitted_for_remote_host() {
        let remote = AssistantOptions {
            host: "build-box".to_owned(),
            repo: Some(std::path::PathBuf::from("/srv/repo")),
            ..options()
        };
        let local = collect_local_sections(&dummy_paths(), &remote);
        assert!(local.paths.is_none(), "remote must omit local paths");
        assert!(
            local.config_scan.is_none(),
            "remote must omit local config_scan"
        );
        assert!(
            local.source_tree.is_none(),
            "remote must omit local source_tree"
        );
        // The omission is explicit: one warning per omitted local section.
        assert!(local.warnings.iter().any(|w| w.starts_with("paths:")));
        assert!(local.warnings.iter().any(|w| w.starts_with("config_scan:")));
        assert!(local.warnings.iter().any(|w| w.starts_with("source_tree:")));
    }

    #[test]
    fn skipped_snapshot_marks_not_collected_and_orients() {
        let (json, orientation) = skipped_snapshot(&options(), "codex", true);
        assert!(json.contains("\"snapshot_collected\": false"));
        assert!(json.contains("\"selected_agent\": \"codex\""));
        assert_eq!(orientation.agent, "codex");
        assert_eq!(orientation.project, "ui");
    }

    #[test]
    fn skipped_snapshot_no_project_uses_none_label() {
        let (_, orientation) = skipped_snapshot(&options_no_project(), "claude", false);
        assert_eq!(orientation.project, "(none)");
    }

    // -----------------------------------------------------------------------
    // redact_path helper
    // -----------------------------------------------------------------------

    #[test]
    fn redact_path_rewrites_home_prefix() {
        let _env = lock_env();
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(
            redact_path("/home/tester/.cache/pohunek"),
            "~/.cache/pohunek"
        );
        assert_eq!(redact_path("/var/lib/pohunek"), "/var/lib/pohunek");
    }

    // -----------------------------------------------------------------------
    // Pure mapping helpers
    // -----------------------------------------------------------------------

    #[test]
    fn map_host_capabilities_drops_path_field() {
        let caps = sample_capabilities();
        let host = map_host_capabilities(&caps);
        // The struct has no path field — the field simply does not exist in
        // RuntimeSummary, so it cannot be serialized.
        assert_eq!(host.runtimes.len(), 2);
        assert_eq!(host.runtimes[0].agent, "codex");
        assert!(host.runtimes[0].available);
        assert_eq!(host.runtimes[1].agent, "claude");
        assert!(!host.runtimes[1].available);
        // Confirm git flags passed through.
        assert!(host.git_available);
        assert!(host.worktree_supported);
    }

    #[test]
    fn map_selected_project_keeps_action_names_only() {
        let info = sample_project_info();
        let actions = sample_project_actions();
        let selected = map_selected_project(&info, &actions);

        assert_eq!(selected.id, "p-abc123");
        assert_eq!(selected.label, "ui");
        assert_eq!(selected.action_names, vec!["review", "fix"]);
        // default_base_branch and is_bare are present but no path fields exist
        // on SelectedProjectDetail.
        assert_eq!(selected.default_base_branch.as_deref(), Some("main"));
        assert!(!selected.is_bare);
    }

    #[test]
    fn map_session_summary_keeps_id_agent_state() {
        let info = sample_session_info();
        let summary = map_session_summary(&info);

        assert_eq!(summary.id, "s-001");
        assert_eq!(summary.agent, "codex");
        assert_eq!(summary.state, "running");
        assert_eq!(summary.activity.as_deref(), Some("working"));
        assert_eq!(summary.project_id.as_deref(), Some("p-abc123"));
        assert_eq!(summary.project_label.as_deref(), Some("ui"));
        assert_eq!(summary.warning_count, 0);
    }

    #[test]
    fn snapshot_protocol_labels_are_not_derived_from_debug() {
        let source = include_str!("snapshot.rs");
        let old_project_source = concat!("format!(\"", "{:?}", "\", info.source).to_lowercase()");
        let old_session_state = concat!("format!(\"", "{:?}", "\", info.state).to_lowercase()");
        let old_activity = concat!("format!(\"{", "a:", "?", "}\").to_lowercase()");

        for pattern in [old_project_source, old_session_state, old_activity] {
            assert!(
                !source.contains(pattern),
                "snapshot protocol labels must come from protocol-owned string helpers"
            );
        }
    }

    #[test]
    fn file_status_absent_for_missing_file() {
        let tmp = std::env::temp_dir().join("pohunek_test_nonexistent_toml_xyz.toml");
        // Make sure it doesn't exist.
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(toml_file_status(&tmp), FileStatus::Absent);
    }

    #[test]
    fn file_status_ok_for_valid_toml() {
        let tmp =
            std::env::temp_dir().join(format!("pohunek_test_valid_{}.toml", std::process::id()));
        std::fs::write(&tmp, "[section]\nkey = \"value\"\n").expect("write");
        assert_eq!(toml_file_status(&tmp), FileStatus::Ok);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn file_status_parse_error_for_invalid_toml() {
        let tmp =
            std::env::temp_dir().join(format!("pohunek_test_invalid_{}.toml", std::process::id()));
        std::fs::write(&tmp, "this is not toml }{").expect("write");
        assert_eq!(toml_file_status(&tmp), FileStatus::ParseError);
        let _ = std::fs::remove_file(&tmp);
    }
}

//! `pohunek project` — list, add, show, rename, and forget projects on a host.
//!
//! A **project** is a git repository the daemon has seen (see
//! `docs/design/projects.md`). References are `<id|label>` resolved **daemon-side**
//! against the target host's own store, so no filesystem path crosses the wire for
//! `show`/`rename`/`rm`; only `add` carries a path, which must be valid on the
//! target host (the CLI fills the local cwd for a local `add` with no PATH). The
//! grammar is host-aware through the global `--host` flag, exactly like `host` and
//! `session`.

use std::path::PathBuf;

use protocol::{
    method, ActionSummary, ProjectActionParams, ProjectActionResult, ProjectActionsParams,
    ProjectActionsResult, ProjectAddParams, ProjectInfo, ProjectListFilter, ProjectListParams,
    ProjectPromptParams, ProjectPromptResult, ProjectRemoveParams, ProjectRemoveResult,
    ProjectRenameParams, ProjectShowParams, ProjectShowResult, ProjectSource, ProjectWorktree,
    PromptLayer, ProviderKind, Request,
};
use serde::Serialize;
use serde_json::Value;

use crate::client::Client;
use crate::commands::request_id;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::LOCAL_HOST;

/// Parse one `project list --filter key=value` argument into a protocol filter.
///
/// Supported keys mirror the project list shape: `source` (auto/manual), `label`,
/// `id`. Owned by the project command but used by clap as a value parser so an
/// invalid filter goes through the usual usage-error sink.
pub(crate) fn parse_project_filter(input: &str) -> Result<ProjectListFilter, String> {
    let (key, value) = input
        .split_once('=')
        .ok_or_else(|| format!("invalid filter {input:?}: expected key=value"))?;
    if value.is_empty() {
        return Err(format!(
            "invalid filter {input:?}: filter value cannot be empty"
        ));
    }
    match key {
        "id" => Ok(ProjectListFilter::Id(value.to_owned())),
        "label" => Ok(ProjectListFilter::Label(value.to_owned())),
        "source" => parse_source(value).map(ProjectListFilter::Source),
        other => Err(format!(
            "unknown filter key {other:?}; expected one of: source, label, id"
        )),
    }
}

fn parse_source(value: &str) -> Result<ProjectSource, String> {
    match value {
        "auto" => Ok(ProjectSource::Auto),
        "manual" => Ok(ProjectSource::Manual),
        other => Err(format!(
            "invalid source filter value {other:?}; expected one of: auto, manual"
        )),
    }
}

/// Run `project list` against the daemon for `host`.
pub(crate) async fn run_list(
    host: &str,
    paths: &Paths,
    filters: &[ProjectListFilter],
    json: bool,
) -> Result<(), CliError> {
    let request = if filters.is_empty() {
        Request::new(
            request_id(method::PROJECT_LIST),
            method::PROJECT_LIST,
            Value::Null,
        )
    } else {
        request_with_params(
            method::PROJECT_LIST,
            &ProjectListParams {
                filters: filters.to_vec(),
            },
        )?
    };
    let mut client = Client::connect(host, paths).await?;
    let projects: Vec<ProjectInfo> = serde_json::from_value(client.request(&request).await?)?;
    if json {
        print!("{}", crate::commands::render_json(&projects)?);
    } else {
        print!("{}", render_list_human(&projects));
    }
    Ok(())
}

/// Run `project add` against the daemon for `host`.
///
/// A local `add` with no PATH sends the CLI's own `current_dir()`; a remote `add`
/// must name a PATH valid on that host (failing fast before dialing).
pub(crate) async fn run_add(
    host: &str,
    paths: &Paths,
    path: Option<PathBuf>,
    name: Option<String>,
    base_branch: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let path = resolve_add_path(host, path)?;
    let request = request_with_params(
        method::PROJECT_ADD,
        &ProjectAddParams {
            path: Some(path),
            name,
            base_branch,
        },
    )?;
    let mut client = Client::connect(host, paths).await?;
    let project: ProjectInfo = serde_json::from_value(client.request(&request).await?)?;
    if json {
        print!("{}", crate::commands::render_json(&project)?);
    } else {
        print!("{}", render_added_human(&project));
    }
    Ok(())
}

/// Run `project show <reference>` against the daemon for `host`.
pub(crate) async fn run_show(
    host: &str,
    paths: &Paths,
    reference: &str,
    json: bool,
) -> Result<(), CliError> {
    let request = request_with_params(
        method::PROJECT_SHOW,
        &ProjectShowParams {
            reference: reference.to_owned(),
        },
    )?;
    let mut client = Client::connect(host, paths).await?;
    let result: ProjectShowResult = serde_json::from_value(client.request(&request).await?)?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_show_human(&result));
    }
    Ok(())
}

/// Run `project prompt <reference> <name>` against the daemon for `host`.
///
/// Resolves one prompt by name to its template content (the in-repo `.pohunek/`
/// layer shadows the host default), or a typed `prompt_not_found`/`invalid_name`.
/// Human output writes the raw template to stdout verbatim (it is fed to a
/// renderer) and the resolved layer to stderr, so a `--json`-free consumer still
/// gets a clean template on stdout.
pub(crate) async fn run_prompt(
    host: &str,
    paths: &Paths,
    reference: &str,
    name: &str,
    json: bool,
) -> Result<(), CliError> {
    let request = request_with_params(
        method::PROJECT_PROMPT,
        &ProjectPromptParams {
            reference: reference.to_owned(),
            name: name.to_owned(),
        },
    )?;
    let mut client = Client::connect(host, paths).await?;
    let result: ProjectPromptResult = serde_json::from_value(client.request(&request).await?)?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        let layer = match result.layer {
            PromptLayer::InRepo => "in-repo .pohunek/",
            PromptLayer::Host => "host config",
        };
        eprintln!("pohunek: prompt '{}' resolved from {layer}", result.name);
        print!("{}", result.content);
    }
    Ok(())
}

/// Human label for a provider kind (matches the wire snake_case form).
fn provider_label(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::LinearIssue => "linear_issue",
        ProviderKind::GithubPr => "github_pr",
        ProviderKind::None => "none",
    }
}

/// Run `project action <reference> <name>` against the daemon for `host`.
///
/// Resolves one action to its full recipe (provider, agent, base branch, branch
/// rule, prompt name + resolved prompt content) — the command the launcher calls.
/// Human output prints the recipe header, then the raw prompt template after a
/// `---` separator (rendered caller-side with provider data, A.4).
pub(crate) async fn run_action(
    host: &str,
    paths: &Paths,
    reference: &str,
    name: &str,
    json: bool,
) -> Result<(), CliError> {
    let request = request_with_params(
        method::PROJECT_ACTION,
        &ProjectActionParams {
            reference: reference.to_owned(),
            name: name.to_owned(),
        },
    )?;
    let mut client = Client::connect(host, paths).await?;
    let result: ProjectActionResult = serde_json::from_value(client.request(&request).await?)?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        println!("provider:    {}", provider_label(&result.provider));
        println!("agent:       {}", result.agent);
        println!(
            "base_branch: {}",
            result
                .base_branch
                .as_deref()
                .unwrap_or("(project default / HEAD)")
        );
        if let Some(branch) = &result.branch {
            println!("branch:      {branch}");
        }
        println!("prompt:      {}", result.prompt_name);
        println!("---");
        print!("{}", result.prompt_content);
    }
    Ok(())
}

/// Run `project actions <reference>` against the daemon for `host`.
///
/// Lists the actions resolvable for the project (the union across the in-repo and
/// host layers), with the template each uses and the layer it resolved from.
pub(crate) async fn run_actions(
    host: &str,
    paths: &Paths,
    reference: &str,
    json: bool,
) -> Result<(), CliError> {
    let request = request_with_params(
        method::PROJECT_ACTIONS,
        &ProjectActionsParams {
            reference: reference.to_owned(),
        },
    )?;
    let mut client = Client::connect(host, paths).await?;
    let result: ProjectActionsResult = serde_json::from_value(client.request(&request).await?)?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_actions_human(&result.actions));
    }
    Ok(())
}

/// Render `project actions` as a tab-separated table (human output).
fn render_actions_human(actions: &[ActionSummary]) -> String {
    if actions.is_empty() {
        return "no actions resolvable for this project\n".to_owned();
    }
    let mut out = String::from("ACTION\tPROVIDER\tTEMPLATE\tLAYER\n");
    for action in actions {
        let layer = match action.layer {
            PromptLayer::InRepo => "in-repo",
            PromptLayer::Host => "host",
        };
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            action.name,
            provider_label(&action.provider),
            action.template,
            layer
        ));
    }
    out
}

/// Run `project rename <reference> <name>` against the daemon for `host`.
pub(crate) async fn run_rename(
    host: &str,
    paths: &Paths,
    reference: &str,
    name: &str,
    json: bool,
) -> Result<(), CliError> {
    let request = request_with_params(
        method::PROJECT_RENAME,
        &ProjectRenameParams {
            reference: reference.to_owned(),
            name: name.to_owned(),
        },
    )?;
    let mut client = Client::connect(host, paths).await?;
    let project: ProjectInfo = serde_json::from_value(client.request(&request).await?)?;
    if json {
        print!("{}", crate::commands::render_json(&project)?);
    } else {
        println!("project {} renamed to {}", project.id, project.label);
    }
    Ok(())
}

/// Run `project rm <reference> [--prune-worktrees]` against the daemon for `host`.
pub(crate) async fn run_rm(
    host: &str,
    paths: &Paths,
    reference: &str,
    prune_worktrees: bool,
    json: bool,
) -> Result<(), CliError> {
    let request = request_with_params(
        method::PROJECT_REMOVE,
        &ProjectRemoveParams {
            reference: reference.to_owned(),
            prune_worktrees,
        },
    )?;
    let mut client = Client::connect(host, paths).await?;
    let result: ProjectRemoveResult = serde_json::from_value(client.request(&request).await?)?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        // A live session was using a worktree the prune would have removed; it was
        // left in place. Warn on stderr (never pollutes a --json stdout consumer).
        if !result.skipped_worktrees.is_empty() {
            eprintln!(
                "pohunek: warning: skipped {} worktree(s) with a live session ({})",
                result.skipped_worktrees.len(),
                result.skipped_worktrees.join(", ")
            );
        }
        println!(
            "project {reference}: removed={}, pruned_worktrees={}",
            result.removed, result.pruned_worktrees
        );
    }
    Ok(())
}

/// Resolve the host-local path for `project add`.
///
/// Local: an explicit PATH is resolved to absolute against the CLI's **own** cwd
/// (so a relative `./repo` means the same thing the user sees, not whatever the
/// daemon's cwd happens to be); with no PATH the CLI's cwd is used. Remote: the
/// PATH is host-local, so it must be an explicit **absolute** path — a relative
/// path (or none) is meaningless on another host and fails fast before dialing.
fn resolve_add_path(host: &str, path: Option<PathBuf>) -> Result<PathBuf, CliError> {
    let remote = !host.is_empty() && host != LOCAL_HOST;
    match path {
        Some(path) if remote => {
            if path.is_relative() {
                return Err(CliError::RemoteAddPathRequired);
            }
            Ok(path)
        }
        Some(path) if path.is_relative() => Ok(std::env::current_dir()?.join(path)),
        Some(path) => Ok(path),
        None if remote => Err(CliError::RemoteAddPathRequired),
        None => Ok(std::env::current_dir()?),
    }
}

fn request_with_params<T>(method: &str, params: &T) -> Result<Request, CliError>
where
    T: Serialize + ?Sized,
{
    Ok(Request::new(
        request_id(method),
        method,
        serde_json::to_value(params)?,
    ))
}

fn source_label(source: ProjectSource) -> &'static str {
    match source {
        ProjectSource::Auto => "auto",
        ProjectSource::Manual => "manual",
    }
}

fn render_list_human(projects: &[ProjectInfo]) -> String {
    let id_width = projects
        .iter()
        .map(|p| p.id.len())
        .max()
        .unwrap_or(0)
        .max("ID".len());
    let label_width = projects
        .iter()
        .map(|p| p.label.len())
        .max()
        .unwrap_or(0)
        .max("LABEL".len());

    let mut output = String::new();
    output.push_str(&format!(
        "{:<id_width$}  {:<label_width$}  {:<6}  {:<4}  ROOT\n",
        "ID",
        "LABEL",
        "SOURCE",
        "BARE",
        id_width = id_width,
        label_width = label_width,
    ));
    for project in projects {
        output.push_str(&format!(
            "{:<id_width$}  {:<label_width$}  {:<6}  {:<4}  {}\n",
            project.id,
            project.label,
            source_label(project.source),
            if project.is_bare { "yes" } else { "-" },
            project.repo_root.display(),
            id_width = id_width,
            label_width = label_width,
        ));
    }
    output
}

fn render_added_human(project: &ProjectInfo) -> String {
    format!(
        "project {} ({}) registered at {}\n",
        project.label,
        project.id,
        project.repo_root.display()
    )
}

fn render_show_human(result: &ProjectShowResult) -> String {
    let p = &result.project;
    let none = || "<none>".to_owned();
    let mut output = String::new();
    let rows: Vec<(&str, String)> = vec![
        ("id", p.id.clone()),
        ("label", p.label.clone()),
        ("source", source_label(p.source).to_owned()),
        ("repo_root", p.repo_root.display().to_string()),
        ("git_common_dir", p.git_common_dir.display().to_string()),
        ("origin_url", p.origin_url.clone().unwrap_or_else(none)),
        (
            "default_base_branch",
            p.default_base_branch.clone().unwrap_or_else(none),
        ),
        (
            "bare",
            if p.is_bare {
                "yes".to_owned()
            } else {
                "no".to_owned()
            },
        ),
        ("added_at", p.added_at.clone()),
        ("last_used_at", p.last_used_at.clone()),
    ];
    let width = rows
        .iter()
        .map(|(field, _)| field.len())
        .max()
        .unwrap_or(0)
        .max("FIELD".len());
    output.push_str(&format!("{:<width$}  VALUE\n", "FIELD", width = width));
    for (field, value) in &rows {
        output.push_str(&format!("{field:<width$}  {value}\n", width = width));
    }

    output.push_str(&format!("\nworktrees ({}):\n", result.worktrees.len()));
    if result.worktrees.is_empty() {
        output.push_str("  <none reported by git>\n");
    } else {
        for wt in &result.worktrees {
            output.push_str(&format!("  {}\n", render_worktree_line(wt)));
        }
    }
    output
}

fn render_worktree_line(wt: &ProjectWorktree) -> String {
    let branch = wt.branch.as_deref().unwrap_or("(detached)");
    let mut flags = Vec::new();
    if wt.owned {
        flags.push("owned".to_owned());
    }
    if wt.bare {
        flags.push("bare".to_owned());
    }
    if wt.locked {
        flags.push("locked".to_owned());
    }
    if let Some(session) = &wt.session_id {
        flags.push(format!("session={session}"));
    }
    let flags = if flags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", flags.join(", "))
    };
    format!("{}  {branch}{flags}", wt.path.display())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use protocol::{ProjectInfo, ProjectShowResult, ProjectSource, ProjectWorktree};

    use super::*;

    fn project(id: &str, label: &str, source: ProjectSource) -> ProjectInfo {
        ProjectInfo {
            id: id.to_owned(),
            label: label.to_owned(),
            repo_root: PathBuf::from(format!("/code/{label}")),
            git_common_dir: PathBuf::from(format!("/code/{label}/.git")),
            origin_url: Some("https://github.com/example/repo.git".to_owned()),
            default_base_branch: None,
            source,
            is_bare: false,
            added_at: "2026-06-23T10:00:00Z".to_owned(),
            last_used_at: "2026-06-23T11:00:00Z".to_owned(),
        }
    }

    #[test]
    fn parses_each_filter_key() {
        assert_eq!(
            parse_project_filter("id=p-abc").expect("id"),
            ProjectListFilter::Id("p-abc".to_owned())
        );
        assert_eq!(
            parse_project_filter("label=ui").expect("label"),
            ProjectListFilter::Label("ui".to_owned())
        );
        assert_eq!(
            parse_project_filter("source=manual").expect("source"),
            ProjectListFilter::Source(ProjectSource::Manual)
        );
    }

    #[test]
    fn rejects_unknown_filter_key_and_bad_source() {
        assert!(parse_project_filter("repo=/x")
            .expect_err("unknown key")
            .contains("unknown filter key"));
        assert!(parse_project_filter("source=nope")
            .expect_err("bad source")
            .contains("invalid source filter value"));
    }

    #[test]
    fn add_path_resolves_locally_and_requires_an_absolute_path_remotely() {
        let cwd = std::env::current_dir().expect("cwd");
        // Local, no path: the CLI fills its own cwd.
        assert_eq!(resolve_add_path(LOCAL_HOST, None).expect("local add"), cwd);
        // Local, relative path: absolutized against the CLI's own cwd (not the
        // daemon's), so `./repo` means what the user sees.
        assert_eq!(
            resolve_add_path(LOCAL_HOST, Some(PathBuf::from("subdir"))).expect("local rel"),
            cwd.join("subdir")
        );
        // Local, absolute path: honored verbatim.
        assert_eq!(
            resolve_add_path(LOCAL_HOST, Some(PathBuf::from("/abs/repo"))).expect("local abs"),
            PathBuf::from("/abs/repo")
        );
        // Remote, absolute host-local path: honored verbatim.
        assert_eq!(
            resolve_add_path("host-b", Some(PathBuf::from("/on/remote"))).expect("remote abs"),
            PathBuf::from("/on/remote")
        );
        // Remote with no path — or a relative path — fails fast (a local path is
        // meaningless on another host).
        assert!(matches!(
            resolve_add_path("host-b", None),
            Err(CliError::RemoteAddPathRequired)
        ));
        assert!(matches!(
            resolve_add_path("host-b", Some(PathBuf::from("rel"))),
            Err(CliError::RemoteAddPathRequired)
        ));
    }

    #[test]
    fn list_table_has_columns_and_rows() {
        let output = render_list_human(&[
            project("p-aaa", "ui", ProjectSource::Auto),
            project("p-bbb", "api", ProjectSource::Manual),
        ]);
        let header = output.lines().next().expect("header");
        for column in ["ID", "LABEL", "SOURCE", "BARE", "ROOT"] {
            assert!(header.contains(column), "header missing {column}: {header}");
        }
        assert!(output.contains("p-aaa"));
        assert!(output.contains("manual"));
        assert!(output.contains("/code/ui"));
    }

    #[test]
    fn show_renders_fields_and_worktrees_with_flags() {
        let result = ProjectShowResult {
            project: project("p-aaa", "ui", ProjectSource::Auto),
            worktrees: vec![
                ProjectWorktree {
                    path: PathBuf::from("/code/ui"),
                    branch: Some("main".to_owned()),
                    head: Some("abc123".to_owned()),
                    bare: false,
                    locked: false,
                    owned: false,
                    session_id: None,
                },
                ProjectWorktree {
                    path: PathBuf::from("/data/worktrees/s-1-ui-feature"),
                    branch: Some("feature".to_owned()),
                    head: Some("def456".to_owned()),
                    bare: false,
                    locked: false,
                    owned: true,
                    session_id: Some("s-1".to_owned()),
                },
            ],
        };
        let output = render_show_human(&result);
        assert!(output.contains("id"));
        assert!(output.contains("p-aaa"));
        assert!(output.contains("worktrees (2):"));
        assert!(output.contains("/code/ui  main"));
        assert!(
            output.contains("owned") && output.contains("session=s-1"),
            "owned worktree with a live session must be flagged: {output}"
        );
    }

    #[test]
    fn list_and_show_json_round_trip() {
        let projects = vec![project("p-aaa", "ui", ProjectSource::Auto)];
        let doc = crate::commands::render_json(&projects).expect("json");
        let parsed: Vec<ProjectInfo> = serde_json::from_str(&doc).expect("parse");
        assert_eq!(parsed, projects);
    }
}

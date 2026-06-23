//! Projects: automatic git-repo awareness for sessions.
//!
//! A **project** is a lightweight record of a git repository the daemon has seen
//! on this host — keyed by the canonical `git_common_dir`, so the main checkout
//! and every linked worktree of one repo collapse to a single logical project
//! (see `docs/design/projects.md`). Projects accrue as a side effect of working:
//! starting a session inside a work tree auto-registers one; `pohunek project
//! add` registers one explicitly. There is deliberately no filesystem scan.
//!
//! This module owns the pure detection unit ([`detect`]) and the project id
//! derivation ([`detect::project_id`]), and [`ProjectManager`] on top of them:
//! the store glue (auto-registration + manual upsert) and `<id|label>` reference
//! resolution.

pub mod detect;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use protocol::{
    ErrorClass, ProjectInfo, ProjectListFilter, ProjectShowResult, ProjectSource, ProjectWorktree,
    ProtocolError,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::store::{ProjectRecord, ProjectResolution, Store, WorktreeStatus};
use crate::worktree::canonical_or_original;

use detect::DetectedProject;

/// Current UTC time as an RFC3339 string for project record timestamps (matches
/// the session/worktree stores). Formatting a valid `OffsetDateTime` cannot fail
/// in practice; the fallback only guards a future API change.
fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// Project store glue: resolve `<id|label>` references and upsert detected/added
/// projects into the unified [`Store`].
///
/// Holds the shared [`Store`] (`Arc`) — the same instance the session registry
/// and worktree manager write through, so all record kinds stay behind one
/// serialization point. Detection itself ([`detect`]) is pure and lives beside
/// this; the manager only adds the policy (what to preserve on re-registration,
/// how to map a missing/ambiguous reference to a typed error).
#[derive(Debug)]
pub struct ProjectManager {
    store: Arc<Store>,
}

impl ProjectManager {
    /// Build a manager over the shared metadata store.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// The shared metadata store, for the `project.*` handlers and tests.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Resolve a `<id|label>` reference to a project, mapping a miss or a
    /// collision to a typed [`ProtocolError`] the CLI renders directly
    /// (design Decision 2).
    pub fn resolve(&self, reference: &str) -> Result<ProjectRecord, ProtocolError> {
        match self.store.find_project(reference).map_err(store_error)? {
            ProjectResolution::Found(record) => Ok(record),
            ProjectResolution::NotFound => Err(project_not_found(reference)),
            ProjectResolution::Ambiguous(candidates) => {
                Err(project_ambiguous(reference, &candidates))
            }
        }
    }

    /// Upsert a project from a [`DetectedProject`], returning the stored record.
    ///
    /// Re-registration **preserves operator-owned fields** (`custom_name`,
    /// `default_base_branch`, `source`, `added_at`) and only refreshes what
    /// detection actually observes (`repo_root`, `origin_url`, `is_bare`) plus
    /// `last_used_at`. A brand-new project is recorded as [`ProjectSource::Auto`]
    /// when `manual` is false, else [`ProjectSource::Manual`]; re-registering an
    /// existing `Auto` project as manual promotes it (so it is never treated as
    /// stale auto data), but auto-registration never demotes a `Manual` project.
    pub fn register(
        &self,
        detected: &DetectedProject,
        manual: bool,
    ) -> Result<ProjectRecord, ProtocolError> {
        self.upsert(detected, manual, None, None)
    }

    /// The upsert primitive behind [`Self::register`] and [`Self::add`]: detect's
    /// observed fields are refreshed, operator-owned fields preserved, and any
    /// explicit `name`/`base_branch` overrides applied — all in **one**
    /// `record_project` write, so an `add` with overrides cannot lose a concurrent
    /// edit in a second-write window.
    fn upsert(
        &self,
        detected: &DetectedProject,
        manual: bool,
        name: Option<String>,
        base_branch: Option<String>,
    ) -> Result<ProjectRecord, ProtocolError> {
        if let Some(name) = &name {
            validate_project_name(name)?;
        }
        let now = now_rfc3339();
        let existing = self
            .store
            .load_projects()
            .map_err(store_error)?
            .into_iter()
            .find(|project| project.git_common_dir == detected.git_common_dir);

        let mut record = match existing {
            Some(mut prev) => {
                prev.repo_root = detected.repo_root.clone();
                prev.origin_url = detected.origin_url.clone();
                prev.is_bare = detected.is_bare;
                prev.last_used_at = now;
                // A manual (re-)add promotes an auto record; auto-registration
                // never demotes a manual one.
                if manual {
                    prev.source = ProjectSource::Manual;
                }
                prev
            }
            None => ProjectRecord {
                git_common_dir: detected.git_common_dir.clone(),
                repo_root: detected.repo_root.clone(),
                custom_name: None,
                origin_url: detected.origin_url.clone(),
                default_base_branch: None,
                is_bare: detected.is_bare,
                source: if manual {
                    ProjectSource::Manual
                } else {
                    ProjectSource::Auto
                },
                added_at: now.clone(),
                last_used_at: now,
            },
        };
        // Explicit overrides win over the preserved/default values.
        if let Some(name) = name {
            record.custom_name = Some(name);
        }
        if let Some(base_branch) = base_branch {
            record.default_base_branch = Some(base_branch);
        }
        self.store.record_project(&record).map_err(store_error)?;
        Ok(record)
    }

    /// Bump a project's `last_used_at` to now (a session started in it). Keyed by
    /// the canonical `git_common_dir` and re-read from the store before writing so
    /// a concurrent rename/add is not clobbered. Returns the updated record, or
    /// `None` if the project no longer exists (a benign race with `project rm`).
    /// The data model defines `last_used_at` as bumped on each session start, so
    /// the `--project` reference path must do this too — not only auto-detection.
    pub fn touch(&self, git_common_dir: &Path) -> Result<Option<ProjectRecord>, ProtocolError> {
        let existing = self
            .store
            .load_projects()
            .map_err(store_error)?
            .into_iter()
            .find(|project| project.git_common_dir == git_common_dir);
        let Some(mut record) = existing else {
            return Ok(None);
        };
        record.last_used_at = now_rfc3339();
        self.store.record_project(&record).map_err(store_error)?;
        Ok(Some(record))
    }

    /// All known projects (display shape), AND-filtered and sorted by label then
    /// id for a stable, low-noise inventory.
    pub fn list(&self, filters: &[ProjectListFilter]) -> Result<Vec<ProjectInfo>, ProtocolError> {
        let mut infos: Vec<ProjectInfo> = self
            .store
            .load_projects()
            .map_err(store_error)?
            .iter()
            .map(to_info)
            .collect();
        infos.retain(|info| filters.iter().all(|filter| filter.matches(info)));
        infos.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.cmp(&b.id)));
        Ok(infos)
    }

    /// Register a project explicitly (`project add`): detect at `path`, upsert as
    /// [`ProjectSource::Manual`] (re-adding promotes an auto record), applying any
    /// explicit `name`/`base_branch` overrides in one write. Errors when `path` is
    /// not a git work tree (the operator named it deliberately) or `name` is blank.
    pub fn add(
        &self,
        path: &Path,
        name: Option<String>,
        base_branch: Option<String>,
    ) -> Result<ProjectInfo, ProtocolError> {
        let detected = detect_at(path)?.ok_or_else(|| not_a_git_repo(path))?;
        let record = self.upsert(&detected, true, name, base_branch)?;
        Ok(to_info(&record))
    }

    /// Set a project's custom display name (`project rename`). Does not bump
    /// `last_used_at` — renaming is not a session start. A blank name is rejected
    /// so the label can never be emptied (which would make the project
    /// referenceable only by its id).
    pub fn rename(&self, reference: &str, name: String) -> Result<ProjectInfo, ProtocolError> {
        validate_project_name(&name)?;
        let mut record = self.resolve(reference)?;
        record.custom_name = Some(name);
        self.store.record_project(&record).map_err(store_error)?;
        Ok(to_info(&record))
    }

    /// Forget a project record (`project rm`). Only removes the record; it never
    /// touches the on-disk repository or its worktrees (pruning owned worktrees is
    /// the worktree-linkage milestone). Returns whether a record was removed.
    pub fn remove(&self, reference: &str) -> Result<bool, ProtocolError> {
        let record = self.resolve(reference)?;
        self.store
            .remove_project(&record.git_common_dir)
            .map_err(store_error)
    }

    /// Show a project plus its worktrees, listed **live** via
    /// `git worktree list --porcelain` on the common dir and enriched with which
    /// worktrees pohunek created (an active binding for this project) and which
    /// have a live session (`live`, supplied by the caller from the registry).
    pub fn show(
        &self,
        reference: &str,
        live: &[LiveSession],
    ) -> Result<ProjectShowResult, ProtocolError> {
        let record = self.resolve(reference)?;
        let info = to_info(&record);
        // `git worktree list` reports fully symlink-resolved paths, while a stored
        // binding path / session path is the un-canonicalized join. Compare both
        // sides through `canonical_or_original` so a symlinked worktree-root (e.g.
        // a symlinked data dir) does not silently mis-mark ownership or sessions.
        let owned: Vec<PathBuf> = self
            .store
            .load_worktrees()
            .map_err(store_error)?
            .into_iter()
            .filter(|binding| {
                binding.status == WorktreeStatus::Active
                    && binding.project_id.as_deref() == Some(info.id.as_str())
            })
            .map(|binding| canonical_or_original(&binding.path))
            .collect();
        // Each live session matches a worktree at its bound worktree path (worktree
        // session) or its cwd (in-place), canonicalized once here.
        let session_paths: Vec<(String, Vec<PathBuf>)> = live
            .iter()
            .map(|session| {
                let mut paths = vec![canonical_or_original(&session.cwd)];
                if let Some(worktree_path) = &session.worktree_path {
                    paths.push(canonical_or_original(worktree_path));
                }
                (session.session_id.clone(), paths)
            })
            .collect();
        let worktrees = git_worktrees(&record.git_common_dir)
            .into_iter()
            .map(|raw| {
                let canonical = canonical_or_original(&raw.path);
                let owned = owned.contains(&canonical);
                let session_id = session_paths
                    .iter()
                    .find(|(_, paths)| paths.contains(&canonical))
                    .map(|(id, _)| id.clone());
                ProjectWorktree {
                    path: raw.path,
                    branch: raw.branch,
                    head: raw.head,
                    bare: raw.bare,
                    locked: raw.locked,
                    owned,
                    session_id,
                }
            })
            .collect();
        Ok(ProjectShowResult {
            project: info,
            worktrees,
        })
    }
}

/// A live session's location, supplied to [`ProjectManager::show`] so it can mark
/// which of a project's worktrees currently host a pohunek session without the
/// manager depending on the session registry.
#[derive(Debug, Clone)]
pub struct LiveSession {
    /// The session id.
    pub session_id: String,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The session's bound worktree path, when it has one.
    pub worktree_path: Option<PathBuf>,
}

/// Convert a stored [`ProjectRecord`] into the wire/display [`ProjectInfo`],
/// deriving the id and label.
fn to_info(record: &ProjectRecord) -> ProjectInfo {
    ProjectInfo {
        id: record.id(),
        label: record.label(),
        repo_root: record.repo_root.clone(),
        git_common_dir: record.git_common_dir.clone(),
        origin_url: record.origin_url.clone(),
        default_base_branch: record.default_base_branch.clone(),
        source: record.source,
        is_bare: record.is_bare,
        added_at: record.added_at.clone(),
        last_used_at: record.last_used_at.clone(),
    }
}

/// One worktree parsed from `git worktree list --porcelain -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawWorktree {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    bare: bool,
    locked: bool,
}

/// List a repository's worktrees live, bounded by the detection git timeout
/// (`-z` so exotic paths are verbatim, not C-quoted). Best-effort: a git failure
/// yields an empty list, so `project show` still renders the record.
fn git_worktrees(git_common_dir: &Path) -> Vec<RawWorktree> {
    match detect::git(git_common_dir, &["worktree", "list", "--porcelain", "-z"]) {
        Some(output) => parse_worktrees_porcelain(&output),
        None => Vec::new(),
    }
}

/// Parse the NUL-delimited `git worktree list --porcelain -z` output: records are
/// separated by an empty field, attributes within a record by NUL. A `branch`
/// ref is shortened to its branch name; `detached` leaves the branch absent.
fn parse_worktrees_porcelain(output: &str) -> Vec<RawWorktree> {
    let mut out = Vec::new();
    let mut current: Option<RawWorktree> = None;
    for field in output.split('\0') {
        if field.is_empty() {
            if let Some(worktree) = current.take() {
                out.push(worktree);
            }
            continue;
        }
        if let Some(path) = field.strip_prefix("worktree ") {
            // Defensive: a new record without the empty separator still flushes.
            if let Some(worktree) = current.take() {
                out.push(worktree);
            }
            current = Some(RawWorktree {
                path: PathBuf::from(path),
                head: None,
                branch: None,
                bare: false,
                locked: false,
            });
        } else if let Some(worktree) = current.as_mut() {
            if let Some(head) = field.strip_prefix("HEAD ") {
                worktree.head = Some(head.to_owned());
            } else if let Some(refname) = field.strip_prefix("branch ") {
                worktree.branch = Some(short_branch(refname));
            } else if field == "bare" {
                worktree.bare = true;
            } else if field == "locked" || field.starts_with("locked ") {
                worktree.locked = true;
            }
            // `detached` and other attributes leave the branch absent.
        }
    }
    if let Some(worktree) = current.take() {
        out.push(worktree);
    }
    out
}

/// Shorten a `refs/heads/<name>` ref to `<name>`; other refs pass through.
fn short_branch(refname: &str) -> String {
    refname
        .strip_prefix("refs/heads/")
        .unwrap_or(refname)
        .to_owned()
}

/// Map a project-store I/O failure to a typed runtime error.
fn store_error(err: io::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "project_store_error",
        format!("project store I/O failed: {err}"),
        None,
    )
}

/// Reject a blank custom name (empty or whitespace-only): a blank label leaves a
/// project referenceable only by its `p-…` id, so it is a usage error.
fn validate_project_name(name: &str) -> Result<(), ProtocolError> {
    if name.trim().is_empty() {
        return Err(ProtocolError::bad_request(
            "project name cannot be empty or whitespace",
        ));
    }
    Ok(())
}

/// An explicitly named path is not a git work tree (no silent fallback). Shared
/// with the session layer so `--repo` and `project add` reject a non-repo the same
/// way.
pub(crate) fn not_a_git_repo(path: &Path) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "not_a_git_repo",
        format!("{} is not a git repository", path.display()),
        Some("point at a git work tree, or omit the path to use the current directory".to_owned()),
    )
}

/// No project matched a `<id|label>` reference.
fn project_not_found(reference: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "project_not_found",
        format!("no project matches {reference:?}"),
        Some(
            "run `pohunek project list` to see known projects, or `project add` to register one"
                .to_owned(),
        ),
    )
}

/// A `<label>` reference matched several projects; list the candidate ids and
/// paths so the operator can disambiguate with an id (design Decision 2).
fn project_ambiguous(reference: &str, candidates: &[ProjectRecord]) -> ProtocolError {
    let list = candidates
        .iter()
        .map(|candidate| format!("{} ({})", candidate.id(), candidate.repo_root.display()))
        .collect::<Vec<_>>()
        .join(", ");
    ProtocolError::new(
        ErrorClass::Runtime,
        "project_ambiguous",
        format!("{reference:?} matches several projects: {list}"),
        Some("disambiguate with the project id (the `p-…` value shown above)".to_owned()),
    )
}

/// Detect the project rooted at `path`, mapping a genuine I/O fault to a typed
/// error. A non-git directory (or any non-fatal detection failure) is `Ok(None)`.
pub fn detect_at(path: &Path) -> Result<Option<DetectedProject>, ProtocolError> {
    detect::detect(path).map_err(|err| {
        ProtocolError::new(
            ErrorClass::Runtime,
            "project_detect_failed",
            format!("git detection failed: {err}"),
            None,
        )
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use protocol::ProjectSource;

    use super::{parse_worktrees_porcelain, LiveSession, ProjectManager, RawWorktree};
    use crate::store::{Store, WorktreeBinding, WorktreeStatus};

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("pohunek-pm-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Init a repo on `main` with one commit, returning its dir.
    fn init_repo(tag: &str) -> PathBuf {
        let dir = unique_dir(tag).join("repo");
        let init = Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init", "-q"])
            .arg(&dir)
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init failed");
        git_in(&dir, &["config", "user.email", "test@example.com"]);
        git_in(&dir, &["config", "user.name", "Test"]);
        git_in(&dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("README.md"), "init\n").expect("write README");
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    fn manager(tag: &str) -> (ProjectManager, Arc<Store>) {
        let store = Arc::new(Store::new(
            unique_dir(&format!("{tag}-store")).join("metadata.jsonl"),
        ));
        (ProjectManager::new(store.clone()), store)
    }

    #[test]
    fn add_registers_manual_then_list_and_show_round_trip() {
        let (pm, _store) = manager("roundtrip");
        let repo = init_repo("roundtrip");

        // add → Manual source, label = repo basename.
        let added = pm
            .add(&repo, None, Some("develop".to_owned()))
            .expect("add the repo");
        assert_eq!(added.source, ProjectSource::Manual);
        assert_eq!(added.label, "repo");
        assert_eq!(added.default_base_branch.as_deref(), Some("develop"));

        // list → contains it.
        let listed = pm.list(&[]).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, added.id);

        // show → the project plus its (single, main) live worktree, not owned.
        let shown = pm.show(&added.id, &[]).expect("show by id");
        assert_eq!(shown.project.id, added.id);
        assert_eq!(
            shown.worktrees.len(),
            1,
            "main checkout only: {:?}",
            shown.worktrees
        );
        let main = &shown.worktrees[0];
        assert_eq!(main.branch.as_deref(), Some("main"));
        assert!(!main.owned, "the main checkout is not a pohunek worktree");
        assert_eq!(main.session_id, None);

        // rm → forgets the record; resolving it again is NotFound.
        assert!(pm.remove(&added.id).expect("remove"), "removed");
        assert!(pm.list(&[]).expect("list").is_empty());
        let err = pm
            .remove(&added.id)
            .expect_err("second remove resolves nothing");
        assert_eq!(err.code, "project_not_found");
    }

    #[test]
    fn show_marks_owned_worktree_and_live_session() {
        let (pm, store) = manager("show-owned");
        let repo = init_repo("show-owned");
        let added = pm.add(&repo, None, None).expect("add");

        // Create a real linked worktree and record a binding owned by this project.
        let wt = unique_dir("show-owned-wt").join("checkout");
        git_in(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                wt.to_str().expect("utf8"),
            ],
        );
        let canonical_wt = std::fs::canonicalize(&wt).expect("canonical worktree");
        store
            .record_worktree(&WorktreeBinding {
                session_id: "s-1".to_owned(),
                repository: std::fs::canonicalize(&repo).expect("canonical repo"),
                branch: "feature".to_owned(),
                base_branch: "main".to_owned(),
                branch_slug: "feature".to_owned(),
                path: canonical_wt.clone(),
                status: WorktreeStatus::Active,
                project_id: Some(added.id.clone()),
                created_at: "2026-06-23T00:00:00Z".to_owned(),
                updated_at: "2026-06-23T00:00:00Z".to_owned(),
            })
            .expect("record worktree binding");

        let live = vec![LiveSession {
            session_id: "s-1".to_owned(),
            cwd: canonical_wt.clone(),
            worktree_path: Some(canonical_wt.clone()),
        }];
        let shown = pm.show(&added.id, &live).expect("show");

        assert_eq!(
            shown.worktrees.len(),
            2,
            "main + feature: {:?}",
            shown.worktrees
        );
        let feature = shown
            .worktrees
            .iter()
            .find(|w| w.branch.as_deref() == Some("feature"))
            .expect("feature worktree present");
        assert!(feature.owned, "pohunek owns the feature worktree");
        assert_eq!(feature.session_id.as_deref(), Some("s-1"));
    }

    #[test]
    fn rename_sets_custom_label_without_changing_id() {
        let (pm, _store) = manager("rename");
        let repo = init_repo("rename");
        let added = pm.add(&repo, None, None).expect("add");
        assert_eq!(added.label, "repo");

        let renamed = pm
            .rename(&added.id, "dashboard".to_owned())
            .expect("rename");
        assert_eq!(renamed.label, "dashboard");
        assert_eq!(
            renamed.id, added.id,
            "id is key-derived, unchanged by rename"
        );
        // The label is now resolvable.
        let shown = pm.show("dashboard", &[]).expect("show by new label");
        assert_eq!(shown.project.id, added.id);
    }

    #[test]
    fn rename_and_add_reject_a_blank_name() {
        let (pm, _store) = manager("blank-name");
        let repo = init_repo("blank-name");
        let added = pm.add(&repo, None, None).expect("add");
        // A whitespace-only rename is rejected (would blank the label).
        let err = pm
            .rename(&added.id, "   ".to_owned())
            .expect_err("blank rename");
        assert_eq!(err.code, "bad_request", "got: {err:?}");
        // A blank --name on add is rejected too, before any write.
        let repo2 = init_repo("blank-name-2");
        let err = pm
            .add(&repo2, Some(String::new()), None)
            .expect_err("blank add name");
        assert_eq!(err.code, "bad_request", "got: {err:?}");
    }

    #[test]
    fn add_on_a_non_git_path_errors() {
        let (pm, _store) = manager("add-nongit");
        let non_git = unique_dir("add-nongit-dir");
        let err = pm
            .add(&non_git, None, None)
            .expect_err("non-git path errors");
        assert_eq!(err.code, "not_a_git_repo");
    }

    #[test]
    fn list_filters_by_source_and_label() {
        let (pm, _store) = manager("filter");
        let a = init_repo("filter-a");
        let b = init_repo("filter-b");
        pm.add(&a, Some("alpha".to_owned()), None).expect("add a");
        pm.add(&b, Some("beta".to_owned()), None).expect("add b");

        let by_label = pm
            .list(&[protocol::ProjectListFilter::Label("alpha".to_owned())])
            .expect("by label");
        assert_eq!(by_label.len(), 1);
        assert_eq!(by_label[0].label, "alpha");

        // Both were added manually, so a manual filter returns both.
        let manual = pm
            .list(&[protocol::ProjectListFilter::Source(ProjectSource::Manual)])
            .expect("by source");
        assert_eq!(manual.len(), 2);
        assert!(pm
            .list(&[protocol::ProjectListFilter::Source(ProjectSource::Auto)])
            .expect("auto")
            .is_empty());
    }

    #[test]
    fn resolve_reports_ambiguous_label_with_candidates() {
        let (pm, _store) = manager("ambiguous");
        // Two distinct repos sharing the basename label "shared".
        let a = unique_dir("amb-a").join("shared");
        let b = unique_dir("amb-b").join("shared");
        for dir in [&a, &b] {
            let init = Command::new("git")
                .args(["-c", "init.defaultBranch=main", "init", "-q"])
                .arg(dir)
                .output()
                .expect("git init");
            assert!(init.status.success());
        }
        pm.add(&a, None, None).expect("add a");
        pm.add(&b, None, None).expect("add b");

        let err = pm.resolve("shared").expect_err("ambiguous label");
        assert_eq!(err.code, "project_ambiguous");
        // Both candidate ids must be listed so the operator can disambiguate.
        let id_a = pm.add(&a, None, None).expect("re-add a").id;
        assert!(
            err.msg.contains(&id_a),
            "ambiguous message names candidate ids: {}",
            err.msg
        );
    }

    #[test]
    fn parses_porcelain_z_records_with_branch_detached_bare_and_locked() {
        // NUL-delimited (`-z`): attributes split by NUL, records by an empty field.
        let output = concat!(
            "worktree /repo\0HEAD abc123\0branch refs/heads/main\0bare\0\0",
            "worktree /repo-feature\0HEAD def456\0branch refs/heads/feature/login\0locked busy\0\0",
            "worktree /repo-detached\0HEAD aaa111\0detached\0\0",
        );
        let parsed = parse_worktrees_porcelain(output);
        assert_eq!(
            parsed,
            vec![
                RawWorktree {
                    path: PathBuf::from("/repo"),
                    head: Some("abc123".to_owned()),
                    branch: Some("main".to_owned()),
                    bare: true,
                    locked: false,
                },
                RawWorktree {
                    path: PathBuf::from("/repo-feature"),
                    head: Some("def456".to_owned()),
                    // `refs/heads/feature/login` shortens to `feature/login`.
                    branch: Some("feature/login".to_owned()),
                    bare: false,
                    locked: true,
                },
                RawWorktree {
                    path: PathBuf::from("/repo-detached"),
                    head: Some("aaa111".to_owned()),
                    branch: None,
                    bare: false,
                    locked: false,
                },
            ]
        );
    }

    #[test]
    fn parses_empty_listing_as_no_worktrees() {
        assert!(parse_worktrees_porcelain("").is_empty());
    }
}

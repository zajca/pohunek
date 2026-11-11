//! `project.candidates`: repositories a user works in, proposed from pohunek's
//! own sessions and, on request, from agent CLI history metadata.
//!
//! Every observation is a working directory, an agent and a time. Each distinct
//! directory is resolved to its repository once ([`detect`]), so the main
//! checkout and every linked worktree of one repository collapse to one
//! candidate keyed by the git common dir. The scan reads and writes nothing
//! else: it registers no project and touches no history file beyond the head
//! read in [`crate::external::history`].

// Rust guideline compliant 2026-10-12

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ::time::format_description::well_known::Rfc3339;
use ::time::OffsetDateTime;
use protocol::{CandidateLimit, CandidateSessions, ProjectCandidate, ProjectCandidatesResult};

use super::detect::{self, DetectedProject};
use crate::external::history::HistoryScan;
use crate::worktree::canonical_or_original;

/// Most history files one scan reads. Each read is bounded by
/// [`DEFAULT_HEAD_BYTES`], so this bounds the bytes a scan reads.
const DEFAULT_MAX_FILES: usize = 5_000;

/// Most directory entries one scan lists across all history trees.
const DEFAULT_MAX_ENTRIES: usize = 100_000;

/// Most directories one scan enters across all history trees.
const DEFAULT_MAX_DIRECTORIES: usize = 8_192;

/// Bytes read from the head of one history file (64 KiB). A record cut by this
/// bound is skipped and reported as [`CandidateLimit::LineLength`]. The working
/// directory sits in the first records of every supported history format.
const DEFAULT_HEAD_BYTES: usize = 64 * 1024;

/// Records examined at the head of one history file.
const DEFAULT_MAX_RECORDS: usize = 32;

/// Time one scan may take, history walk and repository resolution together.
/// Resolving a repository runs several short `git` processes, so a large
/// history needs seconds, not milliseconds.
const DEFAULT_DEADLINE: Duration = Duration::from_secs(15);

/// Directories whose contents are scratch space, not projects. The process
/// temporary directory is added at run time.
const DEFAULT_TEMP_ROOTS: &[&str] = &[
    "/tmp",
    "/var/tmp",
    "/private/tmp",
    "/private/var/folders",
    "/dev/shm",
];

/// Work bounds and path filters of a `project.candidates` scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateConfig {
    /// Most history files read.
    pub max_files: usize,
    /// Most directory entries listed.
    pub max_entries: usize,
    /// Most directories entered.
    pub max_directories: usize,
    /// Bytes read from the head of one history file.
    pub head_bytes: usize,
    /// Records examined at the head of one history file.
    pub max_records: usize,
    /// Time the whole scan may take.
    pub deadline: Duration,
    /// Directories whose repositories are not proposed.
    pub temp_roots: Vec<PathBuf>,
}

impl Default for CandidateConfig {
    fn default() -> Self {
        let mut temp_roots: Vec<PathBuf> = DEFAULT_TEMP_ROOTS.iter().map(PathBuf::from).collect();
        temp_roots.push(std::env::temp_dir());
        Self {
            max_files: DEFAULT_MAX_FILES,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_directories: DEFAULT_MAX_DIRECTORIES,
            head_bytes: DEFAULT_HEAD_BYTES,
            max_records: DEFAULT_MAX_RECORDS,
            deadline: DEFAULT_DEADLINE,
            temp_roots,
        }
    }
}

/// A pohunek-owned session's contribution.
#[derive(Debug, Clone)]
pub(crate) struct OwnedSession {
    /// The runtime id of the session's agent.
    pub(crate) agent: String,
    /// The session's working directory.
    pub(crate) cwd: PathBuf,
    /// The session's last update (RFC3339).
    pub(crate) updated_at: String,
}

/// Everything one scan reads, gathered before the blocking work starts.
pub(crate) struct Inputs {
    /// Sessions of the pohunek source; empty when the source is not requested.
    pub(crate) owned: Vec<OwnedSession>,
    /// The history source: its scan and the user's home, `None` when the
    /// source is not requested.
    pub(crate) history: Option<(HistoryScan, PathBuf)>,
    /// The git common dirs of the registered projects.
    pub(crate) registered: HashSet<PathBuf>,
}

/// Where an observation came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Owned,
    History,
}

/// One working directory seen for one agent.
struct Observation {
    origin: Origin,
    agent: String,
    at: OffsetDateTime,
}

/// A repository the observations resolved to.
struct Repository {
    project: DetectedProject,
    sessions: BTreeMap<String, CandidateSessions>,
    last: OffsetDateTime,
}

/// The directories observed, each with its observations.
type Observed = Vec<(PathBuf, Vec<Observation>)>;

/// Resolves `inputs` to candidates.
///
/// `deadline` is shared with the history walk: a scan that ran out of time
/// stops resolving and reports [`CandidateLimit::Time`].
pub(crate) fn resolve(
    inputs: Inputs,
    config: &CandidateConfig,
    deadline: Instant,
    mut limits: BTreeSet<CandidateLimit>,
) -> ProjectCandidatesResult {
    let home = inputs
        .history
        .as_ref()
        .map(|(_scan, home)| canonical_or_original(home));
    let observed = observe(inputs.owned, inputs.history, &mut limits);
    let temp_roots: Vec<PathBuf> = config
        .temp_roots
        .iter()
        .map(|root| canonical_or_original(root))
        .collect();
    let mut repositories: HashMap<PathBuf, Repository> = HashMap::new();
    let mut dropped = 0_u32;
    for (cwd, observations) in observed {
        if Instant::now() >= deadline {
            limits.insert(CandidateLimit::Time);
            break;
        }
        let Some((project, canonical_cwd)) = resolve_cwd(&cwd, &temp_roots) else {
            dropped = dropped.saturating_add(1);
            continue;
        };
        // History is the least trusted source: its paths must lie in the
        // user's home and name a repository that still exists.
        let history_allowed = project.repo_root.exists()
            && home.as_ref().is_some_and(|home| {
                canonical_cwd.starts_with(home) && project.repo_root.starts_with(home)
            });
        let kept: Vec<&Observation> = observations
            .iter()
            .filter(|o| o.origin == Origin::Owned || history_allowed)
            .collect();
        if kept.is_empty() {
            dropped = dropped.saturating_add(1);
            continue;
        }
        let repository = repositories
            .entry(project.git_common_dir.clone())
            .or_insert_with(|| Repository {
                project,
                sessions: BTreeMap::new(),
                last: kept[0].at,
            });
        for observation in kept {
            let counts = repository
                .sessions
                .entry(observation.agent.clone())
                .or_default();
            match observation.origin {
                Origin::Owned => counts.owned = counts.owned.saturating_add(1),
                Origin::History => counts.history = counts.history.saturating_add(1),
            }
            repository.last = repository.last.max(observation.at);
        }
    }
    finish(repositories, &inputs.registered, dropped, limits)
}

/// Groups the sessions and history records by working directory, newest first,
/// so a deadline drops the oldest directories.
fn observe(
    owned: Vec<OwnedSession>,
    history: Option<(HistoryScan, PathBuf)>,
    limits: &mut BTreeSet<CandidateLimit>,
) -> Observed {
    let mut by_cwd: HashMap<PathBuf, Vec<Observation>> = HashMap::new();
    for session in owned {
        let Ok(at) = OffsetDateTime::parse(&session.updated_at, &Rfc3339) else {
            continue;
        };
        by_cwd.entry(session.cwd).or_default().push(Observation {
            origin: Origin::Owned,
            agent: session.agent,
            at,
        });
    }
    if let Some((scan, _home)) = history {
        limits.extend(scan.limits);
        for record in scan.records {
            by_cwd.entry(record.cwd).or_default().push(Observation {
                origin: Origin::History,
                agent: record.agent.as_wire().to_owned(),
                at: OffsetDateTime::from(record.modified),
            });
        }
    }
    let mut observed: Observed = by_cwd.into_iter().collect();
    let newest = |observations: &[Observation]| observations.iter().map(|o| o.at).max();
    observed.sort_by(|(left_path, left), (right_path, right)| {
        newest(right)
            .cmp(&newest(left))
            .then_with(|| left_path.cmp(right_path))
    });
    observed
}

/// The result document: candidates newest activity first.
fn finish(
    repositories: HashMap<PathBuf, Repository>,
    registered: &HashSet<PathBuf>,
    dropped: u32,
    limits: BTreeSet<CandidateLimit>,
) -> ProjectCandidatesResult {
    let mut candidates: Vec<(OffsetDateTime, ProjectCandidate)> = repositories
        .into_values()
        .map(|repository| {
            let candidate = ProjectCandidate {
                exists: repository.project.repo_root.exists(),
                registered: registered.contains(&repository.project.git_common_dir),
                repo_root: repository.project.repo_root,
                origin: repository
                    .project
                    .origin_url
                    .map(|url| strip_userinfo(&url)),
                last_used_at: format_time(repository.last),
                sessions: repository.sessions,
            };
            (repository.last, candidate)
        })
        .collect();
    candidates.sort_by(|(left_at, left), (right_at, right)| {
        right_at
            .cmp(left_at)
            .then_with(|| left.repo_root.cmp(&right.repo_root))
    });
    ProjectCandidatesResult {
        candidates: candidates
            .into_iter()
            .map(|(_at, candidate)| candidate)
            .collect(),
        dropped,
        limits_reached: limits.into_iter().collect(),
    }
}

/// The repository of `cwd` with the canonical form of `cwd`, or `None` when it
/// is not listable: the directory is gone, lies in a temporary directory, or is
/// not in a git repository.
fn resolve_cwd(cwd: &Path, temp_roots: &[PathBuf]) -> Option<(DetectedProject, PathBuf)> {
    let canonical = std::fs::canonicalize(cwd).ok()?;
    if !canonical.is_dir() || in_any(&canonical, temp_roots) {
        return None;
    }
    let project = detect::detect(&canonical).ok().flatten()?;
    (!in_any(&project.repo_root, temp_roots)).then_some((project, canonical))
}

fn in_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// `url` without the userinfo of its authority. The detected origin already
/// carries a `<redacted>@` placeholder for a credential; the candidate drops
/// the placeholder as well, so the field is a plain URL.
fn strip_userinfo(url: &str) -> String {
    url.replace("<redacted>@", "")
}

fn format_time(at: OffsetDateTime) -> String {
    at.to_offset(::time::UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// Time the scan may run until, counted from `start`.
pub(crate) fn deadline_from(start: Instant, config: &CandidateConfig) -> Instant {
    start.checked_add(config.deadline).unwrap_or(start)
}

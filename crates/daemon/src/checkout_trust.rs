//! The trust predicate for a checkout a runtime may load project
//! configuration from.
//!
//! A checkout is trusted only when every term holds; any error, ambiguity or
//! missing fact means it is not:
//!
//! 1. The project record has an `origin_url` that is exactly
//!    `https://github.com/<owner>/<repo>[.git]` or
//!    `git@github.com:<owner>/<repo>[.git]` (scheme, host and owner compared
//!    without regard to letter case, a `.git` suffix stripped), and the
//!    lower-cased `owner/repo` is in the host's `trusted_repositories`. The
//!    launch directory holds a `.git` entry.
//! 2. The daemon fetches `HEAD` of `https://github.com/<owner>/<repo>.git`
//!    (one commit, trees only) into a private bare repository it creates for
//!    the evaluation. Git verifies every fetched object, so the commit and its
//!    trees are what GitHub serves.
//! 3. For each declared path, the files on disk are exactly the blobs the
//!    tree of that commit holds there, with the same contents (hashed by git
//!    in the private repository) and executable bits. Symbolic links,
//!    submodules, special files, nested `.git` entries, a non-directory
//!    ancestor and an entry that differs from a path component only by ASCII
//!    letter case all refuse.
//!
//! The scratch repository lives in a daemon-private directory, never in a
//! directory a launched session can write. No git command runs against the checkout: its refs, index, objects and
//! configuration are never read, so a process that can write them cannot
//! influence the answer. Every git command runs in the private repository
//! through the bounded runner with a cleared environment, no user or system
//! configuration, no hooks, no pager and no lazy fetch, and all commands of
//! one evaluation share one deadline. The files can still change between the
//! evaluation and the moment the runtime reads them; a process that can write
//! the checkout can win that race.

// Rust guideline compliant 2026-11-06

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::bounded_run::{self, RunLimits, RunOutcome, RunSpec, StderrPolicy};

/// Longest a single git command of the predicate may run, within the total
/// deadline. The commands read objects of one checkout or one remote ref, so a
/// command that takes longer is a stalled filesystem or network.
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest one evaluation may take across all its git commands, so a launch
/// holding its lifecycle lock is never delayed longer by a hostile or stalled
/// checkout or network.
pub(crate) const TOTAL_DEADLINE: Duration = Duration::from_secs(30);

/// Most stdout bytes one git command may produce; the runner lowers a larger
/// cap to its own maximum. A larger answer is a failure, which means not
/// trusted.
const GIT_STDOUT_CAP_BYTES: usize = bounded_run::MAX_STDOUT_CAP_BYTES;

/// Most files below the declared paths the predicate compares; a checkout with
/// more is not trusted.
const MAX_COMPARED_FILES: usize = 2048;

/// Deepest directory nesting the predicate walks below a declared path.
const MAX_COMPARED_DEPTH: usize = 32;

/// The null device, used as the user and system git configuration and as the
/// hooks directory.
const NULL_DEVICE: &str = "/dev/null";

/// Host of the only remote the predicate accepts.
const GITHUB_HOST: &str = "github.com";

/// Scheme prefix of the HTTPS form of a GitHub remote.
const HTTPS_SCHEME: &str = "https://";

/// User prefix of the SCP form of a GitHub remote.
const SCP_USER: &str = "git@";

/// Suffix git accepts on a remote URL.
const GIT_SUFFIX: &str = ".git";

/// Mode of the daemon-private scratch directory.
const SCRATCH_DIR_MODE: u32 = 0o700;

/// File mode of a regular, non-executable blob in a tree listing.
const MODE_FILE: &str = "100644";

/// File mode of an executable blob in a tree listing.
const MODE_EXECUTABLE: &str = "100755";

/// Owner, group and other execute bits of a file mode.
const EXECUTE_BITS: u32 = 0o111;

/// Git settings that make a run independent of the user's configuration and
/// of anything the checkout can name.
const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", NULL_DEVICE),
    ("GIT_CONFIG_SYSTEM", NULL_DEVICE),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_ATTR_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_NO_REPLACE_OBJECTS", "1"),
    ("GIT_LITERAL_PATHSPECS", "1"),
    ("GIT_PAGER", "cat"),
    ("GIT_NO_LAZY_FETCH", "1"),
];

/// Per-command git options placed before the subcommand.
const GIT_OPTIONS: &[&str] = &[
    "--no-pager",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.pager=cat",
    "-c",
    "core.attributesFile=/dev/null",
    "-c",
    "core.excludesFile=/dev/null",
];

/// Extra options of the fetch: https only, no credential helper, no redirect,
/// and every received object verified.
const GIT_FETCH_OPTIONS: &[&str] = &[
    "-c",
    "protocol.allow=never",
    "-c",
    "protocol.https.allow=always",
    "-c",
    "credential.helper=",
    "-c",
    "http.followRedirects=false",
    "-c",
    "fetch.fsckObjects=true",
    "-c",
    "transfer.fsckObjects=true",
];

/// Extra options of a fetch from a replaced source: local paths only.
const GIT_LOCAL_FETCH_OPTIONS: &[&str] = &[
    "-c",
    "protocol.allow=never",
    "-c",
    "protocol.file.allow=always",
    "-c",
    "fetch.fsckObjects=true",
    "-c",
    "transfer.fsckObjects=true",
];

/// Transport git may use for the fetch, by source.
const HTTPS_ONLY: &str = "https";
const FILE_ONLY: &str = "file";

/// Names the URL a repository's remote `HEAD` is fetched from, given the
/// lower-cased `owner/repo`. `None` is a failed lookup.
#[derive(Clone)]
pub(crate) struct RemoteSourceResolver(Arc<RemoteSourceRead>);

/// The function behind a [`RemoteSourceResolver`].
type RemoteSourceRead = dyn Fn(&str) -> Option<String> + Send + Sync;

impl RemoteSourceResolver {
    /// A resolver that answers with `read`.
    pub(crate) fn new<F>(read: F) -> Self
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        Self(Arc::new(read))
    }

    fn resolve(&self, repository: &str) -> Option<String> {
        (self.0)(repository)
    }
}

impl std::fmt::Debug for RemoteSourceResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteSourceResolver")
    }
}

/// What the predicate evaluates.
#[derive(Clone, Copy)]
pub(crate) struct CheckoutFacts<'a> {
    /// The launch directory; the declared paths are read below it.
    pub(crate) cwd: &'a Path,
    /// The `origin_url` of the project record of the launch.
    pub(crate) stored_origin: Option<&'a str>,
    /// Lower-cased `owner/repo` entries of the host's `trusted_repositories`.
    pub(crate) trusted_repositories: &'a [String],
    /// Checkout paths the runtime declares, relative to the launch directory.
    pub(crate) paths: &'a [String],
    /// Daemon-private directory the evaluation creates its scratch repository
    /// in; no launched session can write below it.
    pub(crate) scratch_root: &'a Path,
    /// Replacement for the GitHub URL the remote `HEAD` is fetched from.
    pub(crate) remote_source: Option<&'a RemoteSourceResolver>,
}

/// Why the predicate could not reach an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// The `git` executable could not be resolved.
    NoGit,
    /// The launch directory is not valid UTF-8, so it cannot be an argument.
    CwdNotUtf8,
    /// The command ended in a timeout, an overflow or a spawn failure.
    Run,
    /// The total deadline of the evaluation passed.
    Deadline,
    /// The command exited with a status the predicate does not expect.
    Status,
    /// The command printed bytes the predicate cannot read.
    Output,
    /// The remote `HEAD` could not be fetched.
    Remote,
    /// A declared path could not be read from disk.
    Disk,
}

impl Failure {
    fn reason(self) -> &'static str {
        match self {
            Self::NoGit => "git is not available",
            Self::CwdNotUtf8 => "the launch directory is not valid UTF-8",
            Self::Run => "a git command timed out, overflowed or could not run",
            Self::Deadline => "the evaluation passed its total deadline",
            Self::Status => "a git command failed",
            Self::Output => "a git command printed output the predicate cannot read",
            Self::Remote => "the remote HEAD could not be fetched",
            Self::Disk => "a declared path could not be read",
        }
    }
}

/// How one git command ended within the limits.
struct GitResult {
    code: Option<i32>,
    stdout: String,
}

impl GitResult {
    /// The stdout of a command that exited 0, with its separators intact.
    fn success_stdout(&self) -> Result<&str, Failure> {
        if self.code == Some(0) {
            Ok(&self.stdout)
        } else {
            Err(Failure::Status)
        }
    }

    /// The trimmed stdout of `rev-parse -q --verify`: `Some` for exit 0,
    /// `None` for exit 1 (the name resolves to nothing), a failure otherwise.
    fn verified_object(&self) -> Result<Option<&str>, Failure> {
        match self.code {
            Some(0) => Ok(Some(self.stdout.trim())),
            Some(1) if self.stdout.trim().is_empty() => Ok(None),
            _ => Err(Failure::Status),
        }
    }
}

/// Whether the checkout of `facts` is trusted.
///
/// A failure logs one warning and returns `false`. A plain "no" (an untrusted
/// repository, a changed path) logs at debug level only, because it is the
/// expected answer for most checkouts.
#[must_use]
pub(crate) fn is_trusted(facts: &CheckoutFacts<'_>) -> bool {
    match evaluate(facts) {
        Ok(Verdict::Trusted) => true,
        Ok(Verdict::Untrusted(why)) => {
            debug!(
                reason = why,
                "checkout not trusted for the runtime's trusted arguments"
            );
            false
        }
        Err(failure) => {
            warn!(
                event = "checkout_trust.failed",
                checkout = %facts.cwd.display(),
                reason = failure.reason(),
                "the checkout trust predicate could not be evaluated; the checkout is not trusted"
            );
            false
        }
    }
}

enum Verdict {
    Trusted,
    Untrusted(&'static str),
}

fn evaluate(facts: &CheckoutFacts<'_>) -> Result<Verdict, Failure> {
    if facts.trusted_repositories.is_empty() || facts.paths.is_empty() {
        return Ok(Verdict::Untrusted(
            "no trusted repository or no declared path",
        ));
    }
    let Some(stored) = facts.stored_origin else {
        return Ok(Verdict::Untrusted("the project has no origin"));
    };
    let Some(repository) = github_repository(stored) else {
        return Ok(Verdict::Untrusted("the origin is not a GitHub repository"));
    };
    if !facts.trusted_repositories.contains(&repository) {
        return Ok(Verdict::Untrusted("the origin repository is not trusted"));
    }
    if std::fs::symlink_metadata(facts.cwd.join(".git")).is_err() {
        return Ok(Verdict::Untrusted(
            "the launch directory is not a checkout root",
        ));
    }
    let git = Git::new(facts.scratch_root)?;
    let head = match facts.remote_source {
        Some(resolver) => {
            let source = resolver.resolve(&repository).ok_or(Failure::Remote)?;
            git.fetch_head(&source, FILE_ONLY)?
        }
        None => git.fetch_head(
            &format!("{HTTPS_SCHEME}{GITHUB_HOST}/{repository}{GIT_SUFFIX}"),
            HTTPS_ONLY,
        )?,
    };

    for path in facts.paths {
        if !path_is_plain(&git, facts.cwd, &head, path)? {
            return Ok(Verdict::Untrusted(
                "a declared path sits below a non-directory or collides by letter case",
            ));
        }
    }
    if !contents_match(&git, facts, &head)? {
        return Ok(Verdict::Untrusted(
            "a declared path differs from the remote HEAD",
        ));
    }
    Ok(Verdict::Trusted)
}

/// Whether the files below every declared path are exactly the blobs of
/// `head`'s tree there, byte for byte and in executable bit.
fn contents_match(git: &Git, facts: &CheckoutFacts<'_>, head: &str) -> Result<bool, Failure> {
    let mut listing = vec!["ls-tree", "-r", "-z", head, "--"];
    listing.extend(facts.paths.iter().map(String::as_str));
    let listing = git.run(&listing)?;
    let mut expected: BTreeMap<String, (bool, String)> = BTreeMap::new();
    for record in listing
        .success_stdout()?
        .split('\0')
        .filter(|r| !r.is_empty())
    {
        let (meta, path) = record.split_once('\t').ok_or(Failure::Output)?;
        let mut fields = meta.split(' ');
        let (Some(mode), Some("blob"), Some(object), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Ok(false);
        };
        let executable = match mode {
            MODE_FILE => false,
            MODE_EXECUTABLE => true,
            _ => return Ok(false),
        };
        expected.insert(path.to_owned(), (executable, object.to_owned()));
    }

    let mut on_disk: Vec<(String, bool)> = Vec::new();
    for path in facts.paths {
        if !collect_files(facts.cwd, path, 0, &mut on_disk)? {
            return Ok(false);
        }
    }
    if on_disk.len() != expected.len() {
        return Ok(false);
    }
    if on_disk.is_empty() {
        return Ok(true);
    }
    let root = facts.cwd.to_str().ok_or(Failure::CwdNotUtf8)?;
    let absolute: Vec<String> = on_disk
        .iter()
        .map(|(path, _)| format!("{root}/{path}"))
        .collect();
    let mut hashing = vec!["hash-object", "--no-filters", "--"];
    hashing.extend(absolute.iter().map(String::as_str));
    let hashes = git.run(&hashing)?;
    let hashes: Vec<&str> = hashes.success_stdout()?.lines().collect();
    if hashes.len() != on_disk.len() {
        return Err(Failure::Output);
    }
    Ok(on_disk
        .iter()
        .zip(hashes)
        .all(|((path, executable), hash)| {
            expected.get(path).is_some_and(|(want_executable, want)| {
                want_executable == executable && want == hash
            })
        }))
}

/// Appends the regular files below `relative` (relative to `cwd`) to `out`
/// with their executable bit. `Ok(false)` refuses: a symbolic link, a special
/// file, a `.git` entry, a name that is not UTF-8, or more than the compared
/// limits. A missing path holds no file.
fn collect_files(
    cwd: &Path,
    relative: &str,
    depth: usize,
    out: &mut Vec<(String, bool)>,
) -> Result<bool, Failure> {
    use std::os::unix::fs::PermissionsExt as _;

    if depth > MAX_COMPARED_DEPTH {
        return Ok(false);
    }
    let metadata = match std::fs::symlink_metadata(cwd.join(relative)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(_unreadable) => return Err(Failure::Disk),
    };
    let kind = metadata.file_type();
    if kind.is_file() {
        if out.len() >= MAX_COMPARED_FILES {
            return Ok(false);
        }
        out.push((
            relative.to_owned(),
            metadata.permissions().mode() & EXECUTE_BITS != 0,
        ));
        return Ok(true);
    }
    if !kind.is_dir() {
        return Ok(false);
    }
    let entries = std::fs::read_dir(cwd.join(relative)).map_err(|_unreadable| Failure::Disk)?;
    for entry in entries {
        let entry = entry.map_err(|_unreadable| Failure::Disk)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Ok(false);
        };
        if name.eq_ignore_ascii_case(GIT_SUFFIX) {
            return Ok(false);
        }
        if !collect_files(cwd, &format!("{relative}/{name}"), depth + 1, out)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether `path` is reached through real directories only: in `head`'s tree
/// every proper ancestor is a tree (or absent), no entry along the path
/// differs from its component only by ASCII letter case, and no ancestor is a
/// symbolic link on disk. A case-insensitive file system would otherwise open
/// the colliding entry, and a symbolic link would redirect the read outside
/// the compared tree.
fn path_is_plain(git: &Git, cwd: &Path, head: &str, path: &str) -> Result<bool, Failure> {
    let components: Vec<&str> = path.split('/').collect();
    for depth in 0..components.len() {
        let prefix = components[..depth].join("/");
        if depth > 0 {
            if std::fs::symlink_metadata(cwd.join(&prefix))
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Ok(false);
            }
            let ancestor =
                git.run(&["rev-parse", "-q", "--verify", &format!("{head}:{prefix}")])?;
            if let Some(object) = ancestor.verified_object()? {
                let kind = git.run(&["cat-file", "-t", object])?;
                if kind.success_stdout()?.trim() != "tree" {
                    return Ok(false);
                }
            }
        }
        let listed = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        let mut listing = vec!["ls-tree", "-z", "--name-only", head];
        if !listed.is_empty() {
            listing.push("--");
            listing.push(&listed);
        }
        let names = git.run(&listing)?;
        let wanted = format!("{listed}{}", components[depth]);
        if names
            .success_stdout()?
            .split('\0')
            .any(|name| name.eq_ignore_ascii_case(&wanted) && name != wanted)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether `value` is a full git object id in lower-case hexadecimal (SHA-1 or
/// SHA-256).
fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The lower-cased `owner/repo` of a GitHub remote URL in one of the two
/// accepted forms, or `None` for any other string.
///
/// The scheme, host and owner are compared without regard to letter case and
/// a trailing `.git` is stripped; a user, port, query, fragment or extra path
/// segment refuses.
fn github_repository(url: &str) -> Option<String> {
    let rest = if url.len() > HTTPS_SCHEME.len()
        && url.is_char_boundary(HTTPS_SCHEME.len())
        && url[..HTTPS_SCHEME.len()].eq_ignore_ascii_case(HTTPS_SCHEME)
    {
        let (host, path) = url[HTTPS_SCHEME.len()..].split_once('/')?;
        host.eq_ignore_ascii_case(GITHUB_HOST).then_some(path)?
    } else {
        let after_user = url.strip_prefix(SCP_USER)?;
        let (host, path) = after_user.split_once(':')?;
        host.eq_ignore_ascii_case(GITHUB_HOST).then_some(path)?
    };
    let (owner, repo) = rest.split_once('/')?;
    let repo = repo.strip_suffix(GIT_SUFFIX).unwrap_or(repo);
    let owner_ok = !owner.is_empty()
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    let repo_ok = !repo.is_empty()
        && repo != "."
        && repo != ".."
        && repo
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    (owner_ok && repo_ok).then(|| format!("{owner}/{repo}").to_ascii_lowercase())
}

/// Runs git in a private bare repository under one total deadline.
struct Git {
    program: std::path::PathBuf,
    repository_args: Vec<String>,
    env: Vec<(String, String)>,
    deadline: Instant,
    /// Removes the private repository when the evaluation ends.
    _private: tempfile::TempDir,
}

impl Git {
    fn new(scratch_root: &Path) -> Result<Self, Failure> {
        use std::os::unix::fs::DirBuilderExt as _;

        let program = crate::git::executable().map_err(|_not_found| Failure::NoGit)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(SCRATCH_DIR_MODE)
            .create(scratch_root)
            .map_err(|_unwritable| Failure::Disk)?;
        let private = tempfile::Builder::new()
            .prefix("pohunek-trust-")
            .tempdir_in(scratch_root)
            .map_err(|_unwritable| Failure::Disk)?;
        let path = private.path().to_str().ok_or(Failure::Disk)?.to_owned();
        let mut repository_args = vec![format!("--git-dir={path}")];
        repository_args.extend(GIT_OPTIONS.iter().map(|option| (*option).to_owned()));
        let git = Self {
            program,
            repository_args,
            env: GIT_ENV
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            deadline: Instant::now() + TOTAL_DEADLINE,
            _private: private,
        };
        let init = git.execute(
            &[
                "init".to_owned(),
                "--bare".to_owned(),
                "-q".to_owned(),
                "--template=".to_owned(),
                path,
            ],
            &git.env,
        )?;
        init.success_stdout()?;
        Ok(git)
    }

    /// Runs `args` in the private repository.
    fn run(&self, args: &[&str]) -> Result<GitResult, Failure> {
        let mut command_args = self.repository_args.clone();
        command_args.extend(args.iter().map(|arg| (*arg).to_owned()));
        self.execute(&command_args, &self.env)
    }

    /// Fetches `HEAD` of `source` (one commit, trees only) into the private
    /// repository over `transport` and returns the commit it names.
    fn fetch_head(&self, source: &str, transport: &str) -> Result<String, Failure> {
        let options = if transport == FILE_ONLY {
            GIT_LOCAL_FETCH_OPTIONS
        } else {
            GIT_FETCH_OPTIONS
        };
        let mut command_args = self.repository_args.clone();
        command_args.extend(options.iter().map(|option| (*option).to_owned()));
        command_args.extend(
            [
                "fetch",
                "-q",
                "--no-tags",
                "--depth=1",
                "--filter=blob:none",
            ]
            .map(str::to_owned),
        );
        command_args.extend([source.to_owned(), "HEAD".to_owned()]);
        let mut env = self.env.clone();
        env.push(("GIT_ALLOW_PROTOCOL".to_owned(), transport.to_owned()));
        let fetched = self
            .execute(&command_args, &env)
            .map_err(|failure| match failure {
                Failure::Deadline => Failure::Deadline,
                _ => Failure::Remote,
            })?;
        if fetched.code != Some(0) {
            return Err(Failure::Remote);
        }
        let head = self.run(&["rev-parse", "-q", "--verify", "FETCH_HEAD^{commit}"])?;
        let head = head.verified_object()?.ok_or(Failure::Remote)?;
        if is_object_id(head) {
            Ok(head.to_owned())
        } else {
            Err(Failure::Remote)
        }
    }

    fn execute(&self, args: &[String], env: &[(String, String)]) -> Result<GitResult, Failure> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Failure::Deadline);
        }
        let spec = RunSpec {
            program: &self.program,
            args,
            search_path: None,
            env,
            seeded_env: &[],
            limits: RunLimits::new(GIT_COMMAND_TIMEOUT.min(remaining), GIT_STDOUT_CAP_BYTES),
            stderr: StderrPolicy::Discard,
        };
        match bounded_run::run(&spec, &Instant::now, None) {
            RunOutcome::Exited(output) => {
                let stdout = output
                    .stdout_utf8()
                    .map_err(|_invalid_utf8| Failure::Output)?
                    .to_owned();
                Ok(GitResult {
                    code: output.status().code(),
                    stdout,
                })
            }
            RunOutcome::TimedOut
            | RunOutcome::Overflow
            | RunOutcome::Cancelled
            | RunOutcome::Unavailable => Err(Failure::Run),
        }
    }
}

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
//!    tree of that commit holds there, with the same contents (all files hashed
//!    by one git process in the private repository) and executable bits. Symbolic links,
//!    submodules, special files, nested `.git` entries, a non-directory
//!    ancestor and an entry that differs from a path component only by letter
//!    case all refuse. So does a declared path that the file system resolves
//!    under another spelling (case or normalization folding).
//!
//! A resume evaluates the directory and project the session was created
//! in, not its current ones, and only when it relaunches in that directory
//! (`SessionRegistry::trusted_relaunch_args`).
//!
//! The scratch repository lives in a daemon-private directory, never in a
//! directory a launched session can write. No git command runs against the
//! checkout: its refs, index, objects and configuration are never read, so a
//! process that can write them cannot influence the answer. Every git command
//! runs in the private repository through the bounded runner with a cleared
//! environment, no user or system configuration, no hooks, no pager and no
//! lazy fetch. The walk of the declared paths on disk and all git commands of
//! one evaluation share one deadline, and the launch stops waiting for an
//! evaluation that outlives it (a stalled file system call), which means not
//! trusted.
//!
//! The launch directory is opened once; every component below it is opened
//! relative to the descriptor of its parent without following a symbolic link,
//! and a file is read in-process, up to a bound, through the descriptor that
//! was checked. A component replaced by a link or by another kind of file
//! while the evaluation runs is refused instead of redirecting a read. The
//! files can still change between the evaluation and the moment the runtime
//! reads them; a process that can write the checkout can win that race.

// Rust guideline compliant 2026-11-06

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;
use tracing::{debug, warn};

use crate::bounded_run::{self, RunLimits, RunOutcome, RunSpec, StderrPolicy};

/// Longest a single git command of the predicate may run, within the total
/// deadline. A command that takes longer is a stalled filesystem or network.
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest one evaluation may take across all its git commands and the
/// checks between its file system calls. A single file system call that never
/// returns (a stalled mount) cannot be interrupted, so the caller stops waiting
/// for the evaluation after this long and treats the checkout as not trusted;
/// the blocked thread and its scratch directory stay until the call returns.
pub const TOTAL_DEADLINE: Duration = Duration::from_secs(30);

/// Most stdout bytes one git command may produce; the runner lowers a larger
/// cap to its own maximum. A larger answer is a failure, which means not
/// trusted.
const GIT_STDOUT_CAP_BYTES: usize = bounded_run::MAX_STDOUT_CAP_BYTES;

/// Most files below the declared paths the predicate compares; a checkout with
/// more is not trusted.
const MAX_COMPARED_FILES: usize = 2048;

/// Most directory entries (files and directories) the walk of the declared
/// paths visits; a checkout with more is not trusted.
const MAX_WALKED_ENTRIES: usize = 8192;

/// Largest declared file the predicate hashes, in bytes. The declared paths
/// hold hooks, rules and plugin configuration; a larger file is not trusted
/// instead of being read.
const MAX_COMPARED_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Most bytes of all declared files together that the predicate reads; a
/// checkout with more is not trusted. The declared paths hold hooks, rules and
/// plugin configuration, so this is far above any real checkout and keeps the
/// scratch blob stream small.
const MAX_COMPARED_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Bytes read from a declared file between two deadline checks.
const READ_CHUNK_BYTES: usize = 64 * 1024;

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

/// Name of the file `git fast-import` exports the blob marks to, in the
/// private directory.
const MARKS_FILE: &str = "blob-marks";

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

/// The point of the read of a declared path at which a [`ReadObserver`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutReadStep {
    /// Right before the path is opened relative to its parent directory.
    BeforeOpen,
    /// After a declared file was opened and its size checked, right before its
    /// bytes are read.
    BeforeRead,
}

/// Runs at each [`CheckoutReadStep`] of the evaluation with the absolute path
/// being read. Lets an embedder or a test change the checkout at an exact point
/// of the read.
#[derive(Clone)]
pub(crate) struct ReadObserver(Arc<ReadObserve>);

/// The function behind a [`ReadObserver`].
type ReadObserve = dyn Fn(CheckoutReadStep, &Path) + Send + Sync;

impl ReadObserver {
    /// An observer that runs `observe`.
    pub(crate) fn new<F>(observe: F) -> Self
    where
        F: Fn(CheckoutReadStep, &Path) + Send + Sync + 'static,
    {
        Self(Arc::new(observe))
    }
}

impl std::fmt::Debug for ReadObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadObserver")
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
    /// Observer of the read of the declared paths.
    pub(crate) read_observer: Option<&'a ReadObserver>,
}

/// Why the predicate could not reach an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// The `git` executable could not be resolved.
    NoGit,
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

/// What the predicate concluded for one checkout.
pub(crate) struct Outcome {
    verdict: Result<Verdict, Failure>,
}

impl Outcome {
    /// Whether the checkout is trusted.
    pub(crate) fn trusted(&self) -> bool {
        matches!(self.verdict, Ok(Verdict::Trusted))
    }

    /// Logs a failure as one warning; a plain "no" (an untrusted repository, a
    /// changed path) logs at debug level only, because it is the expected
    /// answer for most checkouts. A trusted outcome logs nothing.
    pub(crate) fn log(&self, checkout: &Path) {
        match &self.verdict {
            Ok(Verdict::Trusted) => {}
            Ok(Verdict::Untrusted(why)) => {
                debug!(
                    reason = *why,
                    "checkout not trusted for the runtime's trusted arguments"
                );
            }
            Err(failure) => {
                warn!(
                    event = "checkout_trust.failed",
                    checkout = %checkout.display(),
                    reason = failure.reason(),
                    "the checkout trust predicate could not be evaluated; the checkout is not trusted"
                );
            }
        }
    }
}

impl Outcome {
    /// The outcome of an evaluation that could not start; the caller has
    /// logged why.
    pub(crate) fn unavailable() -> Self {
        Self {
            verdict: Ok(Verdict::Untrusted("the project record is unavailable")),
        }
    }
}

/// Evaluates the checkout of `facts`. Any error, ambiguity or missing fact is
/// an outcome that is not trusted.
#[must_use]
pub(crate) fn evaluate_trust(facts: &CheckoutFacts<'_>) -> Outcome {
    Outcome {
        verdict: evaluate(facts),
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

    let mut walk = Walk::open(facts, git)?;
    for path in facts.paths {
        if !walk.collect_declared(path)? {
            return Ok(false);
        }
    }
    let files = std::mem::take(&mut walk.files);
    let hashes = git.hash_blobs(walk.stream)?;
    if hashes.len() != files.len() {
        return Err(Failure::Output);
    }
    // Set equality: no path is seen twice, every on-disk path is expected
    // (checked with its hash below) and the counts agree.
    let distinct: std::collections::BTreeSet<&str> =
        files.iter().map(|file| file.relative.as_str()).collect();
    if distinct.len() != files.len()
        || files.len() != expected.len()
        || distinct.iter().any(|path| !expected.contains_key(*path))
    {
        return Ok(false);
    }
    for (file, hash) in files.iter().zip(&hashes) {
        let Some((want_executable, want)) = expected.get(&file.relative) else {
            return Ok(false);
        };
        if file.executable != *want_executable || hash != want {
            return Ok(false);
        }
    }
    Ok(true)
}

/// How a declared path is named on disk.
enum NameState {
    /// Every component exists under exactly the declared name; holds the
    /// descriptor of the directory that contains the last component.
    Exact(OwnedFd),
    /// Nothing on disk resolves to the declared path under any spelling.
    Absent,
    /// A sibling differs from a component only by letter case under Unicode
    /// case folding, a component is missing under its exact name yet the file
    /// system still resolves the path (a case-insensitive or
    /// normalization-insensitive file system opens a variant spelling as the
    /// declared name), a component was replaced while the lookup ran, or the
    /// lookup exceeded its bounds.
    Refused,
}

/// Whether two distinct names are one under Unicode case folding.
fn folds_together(a: &str, b: &str) -> bool {
    fn fold(name: &str) -> String {
        name.chars()
            .flat_map(char::to_uppercase)
            .flat_map(char::to_lowercase)
            .collect()
    }
    a != b && fold(a) == fold(b)
}

/// A regular file found below a declared path.
struct DiskFile {
    /// The path relative to the launch directory.
    relative: String,
    /// Whether any execute bit of the opened file is set.
    executable: bool,
}

/// The blobs of the files read so far, in the stream format of
/// `git fast-import`, written to a daemon-private file.
struct BlobStream {
    writer: std::io::BufWriter<std::fs::File>,
    blobs: usize,
}

impl BlobStream {
    /// Appends `bytes` as the next blob; its mark is its 1-based position.
    fn push(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        self.blobs += 1;
        let written = write!(
            self.writer,
            "blob\nmark :{}\ndata {}\n",
            self.blobs,
            bytes.len()
        )
        .and_then(|()| self.writer.write_all(bytes))
        .and_then(|()| self.writer.write_all(b"\n"));
        written.map_err(|_unwritable| Failure::Disk)
    }
}

/// The bounded walk of the declared paths on disk, relative to a descriptor of
/// the launch directory.
struct Walk<'a> {
    cwd: &'a Path,
    root: OwnedFd,
    deadline: Instant,
    observer: Option<&'a ReadObserver>,
    /// Directory entries visited so far, files and directories alike.
    entries: usize,
    /// Bytes of the declared files read so far.
    bytes: u64,
    /// The regular files found, in the order their blobs were streamed.
    files: Vec<DiskFile>,
    stream: BlobStream,
}

/// Flags for opening a directory without following a link in its name.
fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC
}

/// Flags for opening a file without following a link in its name or blocking
/// on a special file.
fn file_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC
}

/// Whether `errno` says the name no longer designates what was classified: a
/// link, another kind of file, or nothing.
fn changed_under_us(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::LOOP | Errno::NOTDIR | Errno::NOENT | Errno::NXIO | Errno::ISDIR
    )
}

impl<'a> Walk<'a> {
    /// Opens the launch directory of `facts` and the blob stream of `git`.
    fn open(facts: &CheckoutFacts<'a>, git: &Git) -> Result<Self, Failure> {
        let root = rustix::fs::open(
            facts.cwd,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_unreadable| Failure::Disk)?;
        Ok(Self {
            cwd: facts.cwd,
            root,
            deadline: git.deadline,
            observer: facts.read_observer,
            entries: 0,
            bytes: 0,
            files: Vec::new(),
            stream: git.blob_stream()?,
        })
    }

    fn observe(&self, step: CheckoutReadStep, relative: &str) {
        if let Some(observer) = self.observer {
            (observer.0)(step, &self.cwd.join(relative));
        }
    }

    fn check_deadline(&self) -> Result<(), Failure> {
        if Instant::now() >= self.deadline {
            Err(Failure::Deadline)
        } else {
            Ok(())
        }
    }

    /// Looks up each component of `relative` by its exact name among the
    /// entries of its parent directory, so that a file system that folds
    /// letter case or normalizes names cannot present another spelling as
    /// the declared one. Every parent is opened relative to the descriptor of
    /// the one before it, so a component replaced by a link while the lookup
    /// runs refuses. The entries read count toward the walk bounds and the
    /// deadline. A path with a component missing under its exact name is
    /// still resolved once through the file system: only `NotFound` there
    /// means that nothing answers to the declared name.
    ///
    /// On success the descriptor of the directory that contains the last
    /// component comes with it.
    fn exact_names(&mut self, relative: &str) -> Result<NameState, Failure> {
        let mut parent =
            rustix::io::fcntl_dupfd_cloexec(&self.root, 0).map_err(|_unreadable| Failure::Disk)?;
        let mut walked = String::new();
        let mut components = relative.split('/').peekable();
        while let Some(component) = components.next() {
            let mut exact = false;
            let entries = Dir::read_from(&parent).map_err(|_unreadable| Failure::Disk)?;
            for entry in entries {
                self.check_deadline()?;
                self.entries += 1;
                if self.entries > MAX_WALKED_ENTRIES {
                    return Ok(NameState::Refused);
                }
                let entry = entry.map_err(|_unreadable| Failure::Disk)?;
                let Ok(name) = std::str::from_utf8(entry.file_name().to_bytes()) else {
                    continue;
                };
                if name == component {
                    exact = true;
                } else if name != "." && name != ".." && folds_together(name, component) {
                    return Ok(NameState::Refused);
                }
            }
            if !exact {
                return Self::resolve_absent(&parent, component);
            }
            if components.peek().is_none() {
                return Ok(NameState::Exact(parent));
            }
            if !walked.is_empty() {
                walked.push('/');
            }
            walked.push_str(component);
            self.observe(CheckoutReadStep::BeforeOpen, &walked);
            parent = match rustix::fs::openat(&parent, component, directory_flags(), Mode::empty())
            {
                Ok(child) => child,
                Err(errno) if changed_under_us(errno) => return Ok(NameState::Refused),
                Err(_unreadable) => return Err(Failure::Disk),
            };
        }
        Ok(NameState::Refused)
    }

    /// Whether `component` is absent from `parent` under every spelling.
    fn resolve_absent(parent: &OwnedFd, component: &str) -> Result<NameState, Failure> {
        match rustix::fs::statat(parent, component, AtFlags::SYMLINK_NOFOLLOW) {
            Err(Errno::NOENT) => Ok(NameState::Absent),
            Err(_unreadable) => Err(Failure::Disk),
            Ok(_resolved_under_another_spelling) => Ok(NameState::Refused),
        }
    }

    /// Appends the regular files below the declared path `relative` to
    /// `files` and streams their contents. `Ok(false)` refuses: a symbolic
    /// link, a special file, a `.git` entry, a name that is not UTF-8, a
    /// component replaced while the walk runs, or more than the compared
    /// limits. A missing path holds no file. The deadline passing is a
    /// [`Failure::Deadline`].
    fn collect_declared(&mut self, relative: &str) -> Result<bool, Failure> {
        self.check_deadline()?;
        let parent = match self.exact_names(relative)? {
            NameState::Exact(parent) => parent,
            NameState::Absent => return Ok(true),
            NameState::Refused => return Ok(false),
        };
        let name = relative.rsplit('/').next().unwrap_or(relative);
        self.collect(parent.as_fd(), name, relative, 0)
    }

    /// Visits `name` below the directory `parent`; `relative` is its path from
    /// the launch directory, `name` included.
    fn collect(
        &mut self,
        parent: BorrowedFd<'_>,
        name: &str,
        relative: &str,
        depth: usize,
    ) -> Result<bool, Failure> {
        if depth > MAX_COMPARED_DEPTH {
            return Ok(false);
        }
        self.check_deadline()?;
        self.entries += 1;
        if self.entries > MAX_WALKED_ENTRIES {
            return Ok(false);
        }
        let kind = match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => FileType::from_raw_mode(stat.st_mode),
            Err(Errno::NOENT) => return Ok(true),
            Err(_unreadable) => return Err(Failure::Disk),
        };
        self.observe(CheckoutReadStep::BeforeOpen, relative);
        match kind {
            FileType::RegularFile => self.collect_file(parent, name, relative),
            FileType::Directory => self.collect_directory(parent, name, relative, depth),
            _ => Ok(false),
        }
    }

    fn collect_directory(
        &mut self,
        parent: BorrowedFd<'_>,
        name: &str,
        relative: &str,
        depth: usize,
    ) -> Result<bool, Failure> {
        let directory = match rustix::fs::openat(parent, name, directory_flags(), Mode::empty()) {
            Ok(directory) => directory,
            Err(errno) if changed_under_us(errno) => return Ok(false),
            Err(_unreadable) => return Err(Failure::Disk),
        };
        let entries = Dir::read_from(&directory).map_err(|_unreadable| Failure::Disk)?;
        for entry in entries {
            let entry = entry.map_err(|_unreadable| Failure::Disk)?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let Ok(child) = std::str::from_utf8(bytes) else {
                return Ok(false);
            };
            if child.eq_ignore_ascii_case(GIT_SUFFIX) {
                return Ok(false);
            }
            let child_relative = format!("{relative}/{child}");
            if !self.collect(directory.as_fd(), child, &child_relative, depth + 1)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Opens, checks and reads one regular file. The checks and the read use
    /// the one descriptor, so they describe the object that was opened and not
    /// whatever the name designates later.
    fn collect_file(
        &mut self,
        parent: BorrowedFd<'_>,
        name: &str,
        relative: &str,
    ) -> Result<bool, Failure> {
        use std::os::unix::fs::PermissionsExt as _;

        if self.files.len() >= MAX_COMPARED_FILES {
            return Ok(false);
        }
        let handle = match rustix::fs::openat(parent, name, file_flags(), Mode::empty()) {
            Ok(handle) => std::fs::File::from(handle),
            Err(errno) if changed_under_us(errno) => return Ok(false),
            Err(_unreadable) => return Err(Failure::Disk),
        };
        let metadata = handle.metadata().map_err(|_unreadable| Failure::Disk)?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_COMPARED_FILE_BYTES {
            return Ok(false);
        }
        self.observe(CheckoutReadStep::BeforeRead, relative);
        let Some(bytes) = self.read_bounded(&handle)? else {
            return Ok(false);
        };
        self.bytes += bytes.len() as u64;
        if self.bytes > MAX_COMPARED_TOTAL_BYTES {
            return Ok(false);
        }
        self.stream.push(&bytes)?;
        self.files.push(DiskFile {
            relative: relative.to_owned(),
            executable: metadata.permissions().mode() & EXECUTE_BITS != 0,
        });
        Ok(true)
    }

    /// The contents of `file`, or `None` when it holds more than
    /// [`MAX_COMPARED_FILE_BYTES`]. The read stops one byte past the bound, so
    /// a file that grows after its size was checked cannot extend it.
    fn read_bounded(&self, file: &std::fs::File) -> Result<Option<Vec<u8>>, Failure> {
        let mut limited = file.take(MAX_COMPARED_FILE_BYTES + 1);
        let mut contents = Vec::new();
        let mut chunk = vec![0_u8; READ_CHUNK_BYTES];
        loop {
            self.check_deadline()?;
            match limited.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => contents.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_unreadable) => return Err(Failure::Disk),
            }
        }
        Ok((contents.len() as u64 <= MAX_COMPARED_FILE_BYTES).then_some(contents))
    }
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
    private: tempfile::TempDir,
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
            private,
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

    /// A fresh stream that collects the blobs `hash_blobs` hashes, in a file
    /// of the private directory.
    fn blob_stream(&self) -> Result<BlobStream, Failure> {
        let file =
            tempfile::tempfile_in(self.private.path()).map_err(|_unwritable| Failure::Disk)?;
        Ok(BlobStream {
            writer: std::io::BufWriter::new(file),
            blobs: 0,
        })
    }

    /// The object ids git gives the streamed contents as blobs, without
    /// filters, in stream order. One `fast-import` run hashes them all in the
    /// private repository's object format.
    fn hash_blobs(&self, stream: BlobStream) -> Result<Vec<String>, Failure> {
        let BlobStream { writer, blobs } = stream;
        if blobs == 0 {
            return Ok(Vec::new());
        }
        let mut input = writer.into_inner().map_err(|_unwritable| Failure::Disk)?;
        input.rewind().map_err(|_unreadable| Failure::Disk)?;
        let marks = self.private.path().join(MARKS_FILE);
        let marks_arg = format!("--export-marks={}", marks.to_str().ok_or(Failure::Disk)?);
        let mut command_args = self.repository_args.clone();
        command_args.extend(["fast-import".to_owned(), "--quiet".to_owned(), marks_arg]);
        let imported = self.execute_with_stdin(&command_args, &self.env, Some(&input))?;
        imported.success_stdout()?;
        let exported = std::fs::read_to_string(&marks).map_err(|_unreadable| Failure::Output)?;
        let mut hashes = vec![None; blobs];
        for line in exported.lines() {
            let (mark, object) = line.split_once(' ').ok_or(Failure::Output)?;
            let index = mark
                .strip_prefix(':')
                .and_then(|number| number.parse::<usize>().ok())
                .and_then(|number| number.checked_sub(1))
                .ok_or(Failure::Output)?;
            let slot = hashes.get_mut(index).ok_or(Failure::Output)?;
            if slot.is_some() || !is_object_id(object) {
                return Err(Failure::Output);
            }
            *slot = Some(object.to_owned());
        }
        hashes
            .into_iter()
            .map(|hash| hash.ok_or(Failure::Output))
            .collect()
    }

    fn execute(&self, args: &[String], env: &[(String, String)]) -> Result<GitResult, Failure> {
        self.execute_with_stdin(args, env, None)
    }

    fn execute_with_stdin(
        &self,
        args: &[String],
        env: &[(String, String)],
        stdin: Option<&std::fs::File>,
    ) -> Result<GitResult, Failure> {
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
            stdin,
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

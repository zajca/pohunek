//! Pinned, hash-verified staging of the upstream runtime releases.
//!
//! `compat stage-upstream` installs the release a runtime's
//! `compat/<runtime>/compatibility-lock.json` pins into `<out>/<runtime>/`
//! with network access, from the committed npm lockfile
//! `compat/<runtime>/npm/package-lock.json` (every dependency carries an
//! integrity, so `npm ci` refuses any other bytes). The result is an npm
//! prefix: `bin/<binary>` and `lib/node_modules/...`, plus two manifests,
//! `STAGE.sha256` (`sha256sum -c` format, one line per regular file) and
//! `STAGE.links` (symbolic links and executable bits), that pin every byte.
//! `compat verify-stage` recomputes both from disk without network access.
//!
//! The command is data driven: it knows no runtime by name. A lock that does
//! not describe an npm release (another `schema`, no `upstream.npm`) is
//! refused instead of guessed at.

// Rust guideline compliant 2026-10-08

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, LazyLock, Mutex};
use std::thread;
use std::time::Duration;

use clap::Subcommand;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::XtaskError;

/// Lock schema this module understands.
const LOCK_SCHEMA: u32 = 1;

/// Name of the `sha256sum -c` manifest at the stage root.
const SHA256_MANIFEST: &str = "STAGE.sha256";

/// Name of the symbolic-link and executable-bit manifest at the stage root.
const LINKS_MANIFEST: &str = "STAGE.links";

/// Directory of the npm project (manifest, lockfile, `node_modules`) inside a
/// stage; npm's global layout keeps packages in `lib/node_modules`.
const PROJECT_DIR: &str = "lib";

/// Directory of the executable links inside a stage.
const BIN_DIR: &str = "bin";

/// Directory below `compat/<runtime>/` that holds the committed npm project.
const NPM_DIR: &str = "npm";

/// Largest accepted JSON input (lock, npm manifest, npm lockfile). The Pi
/// lockfile is about 70 KiB; the cap only bounds a hostile file.
const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

/// Wall-clock limit of `npm ci`. Pi installs about 150 packages; a run that
/// takes a quarter of an hour is hung on the network, not slow.
const INSTALL_TIMEOUT: Duration = Duration::from_mins(15);

/// Wall-clock limit of the `--version` probe, which only prints a banner.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Probe output kept for comparison; a banner is a few dozen bytes.
const PROBE_OUTPUT_LIMIT: u64 = 4096;

/// Characters of an unexpected probe banner quoted in the error.
const QUOTED_BANNER_CHARS: usize = 200;

/// Read buffer for hashing files, which can be hundreds of MiB.
const HASH_CHUNK_BYTES: usize = 64 * 1024;

/// Mode of the written manifests: public documents.
const MANIFEST_MODE: u32 = 0o644;

/// Owner execute bit. `[ -x file ]` in the POSIX verifier tests the same bit
/// for the file's owner.
const OWNER_EXEC: u32 = 0o100;

/// Placeholder of the release in `upstream.version_output`.
const RELEASE_PLACEHOLDER: &str = "{release}";

/// POSIX `sh` verifier of a staged tree, run as `sh -c "$SCRIPT" sh <stage>`.
///
/// It needs only `sha256sum`, `find`, `sed`, `sort`, `grep` and `readlink`
/// (coreutils or busybox), so the network-isolated namespace can run it with
/// no Rust tooling. It refuses a modified, added or removed file, a file
/// replaced by a link, a retargeted link and a changed executable bit.
/// `docs/knowledge/concepts/upstream-staging.md` quotes it verbatim; a test
/// keeps the two equal and runs it against drifted stages.
#[cfg(test)]
pub(crate) const POSIX_VERIFY: &str = r#"set -eu
cd "$1"
sha256sum -c STAGE.sha256 >/dev/null
tab=$(printf '\t')
files=$(sed 's/^[0-9a-f]\{64\}  //' STAGE.sha256)
links=$(while IFS="$tab" read -r kind path target; do
  if [ "$kind" = link ]; then printf '%s\n' "$path"; fi
done < STAGE.links)
execs=$(while IFS="$tab" read -r kind path target; do
  if [ "$kind" = exec ]; then printf '%s\n' "$path"; fi
done < STAGE.links)
expected=$({ printf '%s\n' "$files"; printf '%s\n' "$links"; } | sed '/^$/d' | LC_ALL=C sort)
actual=$(find . -path ./STAGE.sha256 -prune -o -path ./STAGE.links -prune -o ! -type d -print | sed 's|^\./||' | LC_ALL=C sort)
[ "$expected" = "$actual" ]
printf '%s\n' "$files" | while IFS= read -r path; do
  [ -f "$path" ] && [ ! -h "$path" ] || exit 1
  if printf '%s\n' "$execs" | grep -Fxq -- "$path"; then
    [ -x "$path" ] || exit 1
  else
    [ ! -x "$path" ] || exit 1
  fi
done
while IFS="$tab" read -r kind path target; do
  case $kind in
    link) [ -h "$path" ] && [ "$(readlink "$path")" = "$target" ] || exit 1 ;;
    exec) ;;
    *) exit 1 ;;
  esac
done < STAGE.links
"#;

/// Why an input or a staged path was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    /// A required value, file or tool is absent.
    Missing,
    /// The value or file could not be parsed or has the wrong shape.
    Malformed,
    /// The lock or lockfile describes something this command does not stage.
    Unsupported,
    /// The value differs from the pinned one.
    Mismatch,
    /// The file's bytes differ from the manifest.
    Modified,
    /// The path is on disk but not in the manifest.
    Added,
    /// The path is in the manifest but not on disk.
    Removed,
    /// A symbolic link points somewhere else than the manifest says.
    Retargeted,
    /// The executable bit differs from the manifest.
    ModeChanged,
    /// The path is another kind of entry than the manifest says.
    TypeChanged,
    /// The path holds a character the manifests cannot represent.
    UnsafeName,
    /// A symbolic link is absolute or leaves the stage.
    EscapingLink,
    /// The entry is neither a regular file, a directory nor a symbolic link.
    SpecialFile,
    /// The destination already holds a stage.
    AlreadyExists,
    /// The platform has no pinned digest in the lock.
    Unpinned,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "is missing",
            Self::Malformed => "is malformed",
            Self::Unsupported => "is not a shape this command stages",
            Self::Mismatch => "does not match the pinned value",
            Self::Modified => "was modified",
            Self::Added => "was added",
            Self::Removed => "was removed",
            Self::Retargeted => "points elsewhere than recorded",
            Self::ModeChanged => "changed its executable bit",
            Self::TypeChanged => "is another kind of entry than recorded",
            Self::UnsafeName => "has a name the manifests cannot represent",
            Self::EscapingLink => "is a link that is absolute or leaves the stage",
            Self::SpecialFile => "is not a regular file, directory or symbolic link",
            Self::AlreadyExists => "already exists and is not empty",
            Self::Unpinned => "has no pinned digest for this platform",
        })
    }
}

/// How a delegated command ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Exited with this code, or was killed by a signal when `None`.
    Exit(Option<i32>),
    /// Killed after its wall-clock limit.
    TimedOut,
}

/// A refused stage input or staged tree. Names the subject and never echoes
/// file content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StageError {
    /// An input, path or tool was refused; `more` further problems were found
    /// behind the first.
    Refused {
        /// Lock field, input file or staged path.
        subject: String,
        /// Why it was refused.
        fault: Fault,
        /// Number of additional refused paths not listed.
        more: usize,
    },
    /// A delegated command failed.
    Command {
        /// What the command was for.
        step: &'static str,
        /// How it ended.
        outcome: Outcome,
    },
    /// The upstream binary printed a banner other than the locked release's.
    Banner {
        /// The banner the lock requires.
        expected: String,
        /// The banner printed, escaped and truncated.
        actual: String,
    },
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused {
                subject,
                fault,
                more,
            } => {
                write!(f, "`{subject}` {fault}")?;
                if *more > 0 {
                    write!(f, " (and {more} more)")?;
                }
                Ok(())
            }
            Self::Command { step, outcome } => match outcome {
                Outcome::Exit(Some(code)) => write!(f, "{step} exited with code {code}"),
                Outcome::Exit(None) => write!(f, "{step} was killed by a signal"),
                Outcome::TimedOut => write!(f, "{step} exceeded its time limit"),
            },
            Self::Banner { expected, actual } => write!(
                f,
                "the upstream binary printed {actual}, the lock requires {expected:?}"
            ),
        }
    }
}

impl std::error::Error for StageError {}

fn refuse(subject: impl Into<String>, fault: Fault) -> XtaskError {
    XtaskError::UpstreamStage(StageError::Refused {
        subject: subject.into(),
        fault,
        more: 0,
    })
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> XtaskError {
    let path = path.to_path_buf();
    move |source| XtaskError::Io { path, source }
}

/// `compat` actions that stage and verify upstream releases.
#[derive(Debug, Subcommand)]
pub(crate) enum UpstreamAction {
    /// Install a runtime's locked upstream release into `<out>/<runtime>/`.
    ///
    /// Needs network access. Installs from `compat/<runtime>/npm` with
    /// `npm ci`, checks the release, the root tarball integrity, a pinned
    /// native digest and the `--version` banner against the lock, then writes
    /// `STAGE.sha256` and `STAGE.links`. Refuses a non-empty destination.
    StageUpstream {
        /// Runtime id, a directory name below `compat/`.
        #[arg(long)]
        runtime: String,
        /// Directory that receives `<runtime>/`.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// `npm` executable; defaults to the first `npm` on `PATH`.
        #[arg(long, value_name = "FILE")]
        npm: Option<PathBuf>,
        /// Repository root; defaults to the checkout this xtask belongs to.
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
    },
    /// Recompute a staged tree from disk and compare it with its manifests.
    ///
    /// Offline. Refuses any added, removed or modified file, retargeted link
    /// or changed executable bit, a stage whose npm project differs from the
    /// committed one, and an unpinned installed release.
    VerifyStage {
        /// Runtime id, a directory name below `compat/`.
        #[arg(long)]
        runtime: String,
        /// Directory that holds `<runtime>/`.
        #[arg(long, value_name = "DIR")]
        dir: PathBuf,
        /// Repository root; defaults to the checkout this xtask belongs to.
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
    },
}

/// Runs one staging action; `default_root` is this checkout's root.
pub(crate) fn run(action: UpstreamAction, default_root: &Path) -> Result<(), XtaskError> {
    match action {
        UpstreamAction::StageUpstream {
            runtime,
            out,
            npm,
            root,
        } => {
            let root = root.unwrap_or_else(|| default_root.to_path_buf());
            let path = std::env::var_os("PATH").ok_or_else(|| refuse("PATH", Fault::Missing))?;
            let npm = match npm {
                Some(npm) => npm,
                None => find_in_path("npm", &path)?,
            };
            let summary = stage_upstream(&root, &runtime, &out, &Tools { npm, path })?;
            println!(
                "compat stage-upstream ok: {runtime} {} ({} files, {} links) -> {}",
                summary.release,
                summary.files,
                summary.links,
                summary.stage.display()
            );
        }
        UpstreamAction::VerifyStage { runtime, dir, root } => {
            let root = root.unwrap_or_else(|| default_root.to_path_buf());
            let summary = verify_stage(&root, &runtime, &dir)?;
            println!(
                "compat verify-stage ok: {runtime} {} ({} files, {} links)",
                summary.release, summary.files, summary.links
            );
        }
    }
    Ok(())
}

/// Host tools the staging shells out to.
#[derive(Debug)]
pub(crate) struct Tools {
    /// The `npm` executable.
    pub(crate) npm: PathBuf,
    /// `PATH` handed to npm and to the probe: it must reach `node`.
    pub(crate) path: OsString,
}

/// What a stage or a verification covered.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct StageSummary {
    /// Locked upstream release.
    pub(crate) release: String,
    /// Regular files recorded.
    pub(crate) files: usize,
    /// Symbolic links recorded.
    pub(crate) links: usize,
    /// The stage directory.
    pub(crate) stage: PathBuf,
}

fn find_in_path(program: &str, path: &OsStr) -> Result<PathBuf, XtaskError> {
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| refuse(program, Fault::Missing))
}

// ---------------------------------------------------------------------------
// Lock and committed npm project
// ---------------------------------------------------------------------------

/// The fields of a runtime lock this module reads; the rest belongs to the
/// runtime's own checks.
#[derive(Debug, Deserialize)]
struct RawLock {
    schema: Option<Value>,
    runtime: Option<Value>,
    upstream: Option<RawUpstream>,
}

#[derive(Debug, Deserialize)]
struct RawUpstream {
    npm: Option<String>,
    release: Option<String>,
    integrity: Option<String>,
    binary: Option<String>,
    version_output: Option<String>,
    scripts: Option<bool>,
    sha256: Option<BTreeMap<String, String>>,
}

/// A validated runtime lock.
#[derive(Debug)]
pub(crate) struct Lock {
    npm: String,
    release: String,
    integrity: String,
    binary: String,
    banner: String,
    scripts: bool,
    sha256: Option<BTreeMap<String, String>>,
}

/// The committed npm project of a runtime, validated against its lock.
#[derive(Debug)]
pub(crate) struct Project {
    manifest: Vec<u8>,
    lockfile: Vec<u8>,
    /// `https://<host>/`, the one origin every pinned tarball comes from.
    registry: String,
    /// Path of the executable inside the npm package, from the lockfile.
    bin_path: String,
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, XtaskError> {
    let file = File::open(path).map_err(io_error(path))?;
    let mut bytes = Vec::new();
    file.take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error(path))?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(refuse(path.display().to_string(), Fault::Malformed));
    }
    Ok(bytes)
}

fn read_json(path: &Path, subject: &str) -> Result<(Value, Vec<u8>), XtaskError> {
    let bytes = match read_bounded(path) {
        Err(XtaskError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Err(refuse(subject, Fault::Missing));
        }
        other => other?,
    };
    let value =
        serde_json::from_slice(&bytes).map_err(|_cause| refuse(subject, Fault::Malformed))?;
    Ok((value, bytes))
}

fn check_runtime_name(runtime: &str) -> Result<(), XtaskError> {
    let valid = !runtime.is_empty()
        && runtime
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(refuse("runtime", Fault::Malformed))
    }
}

/// npm `integrity` of a tarball: `sha512-` and the base64 of 64 bytes. The
/// last sextet carries four padding bits, so only `A`, `Q`, `g` and `w` can
/// precede the `==`.
static SHA512_SRI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^sha512-[A-Za-z0-9+/]{85}[AQgw]==$").expect("the SRI pattern is valid")
});

fn is_sha512_sri(text: &str) -> bool {
    SHA512_SRI.is_match(text)
}

fn is_hex_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn read_lock(root: &Path, runtime: &str) -> Result<Lock, XtaskError> {
    check_runtime_name(runtime)?;
    let path = crate::attestation::lock_path(root, runtime);
    let (value, _bytes) = read_json(&path, "lock")?;
    let raw: RawLock =
        serde_json::from_value(value).map_err(|_cause| refuse("lock", Fault::Malformed))?;
    if raw.schema.as_ref().and_then(Value::as_u64) != Some(u64::from(LOCK_SCHEMA)) {
        return Err(refuse("lock.schema", Fault::Unsupported));
    }
    if raw.runtime.as_ref().and_then(Value::as_str) != Some(runtime) {
        return Err(refuse("lock.runtime", Fault::Mismatch));
    }
    let upstream = raw
        .upstream
        .ok_or_else(|| refuse("lock.upstream", Fault::Missing))?;
    let npm = upstream
        .npm
        .ok_or_else(|| refuse("upstream.npm", Fault::Unsupported))?;
    let required = |value: Option<String>, name: &str| {
        value.ok_or_else(|| refuse(format!("upstream.{name}"), Fault::Missing))
    };
    let release = required(upstream.release, "release")?;
    let integrity = required(upstream.integrity, "integrity")?;
    let binary = required(upstream.binary, "binary")?;
    let template = required(upstream.version_output, "version_output")?;
    if !is_sha512_sri(&integrity) {
        return Err(refuse("upstream.integrity", Fault::Malformed));
    }
    if semver::Version::parse(&release).is_err() {
        return Err(refuse("upstream.release", Fault::Malformed));
    }
    if !safe_component(&binary) {
        return Err(refuse("upstream.binary", Fault::Malformed));
    }
    if template.matches(RELEASE_PLACEHOLDER).count() != 1 {
        return Err(refuse("upstream.version_output", Fault::Malformed));
    }
    if let Some(pins) = &upstream.sha256 {
        if pins.values().any(|digest| !is_hex_digest(digest)) {
            return Err(refuse("upstream.sha256", Fault::Malformed));
        }
    }
    Ok(Lock {
        banner: template.replace(RELEASE_PLACEHOLDER, &release),
        npm,
        release,
        integrity,
        binary,
        scripts: upstream.scripts.unwrap_or(false),
        sha256: upstream.sha256,
    })
}

fn exact_dependency(section: Option<&Value>, lock: &Lock) -> bool {
    section.and_then(Value::as_object).is_some_and(|map| {
        map.len() == 1 && map.get(&lock.npm).and_then(Value::as_str) == Some(&lock.release)
    })
}

fn no_other_dependency_kinds(value: &Value) -> bool {
    [
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ]
    .iter()
    .all(|key| value.get(key).is_none())
}

/// `https://host/` of a `resolved` URL, refusing credentials and non-https.
fn origin_of(resolved: &str) -> Option<String> {
    let rest = resolved.strip_prefix("https://")?;
    let host = rest.split('/').next()?;
    let clean = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    clean.then(|| format!("https://{host}/"))
}

pub(crate) fn read_project(root: &Path, runtime: &str, lock: &Lock) -> Result<Project, XtaskError> {
    let dir = root.join("compat").join(runtime).join(NPM_DIR);
    let (manifest_json, manifest) = read_json(&dir.join("package.json"), "npm/package.json")?;
    let (lock_json, lockfile) = read_json(&dir.join("package-lock.json"), "npm/package-lock.json")?;

    if !exact_dependency(manifest_json.get("dependencies"), lock)
        || !no_other_dependency_kinds(&manifest_json)
    {
        return Err(refuse("npm/package.json", Fault::Mismatch));
    }
    // npm 12 runs a dependency's install script only when package.json
    // allows exactly that release; older npm ignores the field.
    let allowed: BTreeSet<&str> = manifest_json
        .get("allowScripts")
        .and_then(Value::as_object)
        .map(|map| map.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let wanted = format!("{}@{}", lock.npm, lock.release);
    let expected_allowed: BTreeSet<&str> = if lock.scripts {
        BTreeSet::from([wanted.as_str()])
    } else {
        BTreeSet::new()
    };
    if allowed != expected_allowed {
        return Err(refuse("npm/package.json allowScripts", Fault::Mismatch));
    }

    if lock_json.get("lockfileVersion").and_then(Value::as_u64) != Some(3) {
        return Err(refuse(
            "npm/package-lock.json lockfileVersion",
            Fault::Unsupported,
        ));
    }
    let packages = lock_json
        .get("packages")
        .and_then(Value::as_object)
        .ok_or_else(|| refuse("npm/package-lock.json packages", Fault::Malformed))?;
    let root_entry = packages
        .get("")
        .ok_or_else(|| refuse("npm/package-lock.json root", Fault::Missing))?;
    if !exact_dependency(root_entry.get("dependencies"), lock)
        || !no_other_dependency_kinds(root_entry)
    {
        return Err(refuse("npm/package-lock.json root", Fault::Mismatch));
    }

    let mut origins = BTreeSet::new();
    let mut script_packages = BTreeSet::new();
    for (key, entry) in packages.iter().filter(|(key, _)| !key.is_empty()) {
        let subject = format!("npm/package-lock.json {key}");
        let resolved = entry.get("resolved").and_then(Value::as_str);
        let integrity = entry.get("integrity").and_then(Value::as_str);
        let (Some(resolved), Some(integrity)) = (resolved, integrity) else {
            return Err(refuse(subject, Fault::Missing));
        };
        if !key.starts_with("node_modules/") || entry.get("link").is_some() {
            return Err(refuse(subject, Fault::Unsupported));
        }
        let Some(origin) = origin_of(resolved) else {
            return Err(refuse(subject, Fault::Unsupported));
        };
        if !is_sha512_sri(integrity) {
            return Err(refuse(subject, Fault::Malformed));
        }
        origins.insert(origin);
        if entry.get("hasInstallScript").and_then(Value::as_bool) == Some(true) {
            script_packages.insert(key.as_str());
        }
    }
    let registry = match origins.iter().collect::<Vec<_>>().as_slice() {
        [origin] => (*origin).clone(),
        [] => return Err(refuse("npm/package-lock.json packages", Fault::Missing)),
        _ => return Err(refuse("npm/package-lock.json resolved", Fault::Unsupported)),
    };

    let root_key = format!("node_modules/{}", lock.npm);
    let package = packages
        .get(&root_key)
        .ok_or_else(|| refuse(format!("npm/package-lock.json {root_key}"), Fault::Missing))?;
    if package.get("version").and_then(Value::as_str) != Some(&lock.release) {
        return Err(refuse("npm/package-lock.json release", Fault::Mismatch));
    }
    if package.get("integrity").and_then(Value::as_str) != Some(&lock.integrity) {
        return Err(refuse("upstream.integrity", Fault::Mismatch));
    }
    // With scripts enabled only the locked package may run one: a dependency
    // that gained an install script would execute unreviewed code.
    if lock.scripts && script_packages != BTreeSet::from([root_key.as_str()]) {
        return Err(refuse(
            "npm/package-lock.json hasInstallScript",
            Fault::Mismatch,
        ));
    }
    let bin_path = package
        .get("bin")
        .and_then(|bin| bin.get(&lock.binary))
        .and_then(Value::as_str)
        .map(|path| path.trim_start_matches("./").to_owned())
        .filter(|path| safe_relative(path))
        .ok_or_else(|| refuse("npm/package-lock.json bin", Fault::Missing))?;

    Ok(Project {
        manifest,
        lockfile,
        registry,
        bin_path,
    })
}

// ---------------------------------------------------------------------------
// Tree scan and manifests
// ---------------------------------------------------------------------------

/// One recorded path of a stage.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Entry {
    File { digest: String, exec: bool },
    Link { target: String },
}

type Tree = BTreeMap<String, Entry>;

/// One path component the manifests can carry.
fn safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.chars().any(|c| c.is_control() || c == '\\')
}

/// A relative path whose components all pass [`safe_component`].
fn safe_relative(path: &str) -> bool {
    !path.is_empty() && path.split('/').all(safe_component)
}

/// A link target is recorded verbatim, so only control characters and
/// backslashes are refused.
fn safe_target(target: &str) -> bool {
    !target.is_empty() && !target.chars().any(|c| c.is_control() || c == '\\')
}

/// Whether `target`, resolved lexically from the directory `depth` levels
/// below the stage root, stays inside the stage.
fn stays_inside(target: &Path, depth: usize) -> bool {
    let mut depth = depth;
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(rest) => depth = rest,
                None => return false,
            },
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// A path rendered with control characters escaped, for error messages.
fn escaped(path: &Path) -> String {
    path.display().to_string().escape_debug().to_string()
}

fn hash_file(path: &Path) -> Result<String, XtaskError> {
    let mut file = File::open(path).map_err(io_error(path))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_CHUNK_BYTES];
    loop {
        let read = file.read(&mut buffer).map_err(io_error(path))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Records every file and symbolic link below `stage`, except the two
/// manifests at its root.
fn scan_tree(stage: &Path) -> Result<Tree, XtaskError> {
    let mut tree = Tree::new();
    let mut pending = vec![(stage.to_path_buf(), String::new(), 0_usize)];
    while let Some((dir, prefix, depth)) = pending.pop() {
        let entries = fs::read_dir(&dir).map_err(io_error(&dir))?;
        for entry in entries {
            let entry = entry.map_err(io_error(&dir))?;
            let path = entry.path();
            let name = entry
                .file_name()
                .into_string()
                .ok()
                .filter(|name| safe_component(name))
                .ok_or_else(|| refuse(escaped(&path), Fault::UnsafeName))?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if depth == 0 && (relative == SHA256_MANIFEST || relative == LINKS_MANIFEST) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(io_error(&path))?;
            let kind = metadata.file_type();
            if kind.is_dir() {
                pending.push((path, relative, depth + 1));
            } else if kind.is_file() {
                let exec = metadata.permissions().mode() & OWNER_EXEC != 0;
                let digest = hash_file(&path)?;
                tree.insert(relative, Entry::File { digest, exec });
            } else if kind.is_symlink() {
                let target = fs::read_link(&path).map_err(io_error(&path))?;
                if !stays_inside(&target, depth) {
                    return Err(refuse(relative, Fault::EscapingLink));
                }
                let target = target
                    .into_os_string()
                    .into_string()
                    .ok()
                    .filter(|target| safe_target(target))
                    .ok_or_else(|| refuse(relative.clone(), Fault::UnsafeName))?;
                tree.insert(relative, Entry::Link { target });
            } else {
                return Err(refuse(relative, Fault::SpecialFile));
            }
        }
    }
    Ok(tree)
}

fn render_manifests(tree: &Tree) -> (String, String) {
    let mut sha256 = String::new();
    let mut links = String::new();
    for (path, entry) in tree {
        match entry {
            Entry::File { digest, exec } => {
                sha256.push_str(&[digest, "  ", path, "\n"].concat());
                if *exec {
                    links.push_str(&["exec\t", path, "\n"].concat());
                }
            }
            Entry::Link { target } => {
                links.push_str(&["link\t", path, "\t", target, "\n"].concat());
            }
        }
    }
    (sha256, links)
}

fn write_manifest(path: &Path, content: &str) -> Result<(), XtaskError> {
    let mut file = File::create(path).map_err(io_error(path))?;
    file.write_all(content.as_bytes()).map_err(io_error(path))?;
    fs::set_permissions(path, fs::Permissions::from_mode(MANIFEST_MODE)).map_err(io_error(path))
}

fn read_manifest(path: &Path, subject: &str) -> Result<String, XtaskError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(refuse(subject, Fault::Missing))
        }
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            Err(refuse(subject, Fault::Malformed))
        }
        Err(source) => Err(XtaskError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Parses the two manifests of `stage`; a duplicate path, an unsafe name or
/// a line of another shape is malformed.
fn parse_manifests(stage: &Path) -> Result<Tree, XtaskError> {
    let malformed = |subject: &str| refuse(subject, Fault::Malformed);
    let mut tree = Tree::new();
    let sha256 = read_manifest(&stage.join(SHA256_MANIFEST), SHA256_MANIFEST)?;
    for line in sha256.split_inclusive('\n') {
        let line = line
            .strip_suffix('\n')
            .ok_or_else(|| malformed(SHA256_MANIFEST))?;
        let (digest, path) = line
            .split_once("  ")
            .filter(|(digest, path)| is_hex_digest(digest) && safe_relative(path))
            .ok_or_else(|| malformed(SHA256_MANIFEST))?;
        let entry = Entry::File {
            digest: digest.to_owned(),
            exec: false,
        };
        if tree.insert(path.to_owned(), entry).is_some() {
            return Err(malformed(SHA256_MANIFEST));
        }
    }
    let links = read_manifest(&stage.join(LINKS_MANIFEST), LINKS_MANIFEST)?;
    for line in links.split_inclusive('\n') {
        let line = line
            .strip_suffix('\n')
            .ok_or_else(|| malformed(LINKS_MANIFEST))?;
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["exec", path] => match tree.get_mut(*path) {
                Some(Entry::File { exec, .. }) if !*exec => *exec = true,
                _ => return Err(malformed(LINKS_MANIFEST)),
            },
            ["link", path, target] if safe_relative(path) && safe_target(target) => {
                let entry = Entry::Link {
                    target: (*target).to_owned(),
                };
                if tree.insert((*path).to_owned(), entry).is_some() {
                    return Err(malformed(LINKS_MANIFEST));
                }
            }
            _ => return Err(malformed(LINKS_MANIFEST)),
        }
    }
    Ok(tree)
}

/// The refused paths of `disk` against `recorded`, in path order.
fn differences(recorded: &Tree, disk: &Tree) -> Vec<(String, Fault)> {
    let mut found = Vec::new();
    for path in recorded.keys().chain(disk.keys()).collect::<BTreeSet<_>>() {
        let fault = match (recorded.get(path), disk.get(path)) {
            (Some(_), None) => Some(Fault::Removed),
            (None, Some(_)) => Some(Fault::Added),
            (
                Some(Entry::File {
                    digest: want,
                    exec: want_exec,
                }),
                Some(Entry::File { digest, exec }),
            ) => {
                if want != digest {
                    Some(Fault::Modified)
                } else if want_exec != exec {
                    Some(Fault::ModeChanged)
                } else {
                    None
                }
            }
            (Some(Entry::Link { target: want }), Some(Entry::Link { target })) => {
                (want != target).then_some(Fault::Retargeted)
            }
            (Some(_), Some(_)) => Some(Fault::TypeChanged),
            (None, None) => None,
        };
        if let Some(fault) = fault {
            found.push((path.clone(), fault));
        }
    }
    found
}

/// Compares the stage on disk with its manifests.
fn check_manifests(stage: &Path) -> Result<Tree, XtaskError> {
    let recorded = parse_manifests(stage)?;
    let disk = scan_tree(stage)?;
    let found = differences(&recorded, &disk);
    match found.split_first() {
        None => Ok(disk),
        Some(((path, fault), rest)) => Err(XtaskError::UpstreamStage(StageError::Refused {
            subject: path.clone(),
            fault: *fault,
            more: rest.len(),
        })),
    }
}

// ---------------------------------------------------------------------------
// Pins
// ---------------------------------------------------------------------------

/// The `upstream.sha256` key of this host, in the `<os>-<arch>` spelling the
/// npm platform packages use.
pub(crate) fn host_key() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("macos", "aarch64") => Some("darwin-arm64"),
        _ => None,
    }
}

fn json_file(path: &Path, subject: &str) -> Result<Value, XtaskError> {
    read_json(path, subject).map(|(value, _bytes)| value)
}

/// Checks a stage's bytes against the lock and the committed npm project.
fn check_pins(stage: &Path, lock: &Lock, project: &Project) -> Result<(), XtaskError> {
    let lib = stage.join(PROJECT_DIR);
    for (name, committed) in [
        ("package.json", &project.manifest),
        ("package-lock.json", &project.lockfile),
    ] {
        let bytes = read_bounded(&lib.join(name)).map_err(|error| match error {
            XtaskError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                refuse(format!("{PROJECT_DIR}/{name}"), Fault::Missing)
            }
            other => other,
        })?;
        if &bytes != committed {
            return Err(refuse(format!("{PROJECT_DIR}/{name}"), Fault::Mismatch));
        }
    }

    let package_dir = lib.join("node_modules").join(&lock.npm);
    let installed = json_file(&package_dir.join("package.json"), "installed package.json")?;
    if installed.get("version").and_then(Value::as_str) != Some(&lock.release) {
        return Err(refuse("installed release", Fault::Mismatch));
    }
    let hidden = json_file(
        &lib.join("node_modules").join(".package-lock.json"),
        "installed lockfile",
    )?;
    let root_key = format!("node_modules/{}", lock.npm);
    let entry = hidden
        .get("packages")
        .and_then(|packages| packages.get(&root_key));
    if entry.and_then(|e| e.get("version")).and_then(Value::as_str) != Some(&lock.release)
        || entry
            .and_then(|e| e.get("integrity"))
            .and_then(Value::as_str)
            != Some(&lock.integrity)
    {
        return Err(refuse("installed integrity", Fault::Mismatch));
    }

    let bin = stage.join(BIN_DIR).join(&lock.binary);
    let expected_target = format!(
        "../{PROJECT_DIR}/node_modules/{}/{}",
        lock.npm, project.bin_path
    );
    let target = fs::read_link(&bin)
        .map_err(|_cause| refuse(format!("{BIN_DIR}/{}", lock.binary), Fault::Missing))?;
    if target != Path::new(&expected_target) {
        return Err(refuse(
            format!("{BIN_DIR}/{}", lock.binary),
            Fault::Retargeted,
        ));
    }

    if let Some(pins) = &lock.sha256 {
        let key = host_key().ok_or_else(|| refuse("upstream.sha256", Fault::Unpinned))?;
        let pinned = pins
            .get(key)
            .ok_or_else(|| refuse(format!("upstream.sha256.{key}"), Fault::Unpinned))?;
        let resolved = fs::canonicalize(&bin).map_err(io_error(&bin))?;
        let inside = fs::canonicalize(stage).map_err(io_error(stage))?;
        if !resolved.starts_with(&inside) {
            return Err(refuse(
                format!("{BIN_DIR}/{}", lock.binary),
                Fault::EscapingLink,
            ));
        }
        if &hash_file(&resolved)? != pinned {
            return Err(refuse(format!("upstream.sha256.{key}"), Fault::Mismatch));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// verify-stage
// ---------------------------------------------------------------------------

fn summarize(lock: &Lock, tree: &Tree, stage: &Path) -> StageSummary {
    let links = tree
        .values()
        .filter(|entry| matches!(entry, Entry::Link { .. }))
        .count();
    StageSummary {
        release: lock.release.clone(),
        files: tree.len() - links,
        links,
        stage: stage.to_path_buf(),
    }
}

/// Recomputes the stage at `<dir>/<runtime>` and compares it with its
/// manifests, the committed npm project and the lock's pins. Offline.
pub(crate) fn verify_stage(
    root: &Path,
    runtime: &str,
    dir: &Path,
) -> Result<StageSummary, XtaskError> {
    let lock = read_lock(root, runtime)?;
    let project = read_project(root, runtime, &lock)?;
    let stage = dir.join(runtime);
    match fs::symlink_metadata(&stage) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(refuse(runtime, Fault::TypeChanged)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(refuse(runtime, Fault::Missing));
        }
        Err(source) => return Err(io_error(&stage)(source)),
    }
    let tree = check_manifests(&stage)?;
    check_pins(&stage, &lock, &project)?;
    Ok(summarize(&lock, &tree, &stage))
}

// ---------------------------------------------------------------------------
// stage-upstream
// ---------------------------------------------------------------------------

/// Removes the scratch directory when staging ends, on every path.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("warning: could not remove `{}`: {error}", self.0.display());
            }
        }
    }
}

/// How a bounded child ended and what it printed.
/// Runs `command` in its own process group and kills the group after
/// `limit`. With `capture`, standard output is kept up to `output_limit`
/// bytes and the rest is discarded.
fn run_bounded(
    command: &mut Command,
    limit: Duration,
    capture: bool,
    output_limit: u64,
    step: &'static str,
) -> Result<Vec<u8>, XtaskError> {
    command
        .stdin(Stdio::null())
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .process_group(0);
    let mut child = command.spawn().map_err(|source| XtaskError::Io {
        path: PathBuf::from(command.get_program()),
        source,
    })?;
    let reader = child.stdout.take().map(|mut stdout| {
        thread::spawn(move || {
            let mut kept = Vec::new();
            let read = (&mut stdout).take(output_limit).read_to_end(&mut kept);
            // Keep draining so the child never blocks on a full pipe.
            let drained = io::copy(&mut stdout, &mut io::sink());
            read.and(drained).map(|_| kept)
        })
    });

    let pid = i32::try_from(child.id()).map_err(|_cause| refuse(step, Fault::Malformed))?;
    let finished = Arc::new(Mutex::new(false));
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let watchdog = {
        let finished = Arc::clone(&finished);
        thread::spawn(move || {
            if stop_rx.recv_timeout(limit) == Err(mpsc::RecvTimeoutError::Timeout) {
                let done = finished.lock().map_or(true, |done| *done);
                if !done {
                    // The group may already be gone; the result is irrelevant.
                    killpg(Pid::from_raw(pid), Signal::SIGKILL).unwrap_or(());
                    return true;
                }
            }
            false
        })
    };
    let status = child.wait();
    if let Ok(mut done) = finished.lock() {
        *done = true;
    }
    drop(stop_tx);
    let timed_out = watchdog.join().unwrap_or(false);
    let stdout = match reader {
        Some(reader) => reader
            .join()
            .map_err(|_cause| refuse(step, Fault::Malformed))?
            .map_err(|source| XtaskError::Io {
                path: PathBuf::from(step),
                source,
            })?,
        None => Vec::new(),
    };
    let status = status.map_err(|source| XtaskError::Io {
        path: PathBuf::from(step),
        source,
    })?;
    let outcome = if timed_out {
        Outcome::TimedOut
    } else {
        Outcome::Exit(status.code())
    };
    if outcome != Outcome::Exit(Some(0)) {
        return Err(XtaskError::UpstreamStage(StageError::Command {
            step,
            outcome,
        }));
    }
    Ok(stdout)
}

fn private_dir(path: &Path) -> Result<(), XtaskError> {
    fs::create_dir(path).map_err(io_error(path))
}

/// The destination must not exist or be an empty directory.
fn check_destination(destination: &Path, runtime: &str) -> Result<(), XtaskError> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_dir() => {
            let empty = fs::read_dir(destination)
                .map_err(io_error(destination))?
                .next()
                .is_none();
            if empty {
                Ok(())
            } else {
                Err(refuse(runtime, Fault::AlreadyExists))
            }
        }
        Ok(_) => Err(refuse(runtime, Fault::AlreadyExists)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(destination)(source)),
    }
}

fn install(
    stage: &Path,
    scratch: &Path,
    lock: &Lock,
    project: &Project,
    tools: &Tools,
) -> Result<(), XtaskError> {
    let lib = stage.join(PROJECT_DIR);
    fs::create_dir_all(&lib).map_err(io_error(&lib))?;
    for (name, bytes) in [
        ("package.json", &project.manifest),
        ("package-lock.json", &project.lockfile),
    ] {
        let path = lib.join(name);
        fs::write(&path, bytes).map_err(io_error(&path))?;
    }

    let (home, cache, tmp) = (
        scratch.join("home"),
        scratch.join("cache"),
        scratch.join("tmp"),
    );
    // npm refuses one file loaded as both the user and the global config.
    let (user_rc, global_rc) = (scratch.join("user.npmrc"), scratch.join("global.npmrc"));
    for dir in [&home, &cache, &tmp] {
        private_dir(dir)?;
    }
    for rc in [&user_rc, &global_rc] {
        File::create(rc).map_err(io_error(rc))?;
    }

    let scripts_flag = if lock.scripts {
        "--ignore-scripts=false"
    } else {
        "--ignore-scripts"
    };
    // Only PATH (npm and its scripts need `node`) comes from the caller; no
    // token, npmrc or registry setting of the host reaches npm.
    let mut command = Command::new(&tools.npm);
    command
        .args(["ci", scripts_flag, "--no-audit", "--no-fund"])
        .current_dir(&lib)
        .env_clear()
        .env("PATH", &tools.path)
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env("npm_config_cache", &cache)
        .env("npm_config_userconfig", &user_rc)
        .env("npm_config_globalconfig", &global_rc)
        .env("npm_config_registry", &project.registry)
        .env("npm_config_update_notifier", "false");
    run_bounded(&mut command, INSTALL_TIMEOUT, false, 0, "npm ci")?;

    let bin_dir = stage.join(BIN_DIR);
    fs::create_dir_all(&bin_dir).map_err(io_error(&bin_dir))?;
    let link = bin_dir.join(&lock.binary);
    let target = format!(
        "../{PROJECT_DIR}/node_modules/{}/{}",
        lock.npm, project.bin_path
    );
    symlink(&target, &link).map_err(io_error(&link))
}

fn probe_banner(
    stage: &Path,
    scratch: &Path,
    lock: &Lock,
    tools: &Tools,
) -> Result<(), XtaskError> {
    let home = scratch.join("probe-home");
    private_dir(&home)?;
    let mut path = OsString::from(stage.join(BIN_DIR));
    path.push(":");
    path.push(&tools.path);
    let mut command = Command::new(stage.join(BIN_DIR).join(&lock.binary));
    command
        .arg("--version")
        .current_dir(&home)
        .env_clear()
        .env("PATH", path)
        .env("HOME", &home);
    let stdout = run_bounded(
        &mut command,
        PROBE_TIMEOUT,
        true,
        PROBE_OUTPUT_LIMIT,
        "--version probe",
    )?;
    let printed = String::from_utf8_lossy(&stdout);
    let printed = printed.trim_end_matches('\n');
    if printed == lock.banner {
        return Ok(());
    }
    let quoted: String = printed.chars().take(QUOTED_BANNER_CHARS).collect();
    Err(XtaskError::UpstreamStage(StageError::Banner {
        expected: lock.banner.clone(),
        actual: format!("{quoted:?}"),
    }))
}

/// Installs the locked release of `runtime` into `<out>/<runtime>/`.
///
/// Every check runs on the final tree: the pins, then the manifests, then
/// the `--version` probe, then the manifests again, so a runtime that writes
/// into its own install is caught here and not in the isolated run.
pub(crate) fn stage_upstream(
    root: &Path,
    runtime: &str,
    out: &Path,
    tools: &Tools,
) -> Result<StageSummary, XtaskError> {
    let lock = read_lock(root, runtime)?;
    let project = read_project(root, runtime, &lock)?;
    fs::create_dir_all(out).map_err(io_error(out))?;
    let out = fs::canonicalize(out).map_err(io_error(out))?;
    let destination = out.join(runtime);
    check_destination(&destination, runtime)?;

    let scratch_dir = out.join(format!(".stage-{runtime}-{}", std::process::id()));
    private_dir(&scratch_dir)?;
    let scratch = Scratch(scratch_dir);
    let stage = scratch.0.join("stage");
    fs::create_dir(&stage).map_err(io_error(&stage))?;

    install(&stage, &scratch.0, &lock, &project, tools)?;
    check_pins(&stage, &lock, &project)?;
    let tree = scan_tree(&stage)?;
    let (sha256, links) = render_manifests(&tree);
    write_manifest(&stage.join(SHA256_MANIFEST), &sha256)?;
    write_manifest(&stage.join(LINKS_MANIFEST), &links)?;
    probe_banner(&stage, &scratch.0, &lock, tools)?;
    check_manifests(&stage)?;

    if destination.is_dir() {
        fs::remove_dir(&destination).map_err(io_error(&destination))?;
    }
    fs::rename(&stage, &destination).map_err(io_error(&destination))?;
    Ok(summarize(&lock, &tree, &destination))
}

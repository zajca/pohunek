//! Pinned, hash-verified staging of the upstream runtime releases.
//!
//! `compat stage-upstream` installs the release a runtime's
//! `compat/<runtime>/compatibility-lock.json` pins into `<out>/<runtime>/`
//! with network access. The lock's `upstream` object selects how:
//!
//! - an npm release (`upstream.npm`, no `kind`) is installed from the
//!   committed npm lockfile `compat/<runtime>/npm/package-lock.json` (every
//!   dependency carries an integrity, so `npm ci` refuses any other bytes)
//!   into an npm prefix, `bin/<binary>` and `lib/node_modules/...`;
//! - a provisioned runtime (`upstream.kind = "provisioner"`) is installed by
//!   the repository script `upstream.provisioner` in place at
//!   `<out>/<runtime>/`, which it fills with its own layout; `bin/<binary>`
//!   links the entrypoint the script reports and `inputs/` holds copies of the
//!   lock and the script.
//!
//! Either stage gets two manifests, `STAGE.sha256` (`sha256sum -c` format, one
//! line per regular file) and `STAGE.links` (symbolic links and executable
//! bits), that pin every byte. `compat verify-stage` recomputes both from disk
//! without network access.
//!
//! The command is data driven: it knows no runtime by name. A lock of another
//! `schema` or `upstream.kind` is refused instead of guessed at.

// Rust guideline compliant 2026-10-15

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{symlink, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, LazyLock};
use std::thread;
use std::time::{Duration, Instant};

use clap::Subcommand;
use nix::fcntl::OFlag;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use regex::Regex;
use rustix::process::{waitid, Pid as RustixPid, WaitId, WaitIdOptions};
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

/// Wall-clock limit shared by all `npm ci` attempts of one runtime. Pi installs
/// about 150 packages; a run that takes a quarter of an hour is hung on the
/// network, not slow. Stalls end earlier, through the per-request limits below.
pub(crate) const INSTALL_TIMEOUT: Duration = Duration::from_mins(15);

/// Idle limit on each registry socket, passed as npm's `fetch-timeout`. npm's
/// default of 5 minutes lets one stalled request hold an attempt for 15 minutes.
const NPM_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Retries npm makes of a request that failed before its response headers,
/// passed as `fetch-retries`. npm's default. A request that stalls before its
/// headers costs `NPM_FETCH_RETRIES + 1` timeouts plus the backoffs below.
const NPM_FETCH_RETRIES: u32 = 2;

/// First backoff before a request retry, passed as `fetch-retry-mintimeout`.
const NPM_FETCH_RETRY_MIN: Duration = Duration::from_secs(1);

/// Largest backoff before a request retry, passed as `fetch-retry-maxtimeout`.
/// npm's default is 60 seconds, which would hold a retry longer than a stall
/// is worth; the next `npm ci` attempt covers a registry that stays slow.
const NPM_FETCH_RETRY_MAX: Duration = Duration::from_secs(10);

/// Multiplier of the backoff per request retry, passed as
/// `fetch-retry-factor`. Pinned to npm's default so the backoff sequence is the
/// same across npm releases.
const NPM_FETCH_RETRY_FACTOR: u32 = 10;

/// `npm ci` attempts per runtime. A stall after the response headers ends
/// `npm ci` without a request retry, so the attempt is the retry. Budget per
/// attempt when every request stalls before its headers: 3 timeouts of 30 s
/// plus backoffs of 1 s and 10 s = 101 s. Three attempts take at most about
/// 5 minutes, inside `INSTALL_TIMEOUT`, which bounds all attempts together.
pub(crate) const NPM_CI_ATTEMPTS: u32 = 3;

/// npm error codes that name a network failure, the only failures an attempt
/// is repeated for. Matched against the last `error code` line of npm's debug
/// log. Integrity, lockfile and HTTP status failures are not listed, so they
/// fail on the first attempt.
const TRANSIENT_NPM_ERRORS: &[&str] = &[
    // Request failed after npm's own request retries.
    "FETCH_ERROR",
    // Socket idle past `fetch-timeout`, during the response headers or body.
    "EIDLETIMEOUT",
    "ETIMEDOUT",
    "ESOCKETTIMEDOUT",
    "ECONNRESET",
    "ECONNREFUSED",
    "ECONNABORTED",
    "EPIPE",
    // DNS lookup did not complete.
    "EAI_AGAIN",
];

/// Bytes of npm's debug log printed after a failed attempt. The file ends with
/// the error block and one `silly unfinished npm timer reifyNode:...` line per
/// package still in flight; npm keeps 15 sockets open per origin by default,
/// so the in-flight packages fit in a few kilobytes.
const NPM_LOG_TAIL_BYTES: u64 = 16 * 1024;

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

/// `upstream.kind` of a runtime installed by a repository provisioner script.
const PROVISIONER_KIND: &str = "provisioner";

/// Repository directory that holds the provisioner scripts. A lock may name
/// only a file directly inside it, so a lock edit selects a reviewed
/// repository script and never a file elsewhere in or outside the checkout.
const PROVISIONER_DIR: &str = "scripts";

/// Directory of a provisioned stage that holds copies of the lock and the
/// provisioner it was built from, compared with the committed files by
/// `verify-stage`.
const INPUTS_DIR: &str = "inputs";

/// Name of the lock copy inside [`INPUTS_DIR`].
const LOCK_COPY: &str = "compatibility-lock.json";

/// Wall-clock limit of a provisioner run. The Hermes provisioner fetches a Git
/// tag, a `CPython` archive and the locked wheels in about a minute; a run that
/// takes half an hour is hung on the network, not slow.
const PROVISION_TIMEOUT: Duration = Duration::from_mins(30);

/// Interpreters a console script of a provisioned stage may name, relative
/// to the stage root. The `provisioner` kind stages a Python virtual
/// environment at `venv/` (`scripts/provision-hermes-compat` creates it there
/// with `--copies`, so both are copies of the pinned interpreter), and uv
/// writes `#!<stage>/venv/bin/<name>` into every console script, naming the
/// interpreter as its `sys.executable` reports it: `python` on Linux and
/// `python3` on macOS (uv 0.9.28 with the pinned `CPython` 3.12.13). The shebang
/// check after provisioning requires exactly one of these paths, and the
/// up-front path bound holds for each. A provisioned runtime that is not a Python
/// virtual environment needs its interpreter declared by the lock before this
/// kind can stage it.
const VENV_INTERPRETERS: [&str; 2] = ["venv/bin/python", "venv/bin/python3"];

/// Longest shebang line, `#!`, the interpreter path and the newline, that uv
/// writes as a direct `#!<python>` line. A longer line, or an interpreter path
/// with a space, makes uv write a `#!/bin/sh` exec trampoline instead
/// (`format_shebang` in uv-install-wheel, after pip's distlib), and the worker's
/// frozen launch rule refuses a trampoline root.
const SHEBANG_LINE_LIMIT: usize = 127;

/// Bytes of a shebang line besides the interpreter path: `#!` and `\n`.
const SHEBANG_OVERHEAD: usize = 3;

/// First bytes of an entrypoint read to find its shebang line; a direct
/// shebang fits in [`SHEBANG_LINE_LIMIT`].
const SHEBANG_READ_BYTES: u64 = 4096;

/// POSIX `sh` verifier of a staged tree, run as `sh -c "$SCRIPT" sh <stage>`.
///
/// It needs only `sha256sum`, `find`, `sed`, `sort`, `grep`, `readlink -e`,
/// `iconv`, `tr` and `wc`, so the network-isolated namespace can run it with
/// no Rust tooling. It refuses a modified, added or removed file, a file
/// replaced by a link, a retargeted link and a changed executable bit.
/// `docs/knowledge/concepts/upstream-staging.md` quotes it verbatim; the docs
/// check keeps the two equal, and staging scenarios run it against drifted trees.
pub(crate) const POSIX_VERIFY: &str = r#"set -eu
[ -d "$1" ] && [ ! -h "$1" ] || exit 1
cd "$1"
root=$(pwd -P && printf x) || exit 1
nl='
'
root=${root%"$nl"x}
for manifest in STAGE.sha256 STAGE.links; do
  [ -f "$manifest" ] && [ ! -h "$manifest" ] || exit 1
  # Shell read can discard NUL, so validate raw bytes before parsing.
  iconv -f UTF-8 -t UTF-8 "$manifest" >/dev/null 2>&1 || exit 1
  [ "$(LC_ALL=C tr -cd '\000' < "$manifest" | wc -c)" -eq 0 ] || exit 1
done
tab=$(printf '\t')
# UTF-8 C1 controls are C2 80..9F; grep's C locale recognizes only ASCII controls.
c1_prefix=$(printf '\302')
c1_first=$(printf '\200')
c1_last=$(printf '\237')
has_control() {
  printf '%s' "$1" | LC_ALL=C grep -q -e '[[:cntrl:]]' -e "$c1_prefix[$c1_first-$c1_last]"
}
safe_path() {
  case $1 in
    ''|/*|*/|*//*|.|..|./*|../*|*/./*|*/../*|*/.|*/..|*\\*) return 1 ;;
  esac
  ! has_control "$1"
}
safe_target() {
  case $1 in ''|*\\*) return 1 ;; esac
  ! has_control "$1"
}
stays_inside() (
  path=$1
  target=$2
  case $target in /*) exit 1 ;; esac
  depth=0
  while [ "$path" != "${path#*/}" ]; do
    depth=$((depth + 1))
    path=${path#*/}
  done
  set -f
  IFS=/
  set -- $target
  for component do
    case $component in
      ..) [ "$depth" -gt 0 ] || exit 1; depth=$((depth - 1)) ;;
      ''|.) ;;
      *) depth=$((depth + 1)) ;;
    esac
  done
)
resolved_inside() (
  path=$1
  target=$2
  case $path in */*) candidate=${path%/*} ;; *) candidate=. ;; esac
  set -f
  IFS=/
  set -- $target
  for component do
    [ -n "$component" ] || continue
    # A component below a regular file is refused here: readlink -e does not
    # fail for it on every userland.
    [ -d "$candidate" ] || exit 1
    candidate=$candidate/$component
    resolved=$(readlink -e "$candidate" && printf x) || exit 1
    case $resolved in "$root$nl"x|"$root"/*"$nl"x) ;; *) exit 1 ;; esac
  done
  case $target in */) [ -d "$candidate" ] || exit 1 ;; esac
  # Resolve the symlink itself to retain a final slash or dot after a file.
  resolved=$(readlink -e "$path" && printf x) || exit 1
  case $resolved in "$root$nl"x|"$root"/*"$nl"x) ;; *) exit 1 ;; esac
)
contains_line() {
  [ -n "$1" ] && printf '%s\n' "$1" | grep -Fxq -- "$2"
}
# Pass each directory as an argument so a newline in its name stays visible.
find . -type d -exec sh -c '
  control=$1
  newline=$2
  shift 2
  for dir do
    [ "$dir" = . ] && continue
    name=${dir##*/}
    printf "%s" "$name" | iconv -f UTF-8 -t UTF-8 >/dev/null 2>&1 || exit 1
    case $name in *\\*|*"$newline"*) exit 1 ;; esac
    if printf "%s" "$name" | LC_ALL=C grep -q -e "[[:cntrl:]]" -e "$control"; then exit 1; fi
  done
' sh "$c1_prefix[$c1_first-$c1_last]" "$nl" {} + || exit 1
files=
while :; do
  line=
  if IFS= read -r line; then :; elif [ -z "$line" ]; then break; else exit 1; fi
  case $line in *'  '*) ;; *) exit 1 ;; esac
  digest=${line%%'  '*}
  path=${line#"$digest  "}
  [ "${#digest}" -eq 64 ] || exit 1
  case $digest in *[!0-9a-f]*) exit 1 ;; esac
  safe_path "$path" || exit 1
  contains_line "$files" "$path" && exit 1
  files="${files}${files:+$nl}$path"
done < STAGE.sha256
links=
execs=
while :; do
  line=
  if IFS= read -r line; then :; elif [ -z "$line" ]; then break; else exit 1; fi
  case $line in
    "exec${tab}"*)
      path=${line#"exec${tab}"}
      case $path in *"$tab"*) exit 1 ;; esac
      safe_path "$path" || exit 1
      contains_line "$files" "$path" || exit 1
      contains_line "$execs" "$path" && exit 1
      execs="${execs}${execs:+$nl}$path"
      ;;
    "link${tab}"*)
      fields=${line#"link${tab}"}
      case $fields in *"$tab"*) ;; *) exit 1 ;; esac
      path=${fields%%"$tab"*}
      target=${fields#"$path$tab"}
      case $target in *"$tab"*) exit 1 ;; esac
      safe_path "$path" && safe_target "$target" && stays_inside "$path" "$target" || exit 1
      contains_line "$files" "$path" && exit 1
      contains_line "$links" "$path" && exit 1
      links="${links}${links:+$nl}$path"
      ;;
    *) exit 1 ;;
  esac
done < STAGE.links
expected=$({ printf '%s\n' "$files"; printf '%s\n' "$links"; } | sed '/^$/d' | LC_ALL=C sort)
actual=$(
  found=$(find . -path ./STAGE.sha256 -prune -o -path ./STAGE.links -prune -o ! -type d -print) || exit 1
  printf '%s\n' "$found" | sed 's|^\./||' | LC_ALL=C sort
)
[ "$expected" = "$actual" ]
printf '%s\n' "$files" | while IFS= read -r path; do
  [ -f "$path" ] && [ ! -h "$path" ] || exit 1
  # -perm -100 tests the owner's execute bit, not this process's access.
  owner_exec=$(find "./$path" -prune -perm -100 -print) || exit 1
  if contains_line "$execs" "$path"; then
    [ "$owner_exec" = "./$path" ] || exit 1
  else
    [ -z "$owner_exec" ] || exit 1
  fi
done
while IFS="$tab" read -r kind path target; do
  if [ "$kind" = link ]; then
    [ -h "$path" ] || exit 1
    actual=$(readlink "$path" && printf x) || exit 1
    [ "$actual" = "$target$nl"x ] || exit 1
    resolved_inside "$path" "$target" || exit 1
  fi
done < STAGE.links
sha256sum -c STAGE.sha256 >/dev/null
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
    /// The entrypoint does not, or at this path would not, start the stage's
    /// venv interpreter through a direct `#!<path>` line.
    IndirectShebang,
    /// The entry is writable by group or others.
    SharedWrite,
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
            Self::IndirectShebang => {
                "does not start the stage's venv interpreter through a direct \
                 `#!<path>` line (a `/bin/sh` trampoline, another interpreter, a path \
                 too long or with a space, or a stage moved after staging)"
            }
            Self::SharedWrite => "is writable by group or others",
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
        /// Runtime the command was staging.
        runtime: String,
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
            Self::Command {
                runtime,
                step,
                outcome,
            } => match outcome {
                Outcome::Exit(Some(code)) => {
                    write!(f, "{step} of `{runtime}` exited with code {code}")
                }
                Outcome::Exit(None) => write!(f, "{step} of `{runtime}` was killed by a signal"),
                Outcome::TimedOut => write!(f, "{step} of `{runtime}` exceeded its time limit"),
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

/// Clears group and other write bits from a file or directory.
fn clear_shared_write(path: &Path, metadata: &fs::Metadata) -> Result<(), XtaskError> {
    let mode = metadata.permissions().mode();
    if mode & 0o022 != 0 {
        let new_perms = fs::Permissions::from_mode(mode & !0o022);
        fs::set_permissions(path, new_perms).map_err(io_error(path))?;
    }
    Ok(())
}

/// Removes group and other write permissions from all entries under `root`.
///
/// uv and npm can leave shared-write entries such as `venv/.lock`, and the
/// smoke's trust check, like the product's executable lookup, refuses them.
fn drop_shared_write(root: &Path) -> Result<(), XtaskError> {
    let metadata = fs::symlink_metadata(root).map_err(io_error(root))?;
    clear_shared_write(root, &metadata)?;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = fs::read_dir(&dir).map_err(io_error(&dir))?;
        for entry in entries {
            let entry = entry.map_err(io_error(&dir))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(io_error(&path))?;
            // Skip symlinks, following them would escape the stage.
            if metadata.file_type().is_symlink() {
                continue;
            }
            clear_shared_write(&path, &metadata)?;
            if metadata.file_type().is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
}

/// Refuses the first entry under `root` that is writable by group or others.
fn refuse_shared_write(root: &Path) -> Result<(), XtaskError> {
    let metadata = fs::symlink_metadata(root).map_err(io_error(root))?;
    let mode = metadata.permissions().mode();
    if mode & 0o022 != 0 {
        return Err(refuse(".", Fault::SharedWrite));
    }
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
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
            let metadata = fs::symlink_metadata(&path).map_err(io_error(&path))?;
            let kind = metadata.file_type();
            // Skip symlinks.
            if kind.is_symlink() {
                continue;
            }
            let mode = metadata.permissions().mode();
            if mode & 0o022 != 0 {
                return Err(refuse(relative, Fault::SharedWrite));
            }
            if kind.is_dir() {
                pending.push((path, relative));
            }
        }
    }
    Ok(())
}

/// `compat` actions that stage and verify upstream releases.
#[derive(Debug, Subcommand)]
pub(crate) enum UpstreamAction {
    /// Install a runtime's locked upstream release into `<out>/<runtime>/`.
    ///
    /// Needs network access. An npm lock installs from
    /// `compat/<runtime>/npm` with `npm ci` and checks the release, the root
    /// tarball integrity and a pinned native digest; a provisioner lock runs
    /// its repository script in place and checks the entrypoint's direct
    /// interpreter shebang. Both check the `--version` banner against the
    /// lock and write `STAGE.sha256` and `STAGE.links`. Refuses a non-empty
    /// destination. A provisioned stage must stay where it was staged: its
    /// scripts name the interpreter by absolute path.
    StageUpstream {
        /// Runtime id, a directory name below `compat/`.
        #[arg(long)]
        runtime: String,
        /// Directory that receives `<runtime>/`.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// `npm` executable for an npm lock; defaults to the first `npm` on
        /// `PATH`.
        #[arg(long, value_name = "FILE")]
        npm: Option<PathBuf>,
        /// Repository root; defaults to the checkout this xtask belongs to.
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
    },
    /// Recompute a staged tree from disk and compare it with its manifests.
    ///
    /// Offline. Refuses any added, removed or modified file (a `.pyc` written
    /// by a consumer included), retargeted link or changed executable bit, a
    /// stage whose npm project or provisioner inputs differ from the committed
    /// ones, an unpinned installed release, and a provisioned entrypoint that
    /// does not start its interpreter through a direct shebang.
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
            let tools = Tools {
                npm,
                path,
                install_limit: INSTALL_TIMEOUT,
            };
            let summary = stage_upstream(&root, &runtime, &out, &tools, &mut io::stderr().lock())?;
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
    /// The `npm` executable of an npm lock; `None` looks it up on `path`.
    pub(crate) npm: Option<PathBuf>,
    /// `PATH` handed to npm, the provisioner and the probe: it must reach
    /// `node` for an npm runtime and the provisioner's own tools otherwise.
    pub(crate) path: OsString,
    /// Wall-clock limit of all `npm ci` attempts of one runtime. Production
    /// passes `INSTALL_TIMEOUT`; tests pass a shorter limit.
    pub(crate) install_limit: Duration,
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
    kind: Option<String>,
    provisioner: Option<String>,
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
    release: String,
    binary: String,
    banner: String,
    source: Source,
}

/// How a lock's release is installed.
#[derive(Debug)]
enum Source {
    /// An npm release installed from the committed npm project.
    Npm(NpmPins),
    /// A runtime installed by the repository script at this relative path.
    Provisioner(String),
}

/// The npm-specific pins of a lock.
#[derive(Debug)]
struct NpmPins {
    package: String,
    integrity: String,
    scripts: bool,
    sha256: Option<BTreeMap<String, String>>,
}

/// The committed provisioner of a runtime, validated against its lock.
#[derive(Debug)]
struct Provisioner {
    /// Canonical path of the script inside the repository.
    script: PathBuf,
    /// File name of the script; its copy in [`INPUTS_DIR`] has this name.
    name: String,
    /// The script's bytes.
    script_bytes: Vec<u8>,
    /// The lock's bytes.
    lock_bytes: Vec<u8>,
}

/// How much of the `--version` output must equal the lock's banner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BannerMatch {
    /// The whole output, without trailing newlines: an npm CLI prints only
    /// its banner.
    Whole,
    /// The first line: a provisioned runtime may print diagnostics after its
    /// banner (Hermes prints its install directory and Python version).
    FirstLine,
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
    let required = |value: Option<String>, name: &str| {
        value.ok_or_else(|| refuse(format!("upstream.{name}"), Fault::Missing))
    };
    let (release, binary, template, source) = match upstream.kind.as_deref() {
        None => {
            let package = upstream
                .npm
                .ok_or_else(|| refuse("upstream.npm", Fault::Unsupported))?;
            let release = required(upstream.release, "release")?;
            let integrity = required(upstream.integrity, "integrity")?;
            let binary = required(upstream.binary, "binary")?;
            let template = required(upstream.version_output, "version_output")?;
            if !is_sha512_sri(&integrity) {
                return Err(refuse("upstream.integrity", Fault::Malformed));
            }
            if let Some(pins) = &upstream.sha256 {
                if pins.values().any(|digest| !is_hex_digest(digest)) {
                    return Err(refuse("upstream.sha256", Fault::Malformed));
                }
            }
            let pins = NpmPins {
                package,
                integrity,
                scripts: upstream.scripts.unwrap_or(false),
                sha256: upstream.sha256,
            };
            (release, binary, template, Source::Npm(pins))
        }
        Some(PROVISIONER_KIND) => {
            // npm fields beside a provisioner would leave two install recipes
            // in one lock; neither is guessed at.
            if upstream.npm.is_some() || upstream.integrity.is_some() || upstream.sha256.is_some() {
                return Err(refuse("upstream.kind", Fault::Malformed));
            }
            let script = required(upstream.provisioner, "provisioner")?;
            let in_dir = script
                .strip_prefix(PROVISIONER_DIR)
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(safe_component);
            if !in_dir {
                return Err(refuse("upstream.provisioner", Fault::Unsupported));
            }
            let release = required(upstream.release, "release")?;
            let binary = required(upstream.binary, "binary")?;
            let template = required(upstream.version_output, "version_output")?;
            (release, binary, template, Source::Provisioner(script))
        }
        Some(_) => return Err(refuse("upstream.kind", Fault::Unsupported)),
    };
    if semver::Version::parse(&release).is_err() {
        return Err(refuse("upstream.release", Fault::Malformed));
    }
    if !safe_component(&binary) {
        return Err(refuse("upstream.binary", Fault::Malformed));
    }
    if template.matches(RELEASE_PLACEHOLDER).count() != 1 {
        return Err(refuse("upstream.version_output", Fault::Malformed));
    }
    Ok(Lock {
        banner: template.replace(RELEASE_PLACEHOLDER, &release),
        release,
        binary,
        source,
    })
}

/// Checks the committed install inputs a lock refers to (the npm project or
/// the provisioner script) without installing anything.
#[cfg(test)]
pub(crate) fn check_inputs(root: &Path, runtime: &str, lock: &Lock) -> Result<(), XtaskError> {
    read_inputs(root, runtime, lock).map(|_inputs| ())
}

/// Reads the provisioner script `relative` of the lock: a regular,
/// owner-executable file directly inside [`PROVISIONER_DIR`] of `root`, never
/// a link.
fn read_provisioner(root: &Path, runtime: &str, relative: &str) -> Result<Provisioner, XtaskError> {
    let subject = "upstream.provisioner";
    let path = root.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(refuse(subject, Fault::Missing));
        }
        Err(source) => return Err(io_error(&path)(source)),
    };
    if !metadata.is_file() {
        return Err(refuse(subject, Fault::TypeChanged));
    }
    if metadata.permissions().mode() & OWNER_EXEC == 0 {
        return Err(refuse(subject, Fault::ModeChanged));
    }
    let script = fs::canonicalize(&path).map_err(io_error(&path))?;
    let scripts_dir = root.join(PROVISIONER_DIR);
    let scripts_dir = fs::canonicalize(&scripts_dir).map_err(io_error(&scripts_dir))?;
    if script.parent() != Some(scripts_dir.as_path()) {
        return Err(refuse(subject, Fault::EscapingLink));
    }
    let name = script
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| *name != LOCK_COPY)
        .ok_or_else(|| refuse(subject, Fault::UnsafeName))?
        .to_owned();
    let script_bytes = read_bounded(&script)?;
    let lock_bytes = read_bounded(&crate::attestation::lock_path(root, runtime))?;
    Ok(Provisioner {
        script,
        name,
        script_bytes,
        lock_bytes,
    })
}

fn exact_dependency(section: Option<&Value>, lock: &Lock, pins: &NpmPins) -> bool {
    section.and_then(Value::as_object).is_some_and(|map| {
        map.len() == 1 && map.get(&pins.package).and_then(Value::as_str) == Some(&lock.release)
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

fn read_project(
    root: &Path,
    runtime: &str,
    lock: &Lock,
    pins: &NpmPins,
) -> Result<Project, XtaskError> {
    let dir = root.join("compat").join(runtime).join(NPM_DIR);
    let (manifest_json, manifest) = read_json(&dir.join("package.json"), "npm/package.json")?;
    let (lock_json, lockfile) = read_json(&dir.join("package-lock.json"), "npm/package-lock.json")?;

    if !exact_dependency(manifest_json.get("dependencies"), lock, pins)
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
    let wanted = format!("{}@{}", pins.package, lock.release);
    let expected_allowed: BTreeSet<&str> = if pins.scripts {
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
    if !exact_dependency(root_entry.get("dependencies"), lock, pins)
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

    let root_key = format!("node_modules/{}", pins.package);
    let package = packages
        .get(&root_key)
        .ok_or_else(|| refuse(format!("npm/package-lock.json {root_key}"), Fault::Missing))?;
    if package.get("version").and_then(Value::as_str) != Some(&lock.release) {
        return Err(refuse("npm/package-lock.json release", Fault::Mismatch));
    }
    if package.get("integrity").and_then(Value::as_str) != Some(&pins.integrity) {
        return Err(refuse("upstream.integrity", Fault::Mismatch));
    }
    // With scripts enabled only the locked package may run one: a dependency
    // that gained an install script would execute unreviewed code.
    if pins.scripts && script_packages != BTreeSet::from([root_key.as_str()]) {
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

/// Resolves each path prefix, including symlinks and `..`, before advancing.
/// A final path that leaves and then re-enters the stage is still unsafe.
fn resolved_inside(stage_root: &Path, path: &Path, target: &Path) -> bool {
    let Some(mut prefix) = path.parent().map(Path::to_path_buf) else {
        return false;
    };
    for component in target.components() {
        prefix.push(component.as_os_str());
        let Ok(resolved) = fs::canonicalize(&prefix) else {
            return false;
        };
        if !resolved.starts_with(stage_root) {
            return false;
        }
    }
    // Path::components discards a trailing slash or dot, while the kernel
    // rejects either when the preceding component is a regular file. A libc
    // `realpath` may not (macOS accepts `file/.` and `file/..`), so the link
    // is also resolved by the kernel itself with a plain `stat`.
    fs::metadata(path).is_ok()
        && fs::canonicalize(path).is_ok_and(|resolved| resolved.starts_with(stage_root))
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
    let stage_root = fs::canonicalize(stage).map_err(io_error(stage))?;
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
                if !resolved_inside(&stage_root, &path, &target) {
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

fn write_manifest(path: &Path, content: impl AsRef<[u8]>) -> Result<(), XtaskError> {
    let mut file = File::create(path).map_err(io_error(path))?;
    file.write_all(content.as_ref()).map_err(io_error(path))?;
    fs::set_permissions(path, fs::Permissions::from_mode(MANIFEST_MODE)).map_err(io_error(path))
}

fn read_manifest(path: &Path, subject: &str) -> Result<String, XtaskError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).bits())
        .open(path)
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => refuse(subject, Fault::Missing),
            _ if error.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32) => {
                refuse(subject, Fault::TypeChanged)
            }
            _ => io_error(path)(error),
        })?;
    if !file.metadata().map_err(io_error(path))?.is_file() {
        return Err(refuse(subject, Fault::TypeChanged));
    }
    let mut text = String::new();
    match file.read_to_string(&mut text) {
        Ok(_) => Ok(text),
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
fn check_pins(
    stage: &Path,
    lock: &Lock,
    pins: &NpmPins,
    project: &Project,
) -> Result<(), XtaskError> {
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

    let package_dir = lib.join("node_modules").join(&pins.package);
    let installed = json_file(&package_dir.join("package.json"), "installed package.json")?;
    if installed.get("version").and_then(Value::as_str) != Some(&lock.release) {
        return Err(refuse("installed release", Fault::Mismatch));
    }
    let hidden = json_file(
        &lib.join("node_modules").join(".package-lock.json"),
        "installed lockfile",
    )?;
    let root_key = format!("node_modules/{}", pins.package);
    let entry = hidden
        .get("packages")
        .and_then(|packages| packages.get(&root_key));
    if entry.and_then(|e| e.get("version")).and_then(Value::as_str) != Some(&lock.release)
        || entry
            .and_then(|e| e.get("integrity"))
            .and_then(Value::as_str)
            != Some(&pins.integrity)
    {
        return Err(refuse("installed integrity", Fault::Mismatch));
    }

    let bin = stage.join(BIN_DIR).join(&lock.binary);
    let expected_target = format!(
        "../{PROJECT_DIR}/node_modules/{}/{}",
        pins.package, project.bin_path
    );
    let target = fs::read_link(&bin)
        .map_err(|_cause| refuse(format!("{BIN_DIR}/{}", lock.binary), Fault::Missing))?;
    if target != Path::new(&expected_target) {
        return Err(refuse(
            format!("{BIN_DIR}/{}", lock.binary),
            Fault::Retargeted,
        ));
    }

    if let Some(digests) = &pins.sha256 {
        let key = host_key().ok_or_else(|| refuse("upstream.sha256", Fault::Unpinned))?;
        let pinned = digests
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

/// Checks a provisioned stage against the lock and the committed provisioner:
/// the copies in [`INPUTS_DIR`] equal the committed lock and script, and
/// `bin/<binary>` links an entrypoint of that name inside the stage that
/// starts its interpreter through a direct shebang.
fn check_provisioned(
    stage: &Path,
    lock: &Lock,
    provisioner: &Provisioner,
) -> Result<(), XtaskError> {
    let inputs = stage.join(INPUTS_DIR);
    for (name, committed) in [
        (LOCK_COPY, &provisioner.lock_bytes),
        (provisioner.name.as_str(), &provisioner.script_bytes),
    ] {
        let subject = format!("{INPUTS_DIR}/{name}");
        let bytes = read_bounded(&inputs.join(name)).map_err(|error| match error {
            XtaskError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                refuse(subject.clone(), Fault::Missing)
            }
            other => other,
        })?;
        if &bytes != committed {
            return Err(refuse(subject, Fault::Mismatch));
        }
    }

    let subject = format!("{BIN_DIR}/{}", lock.binary);
    let bin = stage.join(BIN_DIR).join(&lock.binary);
    if !fs::symlink_metadata(&bin).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(refuse(subject, Fault::Missing));
    }
    let inside = fs::canonicalize(stage).map_err(io_error(stage))?;
    let entrypoint =
        fs::canonicalize(&bin).map_err(|_cause| refuse(subject.clone(), Fault::Missing))?;
    if !entrypoint.starts_with(&inside) {
        return Err(refuse(subject, Fault::EscapingLink));
    }
    if entrypoint.file_name() != Some(OsStr::new(&lock.binary)) {
        return Err(refuse(subject, Fault::Retargeted));
    }
    check_direct_shebang(&entrypoint, &inside, &subject)
}

/// Requires the first line of `script` to be exactly `#!<stage>/<path>` for
/// one of the [`VENV_INTERPRETERS`], with the canonical `stage` path, and that
/// interpreter to be a regular owner-executable file: the line uv writes when
/// the path is short enough and has no space. A `/bin/sh` trampoline, another
/// interpreter (`python3.12` beside them included), and a stage moved after
/// staging (its shebangs keep the old absolute path) fail.
fn check_direct_shebang(script: &Path, stage: &Path, subject: &str) -> Result<(), XtaskError> {
    let indirect = || refuse(subject, Fault::IndirectShebang);
    let mut head = Vec::new();
    File::open(script)
        .map_err(io_error(script))?
        .take(SHEBANG_READ_BYTES)
        .read_to_end(&mut head)
        .map_err(io_error(script))?;
    let line = head.split(|byte| *byte == b'\n').next().unwrap_or_default();
    let interpreter = VENV_INTERPRETERS
        .iter()
        .map(|relative| stage.join(relative))
        .find(|candidate| line.strip_prefix(b"#!") == Some(candidate.as_os_str().as_bytes()))
        .ok_or_else(indirect)?;
    let executable = fs::symlink_metadata(&interpreter).is_ok_and(|metadata| {
        metadata.is_file() && metadata.permissions().mode() & OWNER_EXEC != 0
    });
    if !executable {
        return Err(indirect());
    }
    Ok(())
}

/// The committed install inputs of a lock.
enum Inputs<'lock> {
    /// The npm pins of the lock and the committed npm project.
    Npm(&'lock NpmPins, Project),
    /// The committed provisioner script.
    Provisioner(Provisioner),
}

fn read_inputs<'lock>(
    root: &Path,
    runtime: &str,
    lock: &'lock Lock,
) -> Result<Inputs<'lock>, XtaskError> {
    match &lock.source {
        Source::Npm(pins) => Ok(Inputs::Npm(pins, read_project(root, runtime, lock, pins)?)),
        Source::Provisioner(script) => Ok(Inputs::Provisioner(read_provisioner(
            root, runtime, script,
        )?)),
    }
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
/// manifests, the committed npm project or provisioner and the lock's pins.
/// Offline.
pub(crate) fn verify_stage(
    root: &Path,
    runtime: &str,
    dir: &Path,
) -> Result<StageSummary, XtaskError> {
    let lock = read_lock(root, runtime)?;
    let inputs = read_inputs(root, runtime, &lock)?;
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
    match &inputs {
        Inputs::Npm(pins, project) => check_pins(&stage, &lock, pins, project)?,
        Inputs::Provisioner(provisioner) => check_provisioned(&stage, &lock, provisioner)?,
    }
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

/// Waits without reaping, preserving the group ID until cleanup.
fn observe_child_exit(pid: RustixPid) -> io::Result<()> {
    loop {
        match waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        ) {
            Ok(Some(_status)) => return Ok(()),
            Ok(None) => return Err(io::Error::other("child observation returned no status")),
            Err(rustix::io::Errno::INTR) => {}
            Err(source) => return Err(io::Error::from(source)),
        }
    }
}

enum ChildEvent {
    Exited(io::Result<()>),
    Output(io::Result<Vec<u8>>),
}

/// Runs `command` in its own process group with a single deadline for exit
/// and output collection. All members of the group are killed before the
/// direct child is reaped, including after a successful command.
pub(crate) fn run_bounded(
    command: &mut Command,
    limit: Duration,
    capture: bool,
    output_limit: u64,
    runtime: &str,
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
    let pid = match i32::try_from(child.id()) {
        Ok(pid) => pid,
        Err(_cause) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(refuse(step, Fault::Malformed));
        }
    };
    let Some(rustix_pid) = RustixPid::from_raw(pid) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(refuse(step, Fault::Malformed));
    };
    let group = Pid::from_raw(pid);
    let deadline = Instant::now() + limit;
    let (sender, receiver) = mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        let sender = sender.clone();
        thread::spawn(move || {
            let mut kept = Vec::new();
            let read = (&mut stdout).take(output_limit).read_to_end(&mut kept);
            // Keep draining so the child never blocks on a full pipe.
            let drained = io::copy(&mut stdout, &mut io::sink());
            let _ = sender.send(ChildEvent::Output(read.and(drained).map(|_| kept)));
        });
    }
    thread::spawn(move || {
        let _ = sender.send(ChildEvent::Exited(observe_child_exit(rustix_pid)));
    });

    let mut exited = false;
    let mut output = (!capture).then(Vec::new);
    let mut timed_out = false;
    let observed = loop {
        if exited {
            if let Some(bytes) = output.take() {
                break Ok(bytes);
            }
        }
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(ChildEvent::Exited(Ok(()))) => {
                exited = true;
                let _ = killpg(group, Signal::SIGKILL);
            }
            Ok(ChildEvent::Exited(Err(source)) | ChildEvent::Output(Err(source))) => {
                break Err(XtaskError::Io {
                    path: PathBuf::from(step),
                    source,
                });
            }
            Ok(ChildEvent::Output(Ok(bytes))) => output = Some(bytes),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                break Ok(Vec::new());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(refuse(step, Fault::Malformed));
            }
        }
    };
    if !exited {
        let _ = killpg(group, Signal::SIGKILL);
        // The direct child may have created a new session after spawn.
        let _ = child.kill();
    }
    let status = child.wait().map_err(|source| XtaskError::Io {
        path: PathBuf::from(step),
        source,
    })?;
    let stdout = observed?;
    let outcome = if timed_out {
        Outcome::TimedOut
    } else {
        Outcome::Exit(status.code())
    };
    if outcome != Outcome::Exit(Some(0)) {
        return Err(XtaskError::UpstreamStage(StageError::Command {
            runtime: runtime.to_owned(),
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

/// One runtime's `npm ci` inputs, and where the diagnostics of failed
/// attempts go.
struct NpmCi<'a> {
    runtime: &'a str,
    npm: &'a Path,
    pins: &'a NpmPins,
    project: &'a Project,
    tools: &'a Tools,
    diagnostics: &'a mut dyn Write,
}

/// Installs the committed npm project into `stage`, repeating the attempt for
/// a network failure only, while the install limit lasts.
fn install(
    stage: &Path,
    scratch: &Path,
    lock: &Lock,
    ci: &mut NpmCi<'_>,
) -> Result<(), XtaskError> {
    let lib = stage.join(PROJECT_DIR);
    let (home, cache, tmp, logs) = (
        scratch.join("home"),
        scratch.join("cache"),
        scratch.join("tmp"),
        scratch.join("logs"),
    );
    // npm refuses one file loaded as both the user and the global config.
    let (user_rc, global_rc) = (scratch.join("user.npmrc"), scratch.join("global.npmrc"));
    for dir in [&home, &cache, &tmp, &logs] {
        private_dir(dir)?;
    }
    for rc in [&user_rc, &global_rc] {
        File::create(rc).map_err(io_error(rc))?;
    }

    let scripts_flag = if ci.pins.scripts {
        "--ignore-scripts=false"
    } else {
        "--ignore-scripts"
    };
    let deadline = Instant::now() + ci.tools.install_limit;
    let mut attempt = 1;
    loop {
        // Every attempt starts from the committed bytes: `scan_tree` hashes the
        // whole stage, so a file a failed attempt left behind must not survive.
        reset_project(&lib, ci.project)?;
        let attempt_logs = logs.join(attempt.to_string());
        private_dir(&attempt_logs)?;
        // Only PATH (npm and its scripts need `node`) comes from the caller; no
        // token, npmrc or registry setting of the host reaches npm.
        let mut command = Command::new(ci.npm);
        command
            .args(["ci", scripts_flag, "--no-audit", "--no-fund"])
            .current_dir(&lib)
            .env_clear()
            .env("PATH", &ci.tools.path)
            .env("HOME", &home)
            .env("TMPDIR", &tmp)
            .env("npm_config_cache", &cache)
            .env("npm_config_userconfig", &user_rc)
            .env("npm_config_globalconfig", &global_rc)
            .env("npm_config_registry", &ci.project.registry)
            .env("npm_config_update_notifier", "false")
            .env("npm_config_logs_dir", &attempt_logs)
            .env(
                "npm_config_fetch_timeout",
                NPM_FETCH_TIMEOUT.as_millis().to_string(),
            )
            .env("npm_config_fetch_retries", NPM_FETCH_RETRIES.to_string())
            .env(
                "npm_config_fetch_retry_mintimeout",
                NPM_FETCH_RETRY_MIN.as_millis().to_string(),
            )
            .env(
                "npm_config_fetch_retry_maxtimeout",
                NPM_FETCH_RETRY_MAX.as_millis().to_string(),
            )
            .env(
                "npm_config_fetch_retry_factor",
                NPM_FETCH_RETRY_FACTOR.to_string(),
            );
        let remaining = deadline.saturating_duration_since(Instant::now());
        let error = match run_bounded(&mut command, remaining, false, 0, ci.runtime, "npm ci") {
            Ok(_) => break,
            Err(XtaskError::UpstreamStage(error)) => error,
            Err(other) => return Err(other),
        };
        let retry = attempt < NPM_CI_ATTEMPTS && is_transient(&error, &attempt_logs);
        report_attempt(ci.diagnostics, ci.runtime, attempt, &error, &attempt_logs);
        if !retry {
            return Err(XtaskError::UpstreamStage(error));
        }
        attempt += 1;
    }

    let bin_dir = stage.join(BIN_DIR);
    fs::create_dir_all(&bin_dir).map_err(io_error(&bin_dir))?;
    let link = bin_dir.join(&lock.binary);
    let target = format!(
        "../{PROJECT_DIR}/node_modules/{}/{}",
        ci.pins.package, ci.project.bin_path
    );
    symlink(&target, &link).map_err(io_error(&link))
}

/// Empties `lib` and writes the committed manifest and lockfile into it.
fn reset_project(lib: &Path, project: &Project) -> Result<(), XtaskError> {
    match fs::remove_dir_all(lib) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(lib)(source)),
    }
    fs::create_dir(lib).map_err(io_error(lib))?;
    for (name, bytes) in [
        ("package.json", &project.manifest),
        ("package-lock.json", &project.lockfile),
    ] {
        let path = lib.join(name);
        fs::write(&path, bytes).map_err(io_error(&path))?;
    }
    Ok(())
}

/// Whether a failed attempt may be repeated. A timeout has no budget left, and
/// a signal or an error without a network code is not transient; an unreadable
/// log fails closed.
fn is_transient(error: &StageError, logs: &Path) -> bool {
    let StageError::Command {
        outcome: Outcome::Exit(Some(_)),
        ..
    } = error
    else {
        return false;
    };
    matches!(
        last_npm_error_code(logs),
        Ok(Some(code)) if TRANSIENT_NPM_ERRORS.contains(&code.as_str())
    )
}

/// The newest debug log in `logs`. npm names its logs by their start time, so
/// the greatest file name is the newest.
fn newest_npm_log(logs: &Path) -> io::Result<Option<PathBuf>> {
    let mut newest: Option<PathBuf> = None;
    for entry in fs::read_dir(logs)? {
        let path = entry?.path();
        if newest
            .as_ref()
            .is_none_or(|current| path.file_name() > current.file_name())
        {
            newest = Some(path);
        }
    }
    Ok(newest)
}

/// The code of the last `N error code CODE` line of the newest npm log.
fn last_npm_error_code(logs: &Path) -> io::Result<Option<String>> {
    let Some(path) = newest_npm_log(logs)? else {
        return Ok(None);
    };
    let mut code = None;
    for line in BufReader::new(File::open(path)?).lines() {
        if let Some(found) = npm_error_code(&line?) {
            code = Some(found.to_owned());
        }
    }
    Ok(code)
}

/// The code of an npm log line of the form `<index> error code <CODE>`.
fn npm_error_code(line: &str) -> Option<&str> {
    let (index, rest) = line.split_once(' ')?;
    if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    rest.strip_prefix("error code ").map(str::trim)
}

/// Prints a failed attempt and the tail of its npm debug log, one line each,
/// prefixed with the runtime so the job log names what stalled.
fn report_attempt(
    out: &mut dyn Write,
    runtime: &str,
    attempt: u32,
    error: &StageError,
    logs: &Path,
) {
    // Diagnostics are best effort: a closed stderr must not replace the error.
    let _ = writeln!(
        out,
        "[{runtime}] npm ci attempt {attempt} of {NPM_CI_ATTEMPTS} failed: {error}"
    );
    match npm_log_tail(logs) {
        Ok(Some((path, tail))) => {
            let _ = writeln!(
                out,
                "[{runtime}] npm debug log {} (last {NPM_LOG_TAIL_BYTES} bytes):",
                path.display()
            );
            for line in tail.lines() {
                let _ = writeln!(out, "[{runtime}] {}", line.escape_debug());
            }
        }
        Ok(None) => {
            let _ = writeln!(out, "[{runtime}] npm wrote no debug log");
        }
        Err(source) => {
            let _ = writeln!(
                out,
                "[{runtime}] could not read the npm debug log: {source}"
            );
        }
    }
}

/// The path and the last `NPM_LOG_TAIL_BYTES` of the newest npm log, without
/// the partial line the read starts in.
fn npm_log_tail(logs: &Path) -> io::Result<Option<(PathBuf, String)>> {
    let Some(path) = newest_npm_log(logs)? else {
        return Ok(None);
    };
    let mut file = File::open(&path)?;
    let start = file.metadata()?.len().saturating_sub(NPM_LOG_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(NPM_LOG_TAIL_BYTES)
        .read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        match text.find('\n') {
            Some(newline) => text.drain(..=newline).for_each(drop),
            None => text.clear(),
        }
    }
    Ok(Some((path, text)))
}

/// Runs `<stage>/bin/<binary> --version` in a private `HOME` and compares the
/// output with the lock's banner.
///
/// `PYTHONDONTWRITEBYTECODE=1` keeps a Python runtime from writing `.pyc`
/// files into the stage it imports from; the manifests are re-checked after
/// the probe, so a runtime that writes into its install anyway is refused.
fn probe_banner(
    stage: &Path,
    scratch: &Path,
    lock: &Lock,
    runtime: &str,
    tools: &Tools,
    matching: BannerMatch,
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
        .env("HOME", &home)
        .env("PYTHONDONTWRITEBYTECODE", "1");
    let stdout = run_bounded(
        &mut command,
        PROBE_TIMEOUT,
        true,
        PROBE_OUTPUT_LIMIT,
        runtime,
        "--version probe",
    )?;
    let printed = String::from_utf8_lossy(&stdout);
    let printed = match matching {
        BannerMatch::Whole => printed.trim_end_matches('\n'),
        BannerMatch::FirstLine => printed.split('\n').next().unwrap_or_default(),
    };
    if printed == lock.banner {
        return Ok(());
    }
    let quoted: String = printed.chars().take(QUOTED_BANNER_CHARS).collect();
    Err(XtaskError::UpstreamStage(StageError::Banner {
        expected: lock.banner.clone(),
        actual: format!("{quoted:?}"),
    }))
}

/// Installs the locked release of `runtime` into `<out>/<runtime>/`, the way
/// the lock's `upstream` object selects.
pub(crate) fn stage_upstream(
    root: &Path,
    runtime: &str,
    out: &Path,
    tools: &Tools,
    diagnostics: &mut dyn Write,
) -> Result<StageSummary, XtaskError> {
    let lock = read_lock(root, runtime)?;
    let inputs = read_inputs(root, runtime, &lock)?;
    fs::create_dir_all(out).map_err(io_error(out))?;
    let out = fs::canonicalize(out).map_err(io_error(out))?;
    let destination = out.join(runtime);
    check_destination(&destination, runtime)?;
    match &inputs {
        Inputs::Npm(pins, project) => {
            let npm = match &tools.npm {
                Some(npm) => npm.clone(),
                None => find_in_path("npm", &tools.path)?,
            };
            let mut ci = NpmCi {
                runtime,
                npm: &npm,
                pins,
                project,
                tools,
                diagnostics,
            };
            stage_npm(&out, &destination, &lock, &mut ci)
        }
        Inputs::Provisioner(provisioner) => {
            stage_provisioned(&out, &destination, runtime, &lock, provisioner, tools)
        }
    }
}

/// Installs an npm release in a scratch directory and moves it to
/// `destination` once every check passed.
///
/// Every check runs on the final tree: the pins, then the manifests, then
/// the `--version` probe, then the manifests again, so a runtime that writes
/// into its own install is caught here and not in the isolated run.
fn stage_npm(
    out: &Path,
    destination: &Path,
    lock: &Lock,
    ci: &mut NpmCi<'_>,
) -> Result<StageSummary, XtaskError> {
    let scratch_dir = out.join(format!(".stage-{}-{}", ci.runtime, std::process::id()));
    private_dir(&scratch_dir)?;
    let scratch = Scratch(scratch_dir);
    let stage = scratch.0.join("stage");
    fs::create_dir(&stage).map_err(io_error(&stage))?;

    install(&stage, &scratch.0, lock, ci)?;
    check_pins(&stage, lock, ci.pins, ci.project)?;
    drop_shared_write(&stage)?;
    let tree = scan_tree(&stage)?;
    let (sha256, links) = render_manifests(&tree);
    write_manifest(&stage.join(SHA256_MANIFEST), &sha256)?;
    write_manifest(&stage.join(LINKS_MANIFEST), &links)?;
    probe_banner(
        &stage,
        &scratch.0,
        lock,
        ci.runtime,
        ci.tools,
        BannerMatch::Whole,
    )?;
    check_manifests(&stage)?;
    refuse_shared_write(&stage)?;

    if destination.is_dir() {
        fs::remove_dir(destination).map_err(io_error(destination))?;
    }
    fs::rename(&stage, destination).map_err(io_error(destination))?;
    Ok(summarize(lock, &tree, destination))
}

/// Removes a stage built in place unless staging completed.
struct Partial {
    stage: PathBuf,
    complete: bool,
}

impl Drop for Partial {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        if let Err(error) = fs::remove_dir_all(&self.stage) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!(
                    "warning: could not remove the partial stage `{}`: {error}",
                    self.stage.display()
                );
            }
        }
    }
}

/// Refuses a stage path at which uv would write a `#!/bin/sh` trampoline
/// instead of a direct interpreter shebang, before anything is installed.
fn check_stage_path(destination: &Path) -> Result<(), XtaskError> {
    for relative in VENV_INTERPRETERS {
        let interpreter = destination.join(relative);
        let bytes = interpreter.as_os_str().as_bytes();
        if bytes.len() + SHEBANG_OVERHEAD > SHEBANG_LINE_LIMIT
            || bytes.iter().any(u8::is_ascii_whitespace)
        {
            return Err(refuse("out", Fault::IndirectShebang));
        }
    }
    Ok(())
}

/// Runs the provisioner with `destination` as its install root and returns
/// the entrypoint it prints, relative to `destination`.
///
/// Only `PATH` comes from the caller, as for npm: the script finds its own
/// tools there and pins everything it downloads, and no credential or index
/// setting of the host reaches it.
fn provision(
    destination: &Path,
    scratch: &Path,
    runtime: &str,
    lock: &Lock,
    provisioner: &Provisioner,
    tools: &Tools,
) -> Result<String, XtaskError> {
    let (home, tmp) = (scratch.join("home"), scratch.join("tmp"));
    for dir in [&home, &tmp] {
        private_dir(dir)?;
    }
    let mut command = Command::new(&provisioner.script);
    command
        .arg(destination)
        .current_dir(&home)
        .env_clear()
        .env("PATH", &tools.path)
        .env("HOME", &home)
        .env("TMPDIR", &tmp);
    let stdout = run_bounded(
        &mut command,
        PROVISION_TIMEOUT,
        true,
        PROBE_OUTPUT_LIMIT,
        runtime,
        "provisioner",
    )?;
    let subject = "provisioner output";
    let printed = std::str::from_utf8(&stdout)
        .ok()
        .and_then(|text| text.strip_suffix('\n'))
        .ok_or_else(|| refuse(subject, Fault::Malformed))?;
    let relative = Path::new(printed)
        .strip_prefix(destination)
        .ok()
        .and_then(Path::to_str)
        .filter(|relative| safe_relative(relative))
        .ok_or_else(|| refuse(subject, Fault::EscapingLink))?
        .to_owned();
    if Path::new(&relative).file_name() != Some(OsStr::new(&lock.binary)) {
        return Err(refuse(subject, Fault::Mismatch));
    }
    let entrypoint = destination.join(&relative);
    if !fs::symlink_metadata(&entrypoint).is_ok_and(|metadata| metadata.is_file()) {
        return Err(refuse(subject, Fault::TypeChanged));
    }
    Ok(relative)
}

/// Provisions a runtime in place at `destination` and records it.
///
/// The stage is built where it will be used and never moved, because the
/// console scripts of a virtual environment name their interpreter by
/// absolute path. The checks run on the final tree as for npm: inputs and
/// entrypoint, manifests, the `--version` probe, then the manifests again.
fn stage_provisioned(
    out: &Path,
    destination: &Path,
    runtime: &str,
    lock: &Lock,
    provisioner: &Provisioner,
    tools: &Tools,
) -> Result<StageSummary, XtaskError> {
    check_stage_path(destination)?;
    let scratch_dir = out.join(format!(".stage-{runtime}-{}", std::process::id()));
    private_dir(&scratch_dir)?;
    let scratch = Scratch(scratch_dir);
    // The provisioner refuses an existing install root; check_destination
    // left only an empty directory or nothing.
    if destination.is_dir() {
        fs::remove_dir(destination).map_err(io_error(destination))?;
    }
    let mut partial = Partial {
        stage: destination.to_path_buf(),
        complete: false,
    };

    let entrypoint = provision(destination, &scratch.0, runtime, lock, provisioner, tools)?;
    for dir in [BIN_DIR, INPUTS_DIR] {
        let path = destination.join(dir);
        match fs::symlink_metadata(&path) {
            Ok(_) => return Err(refuse(dir, Fault::AlreadyExists)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&path)(source)),
        }
        fs::create_dir(&path).map_err(io_error(&path))?;
    }
    let link = destination.join(BIN_DIR).join(&lock.binary);
    symlink(format!("../{entrypoint}"), &link).map_err(io_error(&link))?;
    let inputs = destination.join(INPUTS_DIR);
    // Plain, non-executable copies: they are compared, never run.
    write_manifest(&inputs.join(LOCK_COPY), &provisioner.lock_bytes)?;
    write_manifest(&inputs.join(&provisioner.name), &provisioner.script_bytes)?;

    drop_shared_write(destination)?;
    check_provisioned(destination, lock, provisioner)?;
    let tree = scan_tree(destination)?;
    let (sha256, links) = render_manifests(&tree);
    write_manifest(&destination.join(SHA256_MANIFEST), &sha256)?;
    write_manifest(&destination.join(LINKS_MANIFEST), &links)?;
    probe_banner(
        destination,
        &scratch.0,
        lock,
        runtime,
        tools,
        BannerMatch::FirstLine,
    )?;
    check_manifests(destination)?;
    refuse_shared_write(destination)?;
    partial.complete = true;
    Ok(summarize(lock, &tree, destination))
}

//! Test-only discovery of the interpreter that stands in for the pinned Hermes runtime.

// Rust guideline compliant 2026-09-29

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use nix::unistd::Uid;

use super::error::Error;
use super::runner::{filesystem_root_uid, safe_ancestor, validate_runtime};

/// Environment variable naming an explicit interpreter for tests and end-to-end suites.
const INTERPRETER_ENV: &str = "POHUNEK_PYTHON_BIN";
/// Last-resort location searched after every `PATH` entry.
const SYSTEM_INTERPRETER: &str = "/usr/bin/python3";
/// Executable name searched in each absolute `PATH` directory.
const INTERPRETER_NAME: &str = "python3";
/// The embedded plugin uses syntax and typing features from Python 3.10.
const MINIMUM_VERSION: (u8, u8) = (3, 10);
/// The staged validator imports `yaml`, which the pinned Hermes runtime provides.
const PROBE_MODULE: &str = "yaml";
/// Deterministic locale for the capability probe.
const PROBE_LOCALE: &str = "C";

static INTERPRETER: OnceLock<PathBuf> = OnceLock::new();

/// Returns the canonical interpreter used to emulate the pinned Hermes runtime.
///
/// The interpreter passes the production runtime policy
/// ([`validate_runtime`]: canonical file owned by root or the current user,
/// no group or other write bit, effective execute permission, safe
/// ancestors) and can import the embedded plugin's requirements.
///
/// # Panics
///
/// Panics with the requirements when `POHUNEK_PYTHON_BIN`, `PATH`, and the
/// system location offer no suitable interpreter.
pub(crate) fn interpreter() -> &'static Path {
    INTERPRETER.get_or_init(|| {
        let mut rejections = Vec::new();
        for candidate in candidates() {
            match examine(&candidate) {
                Ok(path) => return path,
                Err(reason) => rejections.push(format!("{}: {reason}", candidate.display())),
            }
        }
        panic!(
            "no usable test interpreter: need Python >= {}.{} with `{PROBE_MODULE}` on PATH, at \
             {SYSTEM_INTERPRETER}, or in ${INTERPRETER_ENV}; rejected candidates:\n{}",
            MINIMUM_VERSION.0,
            MINIMUM_VERSION.1,
            rejections.join("\n")
        )
    })
}

fn candidates() -> Vec<PathBuf> {
    if let Some(explicit) = std::env::var_os(INTERPRETER_ENV) {
        return vec![PathBuf::from(explicit)];
    }
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|directory| directory.is_absolute())
                .map(|directory| directory.join(INTERPRETER_NAME))
                .collect()
        })
        .unwrap_or_default();
    candidates.push(PathBuf::from(SYSTEM_INTERPRETER));
    candidates
}

/// Applies the production runtime policy and the capability probe, naming the
/// first failing check.
fn examine(candidate: &Path) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(candidate)
        .map_err(|error| format!("cannot canonicalize ({:?})", error.kind()))?;
    let uid = Uid::effective().as_raw();
    if validate_runtime(&canonical, uid).is_err() {
        return Err(format!(
            "{} violates the runtime policy: {}",
            canonical.display(),
            policy_violations(&canonical, uid)
        ));
    }
    let probe = format!(
        "import sys, {PROBE_MODULE}; raise SystemExit(0 if sys.version_info >= {MINIMUM_VERSION:?} else 1)"
    );
    let output = Command::new(&canonical)
        .args(["-I", "-c", &probe])
        .env_clear()
        .env("LANG", PROBE_LOCALE)
        .output()
        .map_err(|error| format!("cannot run ({:?})", error.kind()))?;
    if output.status.success() {
        Ok(canonical)
    } else {
        Err(format!(
            "capability probe failed (Python >= {}.{} with `{PROBE_MODULE}`): {}",
            MINIMUM_VERSION.0,
            MINIMUM_VERSION.1,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Lists the owner, mode, and ancestor findings behind a policy rejection.
fn policy_violations(path: &Path, uid: u32) -> String {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let Ok(root_uid) = filesystem_root_uid(Error::InvalidHermesRuntime) else {
        return "cannot inspect the filesystem root".to_owned();
    };
    let mut findings = Vec::new();
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let mode = metadata.permissions().mode();
            if !metadata.is_file() {
                findings.push("not a regular file".to_owned());
            }
            if metadata.uid() != uid && metadata.uid() != root_uid {
                findings.push(format!(
                    "owner uid {} is neither {uid} nor root",
                    metadata.uid()
                ));
            }
            if mode & 0o022 != 0 {
                findings.push(format!(
                    "file mode {:o} has group/other write bits",
                    mode & 0o7777
                ));
            }
        }
        Err(error) => findings.push(format!("cannot stat ({:?})", error.kind())),
    }
    for ancestor in path.ancestors() {
        if let Ok(metadata) = std::fs::metadata(ancestor) {
            let mode = metadata.permissions().mode();
            if !safe_ancestor(metadata.uid(), mode, metadata.is_dir(), uid, root_uid) {
                findings.push(format!(
                    "ancestor {} is unsafe (uid {}, mode {:o})",
                    ancestor.display(),
                    metadata.uid(),
                    mode & 0o7777
                ));
            }
        }
    }
    if findings.is_empty() {
        findings.push("not effectively executable by this user".to_owned());
    }
    findings.join("; ")
}

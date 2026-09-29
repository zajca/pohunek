//! Portable discovery of the Python interpreter and Bash used by Hermes integration tests.
//!
//! The interpreter stands in for the pinned Hermes runtime: it must be
//! Python 3.10 or newer and import `yaml`, which the staged validator needs.

#![allow(
    dead_code,
    reason = "each Hermes test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-09-29

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Environment variable naming an explicit interpreter.
const INTERPRETER_ENV: &str = "POHUNEK_PYTHON_BIN";
/// Searched after every absolute `PATH` entry.
const SYSTEM_PYTHON: &str = "/usr/bin/python3";
/// Searched after every absolute `PATH` entry; present on Linux and macOS.
const SYSTEM_BASH: &str = "/bin/bash";
/// The embedded plugin uses syntax and typing features from Python 3.10.
const MINIMUM_VERSION: (u8, u8) = (3, 10);
/// Group and other write bits would let another account replace the binary.
const UNSAFE_WRITE_BITS: u32 = 0o022;
/// Any execute bit; the probe run below proves effective execution.
const EXECUTE_BITS: u32 = 0o111;
/// Deterministic locale for capability probes.
const PROBE_LOCALE: &str = "C";

static PYTHON: OnceLock<PathBuf> = OnceLock::new();
static BASH: OnceLock<PathBuf> = OnceLock::new();

/// Returns the canonical Python 3.10+ interpreter with `yaml` importable.
///
/// # Panics
///
/// Panics with the requirements and every candidate's rejection reason when
/// no candidate qualifies.
pub(crate) fn python() -> &'static Path {
    PYTHON.get_or_init(|| {
        let probe = format!(
            "import sys, yaml; raise SystemExit(0 if sys.version_info >= {MINIMUM_VERSION:?} else 1)"
        );
        let explicit = std::env::var_os(INTERPRETER_ENV).map(PathBuf::from);
        select(
            &candidates("python3", SYSTEM_PYTHON, explicit),
            &["-I", "-c", &probe],
            &format!(
                "need Python >= {}.{} with `yaml` on PATH, at {SYSTEM_PYTHON}, or in ${INTERPRETER_ENV}",
                MINIMUM_VERSION.0, MINIMUM_VERSION.1
            ),
        )
    })
}

/// Returns a canonical Bash from `PATH` or `/bin/bash`.
///
/// # Panics
///
/// Panics with every candidate's rejection reason when no Bash qualifies.
pub(crate) fn bash() -> &'static Path {
    BASH.get_or_init(|| {
        select(
            &candidates("bash", SYSTEM_BASH, None),
            &["-c", "exit 0"],
            &format!("need Bash on PATH or at {SYSTEM_BASH}"),
        )
    })
}

fn select(candidates: &[PathBuf], probe_args: &[&str], requirement: &str) -> PathBuf {
    let mut rejections = Vec::new();
    for candidate in candidates {
        match examine(candidate, probe_args) {
            Ok(path) => return path,
            Err(reason) => rejections.push(format!("{}: {reason}", candidate.display())),
        }
    }
    panic!(
        "no usable test executable: {requirement}; rejected candidates:\n{}",
        rejections.join("\n")
    )
}

fn candidates(name: &str, system: &str, explicit: Option<PathBuf>) -> Vec<PathBuf> {
    if let Some(explicit) = explicit {
        return vec![explicit];
    }
    let mut found: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|directory| directory.is_absolute())
                .map(|directory| directory.join(name))
                .collect()
        })
        .unwrap_or_default();
    found.push(PathBuf::from(system));
    found
}

/// Accepts a canonical, root- or user-owned, non-group/other-writable
/// executable that passes the capability probe, naming the first failing check.
fn examine(candidate: &Path, probe_args: &[&str]) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(candidate)
        .map_err(|error| format!("cannot canonicalize ({:?})", error.kind()))?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|error| format!("{}: cannot stat ({:?})", canonical.display(), error.kind()))?;
    let mode = metadata.permissions().mode();
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", canonical.display()));
    }
    if metadata.uid() != 0 && metadata.uid() != effective_uid() {
        return Err(format!(
            "{} owner uid {} is neither root nor {}",
            canonical.display(),
            metadata.uid(),
            effective_uid()
        ));
    }
    if mode & UNSAFE_WRITE_BITS != 0 {
        return Err(format!(
            "{} mode {:o} has group/other write bits",
            canonical.display(),
            mode & 0o7777
        ));
    }
    if mode & EXECUTE_BITS == 0 {
        return Err(format!("{} has no execute bit", canonical.display()));
    }
    let output = Command::new(&canonical)
        .args(probe_args)
        .env_clear()
        .env("LANG", PROBE_LOCALE)
        .output()
        .map_err(|error| format!("cannot run ({:?})", error.kind()))?;
    if output.status.success() {
        Ok(canonical)
    } else {
        Err(format!(
            "capability probe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn effective_uid() -> u32 {
    nix::unistd::Uid::effective().as_raw()
}

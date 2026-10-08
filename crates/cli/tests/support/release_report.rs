//! The consumer report: the one file a successful run leaves behind.
//!
//! The report is the interface to the release attestation. It is written only
//! after every check passed, through a temporary file renamed into place, so
//! a reader never sees a partial report; a stale report is removed before a
//! run starts so a failed run leaves none.

// Rust guideline compliant 2026-10-08

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::release_env::BINARIES;
use crate::release_layout::Layout;

/// Schema version of the report document.
pub(crate) const REPORT_SCHEMA: u64 = 1;

/// The release matrix this executable was built against. Its suite version
/// binds every report to the suite code compiled into this build.
const MATRIX: &str = include_str!("../../../../compat/matrix.json");

/// Suite version of the compiled-in matrix.
fn suite_version() -> u64 {
    serde_json::from_str::<Value>(MATRIX)
        .ok()
        .and_then(|matrix| matrix["suite_version"].as_u64())
        .expect("compat/matrix.json carries a numeric suite_version")
}

/// Removes a report left by an earlier run.
///
/// # Panics
///
/// Panics when an existing report cannot be removed, since a stale report
/// would then be mistaken for the result of this run.
pub(crate) fn remove_stale(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("cannot remove the stale report {}: {error}", path.display()),
    }
}

/// Removes the stale report named by `value` when it is an absolute path;
/// anything else is left to the input validation to reject.
pub(crate) fn remove_stale_if_absolute(value: Option<std::ffi::OsString>) {
    if let Some(path) = value.map(std::path::PathBuf::from) {
        if path.is_absolute() {
            remove_stale(&path);
        }
    }
}

/// The report document of a successful run.
pub(crate) fn document(
    runtime: &str,
    package_digest: &str,
    upstream_version: &str,
    layout: &Layout,
) -> Value {
    let executables: serde_json::Map<String, Value> = BINARIES
        .iter()
        .map(|name| ((*name).to_owned(), json!(layout.digest(name))))
        .collect();
    json!({
        "schema": REPORT_SCHEMA,
        "runtime": runtime,
        "package_digest": package_digest,
        "upstream_version": upstream_version,
        "suite_version": suite_version(),
        "executables": executables,
    })
}

/// Mode of the report file; the attestation tooling runs as the same user, and
/// the report holds digests only.
const REPORT_MODE: u32 = 0o644;

/// Writes `document` to `path` atomically.
///
/// The bytes go to a fresh, uniquely named file created exclusively next to
/// `path` (never following a link, never reusing an existing file), are synced
/// through the retained descriptor, and are renamed into place. A file or link
/// that already sits at some `<report>.partial` is therefore never opened.
///
/// # Panics
///
/// Panics when the report cannot be written.
pub(crate) fn write(path: &Path, document: &Value) {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    let directory = path.parent().expect("the report has a directory");
    let name = path
        .file_name()
        .expect("the report has a file name")
        .to_string_lossy()
        .into_owned();
    let mut text = serde_json::to_string_pretty(document).expect("serialize the report");
    text.push('\n');
    let fail = |error: &dyn std::fmt::Display| -> ! {
        panic!("cannot write the report {}: {error}", path.display())
    };
    let mut file = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".partial")
        .tempfile_in(directory)
        .unwrap_or_else(|error| fail(&error));
    file.write_all(text.as_bytes())
        .and_then(|()| file.as_file().sync_all())
        .and_then(|()| {
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(REPORT_MODE))
        })
        .unwrap_or_else(|error| fail(&error));
    file.persist(path).unwrap_or_else(|error| fail(&error));
}

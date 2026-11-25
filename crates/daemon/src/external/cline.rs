//! The `cline-session-v1` transcript parser.
//!
//! A session manifest is `<root>/<id>/<id>.json`, one pretty-printed JSON
//! object that names the session in `session_id` and its working directory in
//! `cwd`. The directory also holds `<id>.messages.json` and one
//! `<agent>.messages.json` per subagent; none of them is a session.

// Rust guideline compliant 2026-11-25

use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use protocol::RuntimeRef;
use serde_json::Value;

use super::{
    is_absent_error, normalize_path, unattributed_candidate, TranscriptCandidate,
    TRANSCRIPT_SCAN_BYTE_LIMIT,
};

/// Separator Cline puts in the id of a subagent session.
const SUBAGENT_ID_MARKER: &str = "__";

/// Parses the manifest at `path`, which must lie at exactly `<id>/<id>.json`
/// below `root`.
///
/// Returns no candidate for a file elsewhere in the tree, a manifest that does
/// not fit the read bound or is not a JSON object, a `session_id` that is not
/// the `<id>` directory name, and an id that names a subagent. A `cwd` that is
/// not an absolute path leaves the candidate without a working directory.
pub(super) fn parse_session(
    agent_base: RuntimeRef,
    root: &Path,
    path: &Path,
) -> io::Result<Option<TranscriptCandidate>> {
    let Some(directory_id) = session_directory_id(root, path) else {
        return Ok(None);
    };
    if directory_id.contains(SUBAGENT_ID_MARKER) {
        return Ok(None);
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if is_absent_error(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let byte_limit =
        u64::try_from(TRANSCRIPT_SCAN_BYTE_LIMIT).expect("transcript byte limit fits in u64");
    // One byte past the bound tells a manifest that fits from one that does not.
    let mut manifest = Vec::new();
    file.take(byte_limit + 1).read_to_end(&mut manifest)?;
    if manifest.len() > TRANSCRIPT_SCAN_BYTE_LIMIT {
        return Ok(None);
    }
    let Ok(value) = serde_json::from_slice::<Value>(&manifest) else {
        return Ok(None);
    };
    if value.get("session_id").and_then(Value::as_str) != Some(directory_id) {
        return Ok(None);
    }
    let cwd = value
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|cwd| cwd.is_absolute())
        .map(|cwd| normalize_path(&cwd));
    Ok(Some(unattributed_candidate(
        agent_base,
        Some(directory_id.to_owned()),
        path.to_string_lossy().into_owned(),
        cwd,
        path,
    )))
}

/// The `<id>` directory name of a manifest at `<id>/<id>.json` below `root`,
/// or `None` for any other path.
fn session_directory_id<'p>(root: &Path, path: &'p Path) -> Option<&'p str> {
    let names: Vec<&str> = path
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let [id, file] = names[..] else {
        return None;
    };
    (file.strip_suffix(".json") == Some(id) && !id.is_empty()).then_some(id)
}

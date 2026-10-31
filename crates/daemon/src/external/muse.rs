//! The `muse-session-v1` transcript parser.
//!
//! A session log is `<root>/YYYY/MM/DD/<uuid>/session.jsonl`. Each line is a
//! framed transaction (`{"retained_frame": …, "children": [{"record_json":
//! "<escaped record>"}]}`) or a bare record. The first record whose
//! `payload_type` is `runtime.session.metadata` names the session in
//! `stream.id` and its workspace in `payload.record.workspace_root`.

// Rust guideline compliant 2026-10-27

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};

use protocol::RuntimeRef;
use serde_json::Value;
use uuid::Uuid;

use super::{
    is_absent_error, normalize_path, unattributed_candidate, TranscriptCandidate,
    TRANSCRIPT_SCAN_BYTE_LIMIT, TRANSCRIPT_SCAN_LINE_LIMIT,
};

/// File name of a session log.
const SESSION_FILE: &str = "session.jsonl";
/// `payload_type` of the record that binds a session to its workspace.
const METADATA_PAYLOAD_TYPE: &str = "runtime.session.metadata";
/// Digits of the year, month and day directories.
const DATE_DIGITS: [usize; 3] = [4, 2, 2];

/// Parses the session log at `path`, which must lie at exactly
/// `YYYY/MM/DD/<uuid>/session.jsonl` below `root`.
///
/// Returns no candidate for a log elsewhere in the tree (a subagent log nested
/// below a session, say), a log without a metadata record within the bounded
/// head, or a record whose `stream.id` is not the `<uuid>` directory name. A
/// workspace that is not an absolute path leaves the candidate without a
/// working directory.
pub(super) fn parse_session(
    agent_base: RuntimeRef,
    root: &Path,
    path: &Path,
) -> io::Result<Option<TranscriptCandidate>> {
    let Some(directory_id) = session_directory_id(root, path) else {
        return Ok(None);
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if is_absent_error(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let byte_limit =
        u64::try_from(TRANSCRIPT_SCAN_BYTE_LIMIT).expect("transcript byte limit fits in u64");
    for line in BufReader::new(file.take(byte_limit))
        .split(b'\n')
        .take(TRANSCRIPT_SCAN_LINE_LIMIT)
    {
        let line = line?;
        // A line that is not valid UTF-8, or that the byte limit cut inside a
        // multibyte character, is unparseable like any other bad line.
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let Some(record) = metadata_record(&value) else {
            continue;
        };
        let id = record
            .pointer("/stream/id")
            .and_then(Value::as_str)
            .filter(|id| *id == directory_id);
        let Some(id) = id else {
            return Ok(None);
        };
        let cwd = record
            .pointer("/payload/record/workspace_root")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .filter(|workspace| workspace.is_absolute())
            .map(|workspace| normalize_path(&workspace));
        return Ok(Some(unattributed_candidate(
            agent_base,
            Some(id.to_owned()),
            path.to_string_lossy().into_owned(),
            cwd,
            path,
        )));
    }
    Ok(None)
}

/// The `<uuid>` directory name of a session log at the dated depth below
/// `root`, or `None` for any other path.
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
    let [year, month, day, id, file] = names[..] else {
        return None;
    };
    let dated = [year, month, day]
        .into_iter()
        .zip(DATE_DIGITS)
        .all(|(name, digits)| name.len() == digits && name.bytes().all(|b| b.is_ascii_digit()));
    (dated && file == SESSION_FILE && is_canonical_uuid(id)).then_some(id)
}

/// The first metadata record of `line`: the line itself when it is a bare
/// record, else the first child of a framed transaction that is one.
fn metadata_record(line: &Value) -> Option<Value> {
    let is_metadata = |record: &Value| {
        record.get("payload_type").and_then(Value::as_str) == Some(METADATA_PAYLOAD_TYPE)
    };
    if is_metadata(line) {
        return Some(line.clone());
    }
    line.get("children")?
        .as_array()?
        .iter()
        .filter_map(|child| child.get("record_json")?.as_str())
        .filter_map(|json| serde_json::from_str::<Value>(json).ok())
        .find(is_metadata)
}

/// Whether `id` is a UUID in its lower-case hyphenated form, the only spelling
/// a session directory has.
fn is_canonical_uuid(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|uuid| uuid.hyphenated().to_string() == id)
}

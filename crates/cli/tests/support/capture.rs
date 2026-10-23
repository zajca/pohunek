//! Capture helpers shared by the real-agent package tests.
//!
//! A capture writes the visible rows and the terminal title of a session as
//! `<name>.txt` / `<name>.title`, and the ordered OSC 7501 root records of its
//! retained raw output as `<name>.program-status`. The helpers are plain
//! functions over rows and bytes, so each package keeps its own fixture.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-13

use std::fs;
use std::path::Path;

use base64::Engine as _;
use pohunek_terminal::{parse_program_status, ProgramStatusReport, PROGRAM_STATUS_COMMAND};
use serde_json::Value;

/// Terminal widths every captured state is also recorded at.
pub(crate) const CAPTURE_WIDTHS: [u16; 9] = [20, 30, 40, 60, 80, 100, 120, 160, 200];

/// Rows of every captured screen.
pub(crate) const CAPTURE_ROWS: &str = "24";

/// Replacement for the random part of the throwaway root in captured text, so
/// the files are reproducible.
// hermetic-allowed: #146 the text is a fixed replacement for the random fixture root in captured screens, never a host path
pub(crate) const NEUTRAL_ROOT: &str = "/tmp/ph-sample";

/// Directory below which a recording launches its sessions.
///
/// Agents print their working directory in a footer or a question and clip or
/// wrap it at narrow widths, so a random fixture root would leak a host path
/// into the captures that [`write_capture`] cannot neutralise. A recording
/// launches its sessions in directories of this fixed, short root instead. It
/// is `/dev/shm/ph-<name>` and is removed when the value is dropped.
pub(crate) struct RecordingRoot {
    path: std::path::PathBuf,
}

impl RecordingRoot {
    /// Creates an empty root for the recording of runtime `name`.
    ///
    /// # Panics
    ///
    /// Panics when the directory cannot be created.
    // hermetic-allowed: #1525 a fixed short path under tmpfs keeps recorded screens reproducible; recording is an opt-in run, never part of the gates
    pub(crate) fn new(name: &str) -> Self {
        let path = std::path::PathBuf::from(format!("/dev/shm/ph-{name}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create the recording root");
        Self { path }
    }

    /// A fresh directory `name` below the root.
    ///
    /// # Panics
    ///
    /// Panics when the directory cannot be created.
    pub(crate) fn directory(&self, name: &str) -> std::path::PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path).expect("create a recording directory");
        path
    }
}

impl Drop for RecordingRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Extension of the program-status sequence golden.
pub(crate) const PROGRAM_STATUS_EXTENSION: &str = "program-status";

/// Writes one captured screen: `<name>.txt` holds the rows and `<name>.title`
/// the terminal title when the agent had set one. `root` is replaced by
/// [`NEUTRAL_ROOT`] in both.
pub(crate) fn write_capture(
    directory: &Path,
    name: &str,
    root: &str,
    title: Option<&str>,
    rows: &[String],
) {
    fs::create_dir_all(directory).expect("create the capture directory");
    let text = rows.iter().fold(String::new(), |mut text, row| {
        text.push_str(&row.replace(root, NEUTRAL_ROOT));
        text.push('\n');
        text
    });
    fs::write(directory.join(format!("{name}.txt")), text).expect("write the screen");
    if let Some(title) = title {
        fs::write(
            directory.join(format!("{name}.title")),
            format!("{}\n", title.replace(root, NEUTRAL_ROOT)),
        )
        .expect("write the title");
    }
}

/// The raw bytes of a `session output --json` result (its `ok` object or the
/// whole document).
///
/// # Panics
///
/// Panics when the document holds no valid `data_base64`.
pub(crate) fn output_bytes(result: &Value) -> Vec<u8> {
    let ok = result.get("ok").unwrap_or(result);
    let encoded = ok["data_base64"]
        .as_str()
        .unwrap_or_else(|| panic!("a session output result: {result}"));
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("standard base64 output")
}

/// The ordered root program-status records of raw terminal output, one per
/// line, consecutive repeats removed.
///
/// A root record reads `app=<app>:state=<state>[:kind=<kind>]`, a root clear
/// reads `clear`. Probes, child (`id=`) records and discarded reports are
/// left out, and so are `msg`, `title` and `progress`, which the parser never
/// returns. Both terminators (BEL and `ESC \`) are accepted. Timing-dependent
/// frames are not part of the result.
pub(crate) fn program_status_records(raw: &[u8]) -> Vec<String> {
    let mut prefix = b"\x1b]".to_vec();
    prefix.extend_from_slice(PROGRAM_STATUS_COMMAND);
    prefix.push(b';');
    let mut records: Vec<String> = Vec::new();
    let mut rest = raw;
    while let Some(start) = find(rest, &prefix) {
        let body = &rest[start + prefix.len()..];
        let Some((end, terminator)) = terminator(body) else {
            break;
        };
        let line = match parse_program_status(&body[..end]) {
            Some(ProgramStatusReport::Record(record)) => Some(record.canonical()),
            Some(ProgramStatusReport::Clear) => Some("clear".to_owned()),
            Some(ProgramStatusReport::Probe | ProgramStatusReport::Child) | None => None,
        };
        if let Some(line) = line {
            if records.last() != Some(&line) {
                records.push(line);
            }
        }
        rest = &body[end + terminator..];
    }
    records
}

/// Writes `<name>.program-status` below `directory`: the records of
/// [`program_status_records`], one per line.
pub(crate) fn write_program_status(directory: &Path, name: &str, raw: &[u8]) {
    fs::create_dir_all(directory).expect("create the capture directory");
    let text = program_status_records(raw)
        .iter()
        .fold(String::new(), |mut text, line| {
            text.push_str(line);
            text.push('\n');
            text
        });
    fs::write(
        directory.join(format!("{name}.{PROGRAM_STATUS_EXTENSION}")),
        text,
    )
    .expect("write the program-status golden");
}

/// Offset of the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Offset and length of the first OSC terminator (BEL or `ESC \`) in `body`.
fn terminator(body: &[u8]) -> Option<(usize, usize)> {
    body.iter()
        .enumerate()
        .find_map(|(index, byte)| match byte {
            0x07 => Some((index, 1)),
            0x1b if body.get(index + 1) == Some(&b'\\') => Some((index, 2)),
            _ => None,
        })
}

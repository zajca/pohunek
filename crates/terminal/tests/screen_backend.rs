//! The screen-backend seam of `TerminalTracker` (issue #2538), driven through
//! the crate boundary with the recorded PTY traces committed under
//! `tests/fixtures/` (copies of the spike fixtures, which stay the source under
//! `spikes/libghostty-vt/fixtures/`).
//!
//! Scenarios S2-S4 of the plan run against both models in one build:
//! `with_model(ModelKind::Vt100)` and `with_model(ModelKind::Libghostty)`. The
//! Ghostty halves need the `libghostty` feature, so this file compiles without
//! it and skips them there.

// Rust guideline compliant 2026-11-27

use pohunek_terminal::{ModelKind, TerminalSnapshot, TerminalTracker};

/// First seven bytes of every trace file.
const MAGIC: &[u8; 7] = b"PHTRACE";
/// The trace format version this reader accepts.
const VERSION: u16 = 1;
/// Record tag of an `Output` record.
const TAG_OUTPUT: u8 = 1;
/// Record tag of a `Resize` record.
const TAG_RESIZE: u8 = 2;

// Bounds the reader enforces, mirroring the spike's `config/model.toml`, so a
// malformed fixture fails the scenario instead of panicking on a slice.
/// Largest trace file read.
const MAX_TRACE_BYTES: usize = 32 * 1024 * 1024;
/// Largest `Output` record read.
const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// Most records one trace may hold.
const MAX_RECORDS: usize = 1_000_000;
/// Largest row or column count.
const MAX_DIMENSION: u16 = 500;

/// One entry of a trace.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Record {
    /// One PTY read chunk.
    Output(Vec<u8>),
    /// The PTY was resized.
    Resize { rows: u16, cols: u16 },
}

/// A parsed trace: the initial size and the ordered records.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Trace {
    rows: u16,
    cols: u16,
    records: Vec<Record>,
}

/// Cursor over the trace input that panics with the failing offset.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, len: usize, part: &'static str) -> &[u8] {
        let end = self.at + len;
        assert!(
            end <= self.bytes.len(),
            "trace ends inside {part} at {}",
            self.at
        );
        let slice = &self.bytes[self.at..end];
        self.at = end;
        slice
    }

    fn u16(&mut self, part: &'static str) -> u16 {
        u16::from_le_bytes(self.take(2, part).try_into().expect("two bytes"))
    }

    fn u32(&mut self, part: &'static str) -> u32 {
        u32::from_le_bytes(self.take(4, part).try_into().expect("four bytes"))
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// Parses a trace file; a malformed input panics with the failing offset.
fn parse_trace(bytes: &[u8]) -> Trace {
    assert!(
        bytes.len() <= MAX_TRACE_BYTES,
        "trace over {MAX_TRACE_BYTES} bytes"
    );
    let mut reader = Reader { bytes, at: 0 };
    assert_eq!(reader.take(MAGIC.len(), "the magic"), MAGIC, "bad magic");
    assert_eq!(
        reader.u16("the version"),
        VERSION,
        "unsupported trace version"
    );
    let rows = reader.u16("the initial rows");
    let cols = reader.u16("the initial cols");
    assert!((1..=MAX_DIMENSION).contains(&rows) && (1..=MAX_DIMENSION).contains(&cols));
    let mut records = Vec::new();
    while !reader.done() {
        assert!(records.len() < MAX_RECORDS, "too many records");
        let start = reader.at;
        let tag = reader.take(1, "a record tag")[0];
        records.push(match tag {
            TAG_OUTPUT => {
                let len = reader.u32("an output length") as usize;
                assert!(len <= MAX_RECORD_BYTES, "output record of {len} bytes");
                Record::Output(reader.take(len, "an output record").to_vec())
            }
            TAG_RESIZE => Record::Resize {
                rows: reader.u16("a resize record"),
                cols: reader.u16("a resize record"),
            },
            tag => panic!("unknown record tag {tag} at {start}"),
        });
    }
    Trace {
        rows,
        cols,
        records,
    }
}

/// The committed `.trace` fixtures, sorted by file name.
fn fixture_traces() -> Vec<(String, Trace)> {
    let dir = pohunek_test_support::manifest_dir().join("tests/fixtures");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("the committed fixture directory reads") {
        let path = entry.expect("a fixture directory entry").path();
        if path.extension().is_some_and(|ext| ext == "trace") {
            let name = path
                .file_name()
                .expect("a fixture file name")
                .to_string_lossy()
                .into_owned();
            let bytes = std::fs::read(&path).expect("a fixture reads");
            out.push((name, parse_trace(&bytes)));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        !out.is_empty(),
        "no .trace fixtures under {}",
        dir.display()
    );
    out
}

/// One committed fixture by file name.
fn fixture(name: &str) -> Trace {
    let dir = pohunek_test_support::manifest_dir().join("tests/fixtures");
    let bytes = std::fs::read(dir.join(name)).expect("the fixture reads");
    parse_trace(&bytes)
}

/// Replays a trace through one model, in the recorded chunk order.
fn replay(model: ModelKind, trace: &Trace) -> TerminalTracker {
    let mut tracker = TerminalTracker::with_model(model, trace.rows, trace.cols);
    for record in &trace.records {
        match record {
            Record::Output(bytes) => tracker.feed(bytes),
            Record::Resize { rows, cols } => tracker.resize(*rows, *cols),
        }
    }
    tracker
}

/// The screen state a scenario compares between models.
fn screen_state(snapshot: &TerminalSnapshot) -> (u16, u16, u16, u16, bool, bool, &str) {
    (
        snapshot.rows,
        snapshot.cols,
        snapshot.cursor_row,
        snapshot.cursor_col,
        snapshot.cursor_visible,
        snapshot.alternate_screen,
        &snapshot.visible_text,
    )
}

/// S3: `with_model` runs the requested model, and `new` runs the default of
/// the build (vt100 without the `libghostty` feature, libghostty with it).
#[test]
fn with_model_selects_the_requested_backend() {
    let tracker = TerminalTracker::with_model(ModelKind::Vt100, 4, 20);
    assert_eq!(tracker.model(), ModelKind::Vt100);

    #[cfg(feature = "libghostty")]
    {
        let tracker = TerminalTracker::with_model(ModelKind::Libghostty, 4, 20);
        assert_eq!(tracker.model(), ModelKind::Libghostty);
        assert_eq!(TerminalTracker::new(4, 20).model(), ModelKind::Libghostty);
    }
    #[cfg(not(feature = "libghostty"))]
    assert_eq!(TerminalTracker::new(4, 20).model(), ModelKind::Vt100);
}

/// Fixtures whose visible text the two models are known to report
/// differently (issue #2538 known gaps): a resize reflows the vt100 grid
/// differently than libghostty, libghostty retains scrollback rows that the
/// vt100 backend (scrollback 0) has already dropped, and the two models mark
/// a soft wrap differently when a wide char sits in the last column.
#[cfg(feature = "libghostty")]
const KNOWN_VISIBLE_TEXT_DIFFERENCES: &[&str] =
    &["resize-wrap.trace", "scrollback.trace", "wide-chars.trace"];

/// S2: every fixture reports the same screen geometry and cursor on both
/// models.
#[test]
#[cfg(feature = "libghostty")]
fn every_fixture_reports_the_same_geometry_and_cursor_on_both_models() {
    for (name, trace) in fixture_traces() {
        let vt100 = replay(ModelKind::Vt100, &trace).snapshot(0);
        let ghostty = replay(ModelKind::Libghostty, &trace).snapshot(0);
        assert_eq!(
            (
                vt100.rows,
                vt100.cols,
                vt100.cursor_row,
                vt100.cursor_col,
                vt100.cursor_visible,
                vt100.alternate_screen,
            ),
            (
                ghostty.rows,
                ghostty.cols,
                ghostty.cursor_row,
                ghostty.cursor_col,
                ghostty.cursor_visible,
                ghostty.alternate_screen,
            ),
            "{name}: the two models disagree on the screen geometry or cursor"
        );
    }
}

/// S2: the visible text matches on both models, except the fixtures whose
/// difference is known and pinned below.
#[test]
#[cfg(feature = "libghostty")]
fn visible_text_matches_on_both_models_except_the_pinned_known_differences() {
    for (name, trace) in fixture_traces() {
        if KNOWN_VISIBLE_TEXT_DIFFERENCES.contains(&name.as_str()) {
            continue;
        }
        let vt100 = replay(ModelKind::Vt100, &trace).snapshot(0);
        let ghostty = replay(ModelKind::Libghostty, &trace).snapshot(0);
        assert_eq!(
            vt100.visible_text, ghostty.visible_text,
            "{name}: the two models disagree on the visible text"
        );
    }
}

/// S2 (pinned known differences): the visible text of `resize-wrap` and
/// `scrollback` is exactly what each model reports today, so a pin bump that
/// changes either fails here instead of silently changing snapshots.
#[test]
#[cfg(feature = "libghostty")]
fn the_known_visible_text_differences_are_the_pinned_ones() {
    let scrollback_history: String = (1..=15)
        .map(|line| format!("line{line:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let cases: &[(&str, &str, String)] = &[
        (
            "resize-wrap.trace",
            "after-shri\nnk\nafter-grow",
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123\nafter-shrink\nafter-grow".to_owned(),
        ),
        (
            "scrollback.trace",
            "line13\nline14\nline15\ndone",
            format!("{scrollback_history}\ndone"),
        ),
        (
            "wide-chars.trace",
            "ab\u{4e2d}\u{6587}cd\nabcdefghi\n\u{4e2d}x",
            "ab\u{4e2d}\u{6587}cd\nabcdefghi\u{4e2d}x".to_owned(),
        ),
    ];
    for (name, vt100_text, ghostty_text) in cases {
        let trace = fixture(name);
        let vt100 = replay(ModelKind::Vt100, &trace).snapshot(0);
        let ghostty = replay(ModelKind::Libghostty, &trace).snapshot(0);
        assert_eq!(vt100.visible_text, *vt100_text, "{name}: vt100 text moved");
        assert_eq!(
            ghostty.visible_text, *ghostty_text,
            "{name}: libghostty text moved"
        );
    }
}

/// Visible text both models report for the `emoji-width` fixture (the family
/// emoji, the VS16/VS15 hearts and the flag, each followed by `x`).
#[cfg(feature = "libghostty")]
const EMOJI_WIDTH_TEXT: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}x\n\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}x\n\u{2764}\u{fe0f}x\n\u{2764}\u{fe0f}x\n\u{2764}\u{fe0e}x\n\u{1f1e8}\u{1f1ff}x\n\u{1f1e8}\u{1f1ff}x";

/// S2 (pinned known gap): on the `emoji-width` fixture the screen state is
/// identical on both models, and only the repaint bytes differ — the spike
/// pinned the 17 cell diffs of the mode-2027 rows (0, 3, 5) that a text
/// repaint cannot carry (issue #2538 known gaps).
#[test]
#[cfg(feature = "libghostty")]
fn the_emoji_width_known_gap_stays_pinned() {
    let trace = fixture("emoji-width.trace");
    let vt100 = replay(ModelKind::Vt100, &trace).snapshot(0);
    let ghostty = replay(ModelKind::Libghostty, &trace).snapshot(0);
    assert_eq!(screen_state(&vt100), screen_state(&ghostty));
    assert_eq!(
        screen_state(&vt100),
        (8, 20, 7, 0, true, false, EMOJI_WIDTH_TEXT)
    );
    assert_ne!(
        vt100.ansi, ghostty.ansi,
        "the repaint difference must stay visible"
    );
}

/// S2: the ansi repaint of each model recreates its own screen when fed to a
/// fresh tracker of the same model (the repaint round trip).
#[test]
fn the_ansi_repaint_round_trips_through_a_fresh_tracker_of_the_same_model() {
    for (name, trace) in fixture_traces() {
        let model = ModelKind::Vt100;
        let snapshot = replay(model, &trace).snapshot(0);
        assert!(
            snapshot.ansi.starts_with(b"\x1b[?2026h"),
            "{name}: the repaint opens the synchronized-output bracket"
        );
        let mut fresh = TerminalTracker::with_model(model, snapshot.rows, snapshot.cols);
        fresh.feed(&snapshot.ansi);
        let replayed = fresh.snapshot(0);
        assert_eq!(
            screen_state(&snapshot),
            screen_state(&replayed),
            "{name}: the vt100 repaint does not recreate its screen"
        );
    }
}

/// S2: same round trip through the libghostty backend; its repaint body is
/// `format_vt(unwrap = true, trim = false)` with an empty prefix.
#[test]
#[cfg(feature = "libghostty")]
fn the_ghostty_ansi_repaint_round_trips_through_a_fresh_ghostty_tracker() {
    for (name, trace) in fixture_traces() {
        let snapshot = replay(ModelKind::Libghostty, &trace).snapshot(0);
        assert!(
            snapshot.ansi.starts_with(b"\x1b[?2026h"),
            "{name}: the repaint opens the synchronized-output bracket"
        );
        let mut fresh =
            TerminalTracker::with_model(ModelKind::Libghostty, snapshot.rows, snapshot.cols);
        fresh.feed(&snapshot.ansi);
        let replayed = fresh.snapshot(0);
        assert_eq!(
            screen_state(&snapshot),
            screen_state(&replayed),
            "{name}: the libghostty repaint does not recreate its screen"
        );
    }
}

/// S4: OSC metadata (title, progress) and the OSC 7501 probe spans are
/// identical across the backends, because the `OscTracker` is shared and
/// byte-driven. The `program-status` and `title-progress` fixtures carry the
/// probes, the title and the progress payload.
#[test]
#[cfg(feature = "libghostty")]
fn osc_metadata_and_probes_are_identical_across_backends() {
    for name in ["program-status.trace", "title-progress.trace"] {
        let trace = fixture(name);
        let mut vt100 = replay(ModelKind::Vt100, &trace);
        let mut ghostty = replay(ModelKind::Libghostty, &trace);
        let (a, b) = (vt100.snapshot(7), ghostty.snapshot(7));
        assert_eq!(a.title, b.title, "{name}: title differs");
        assert_eq!(a.progress, b.progress, "{name}: progress differs");
        assert_eq!(
            vt100.take_probes(),
            ghostty.take_probes(),
            "{name}: OSC 7501 probe spans differ"
        );
        assert_eq!(
            a.visible_text, b.visible_text,
            "{name}: visible text differs between the models"
        );
    }
}

/// S4: the OSC metadata and probes are delivered on the default model too,
/// with the values the fixtures were recorded to produce (their
/// `*.expect.toml` sidecars in the spike state the same). Runs without the
/// `libghostty` feature, so the red-on-old rows select it there.
#[test]
fn osc_metadata_and_probes_are_delivered_on_the_default_model() {
    let mut tracker = replay(ModelKind::Vt100, &fixture("title-progress.trace"));
    let snapshot = tracker.snapshot(0);
    assert_eq!(snapshot.title.as_deref(), Some("title two"));
    assert_eq!(snapshot.progress.as_deref(), Some("4;1;50"));
    assert!(tracker.take_probes().is_empty());

    let mut tracker = replay(ModelKind::Vt100, &fixture("program-status.trace"));
    let snapshot = tracker.snapshot(0);
    assert!(
        !tracker.take_probes().is_empty(),
        "the probes of the fixture are delivered"
    );
    assert_eq!(snapshot.visible_text, "visible");
}

//! A terminal snapshot is built on demand and stays at its ring watermark.
//!
//! The hub serves subscribers, resyncs and snapshot requests, so a scenario
//! that only reads pushes and then asks for a repaint exercises the path where
//! no consumer observed the output while it was produced. Each state change
//! (resize, output) is followed by its own repaint request, so a repaint left
//! over from before that change is visible in the result.

// Rust guideline compliant 2026-06-26

use pohunek_session_worker::{wire_terminal, OutputEvent, OutputHub};
use pohunek_terminal::{ScreenModelId, TerminalSnapshot, CURRENT_FORMAT_VERSION};
use pohunek_worker_protocol::{
    Cursor, Dimensions, ScreenModelId as WireScreenModelId, TerminalLink,
    TerminalSnapshot as WireTerminalSnapshot,
};

/// The screen model the hub's tracker runs in this build: the default model
/// of the build (issue #2538 P3a keeps the flip off, so vt100 unless the
/// `libghostty` feature is on).
fn default_model() -> ScreenModelId {
    if cfg!(feature = "libghostty") {
        ScreenModelId::Libghostty
    } else {
        ScreenModelId::Vt100
    }
}

/// Asserts the versioning fields of a hub snapshot (issue #2538 P3a): the
/// screen model that produced it and the current wire format version.
fn assert_versioning(snapshot: &TerminalSnapshot) {
    assert_eq!(
        snapshot.model,
        default_model(),
        "the snapshot must name the model that produced it"
    );
    assert_eq!(
        snapshot.format_version, CURRENT_FORMAT_VERSION,
        "the snapshot must carry the current wire format version"
    );
}

/// Maps a tracker screen model onto the worker wire type in a test.
fn wire_model(model: ScreenModelId) -> WireScreenModelId {
    match model {
        ScreenModelId::Vt100 => WireScreenModelId::Vt100,
        ScreenModelId::Libghostty => WireScreenModelId::Libghostty,
    }
}

/// A small tracker snapshot of the given model, for the wire rows.
fn tracked_snapshot(model: ScreenModelId) -> TerminalSnapshot {
    TerminalSnapshot {
        watermark: 7,
        rows: 24,
        cols: 80,
        cursor_row: 1,
        cursor_col: 2,
        cursor_visible: true,
        alternate_screen: false,
        model,
        format_version: CURRENT_FORMAT_VERSION,
        title: None,
        progress: None,
        visible_text: "screen".to_owned(),
        ansi: b"repaint".to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unobserved_reads_leave_the_repaint_at_their_watermark() {
    let hub = OutputHub::new(1024, 4096, 24, 80).expect("hub");
    hub.push(b"\x1b[2J\x1b[Hone").expect("push first line");
    hub.push(b"\r\ntwo").expect("push second line");

    let first = hub.terminal_snapshot();
    assert_versioning(&first);
    assert_eq!(first.watermark, hub.next_offset());
    assert_eq!((first.rows, first.cols), (24, 80));
    assert!(first.visible_text.contains("one"), "{first:?}");
    assert!(first.visible_text.contains("two"), "{first:?}");

    hub.resize(12, 60);
    let resized = hub.terminal_snapshot();
    assert_versioning(&resized);
    assert_eq!(
        (resized.rows, resized.cols),
        (12, 60),
        "a repaint after resize must have the new size"
    );
    assert_eq!(resized.watermark, first.watermark);
    assert!(resized.visible_text.contains("two"), "{resized:?}");

    hub.push(b"\r\nthree").expect("push after resize");
    let second = hub.terminal_snapshot();
    assert_versioning(&second);
    assert_eq!(
        second.watermark,
        hub.next_offset(),
        "a repaint after output must be at the new ring end"
    );
    assert!(
        second.watermark > resized.watermark,
        "{resized:?} {second:?}"
    );
    assert_eq!((second.rows, second.cols), (12, 60));
    assert!(second.visible_text.contains("three"), "{second:?}");

    assert_eq!(hub.terminal_snapshot().ansi, second.ansi);

    hub.push(b"\r\nfour").expect("push before subscribe");
    let mut subscriber = hub.subscribe_terminal_snapshot();
    let Some(OutputEvent::TerminalSnapshot(chunk)) = subscriber.recv().await else {
        panic!("a terminal snapshot subscriber must begin with the repaint");
    };
    assert_eq!(chunk.terminal().watermark, hub.next_offset());
    assert!(chunk.terminal().visible_text.contains("four"));
}

const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

/// Collects the repaint fragments of a subscriber that starts with a snapshot or gap.
async fn collect_repaint(subscriber: &mut pohunek_session_worker::OutputSubscriber) -> Vec<u8> {
    let mut repaint = Vec::new();
    loop {
        match pohunek_test_support::wait::guard("the next repaint event", subscriber.recv()).await {
            Some(OutputEvent::Gap { .. }) => {}
            Some(OutputEvent::TerminalSnapshot(chunk)) => {
                repaint.extend_from_slice(&chunk.bytes);
                if repaint.len() == chunk.total_bytes {
                    return repaint;
                }
            }
            other => panic!("expected a repaint, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gap_repaint_split_into_bounded_events_is_one_synchronized_bracket() {
    // A history of 8 bytes evicts the early output, and a subscriber queue of
    // 64 bytes splits the full-screen repaint into many events.
    let hub = OutputHub::new(8, 64, 24, 80).expect("hub");
    hub.push(b"\x1b[2J\x1b[H").expect("clear");
    for row in 0..24 {
        hub.push(format!("\x1b[{};1Hrow {row} {}", row + 1, "x".repeat(60)).as_bytes())
            .expect("fill row");
    }
    let mut subscriber = hub.subscribe(None).expect("gap subscribe");

    let repaint = collect_repaint(&mut subscriber).await;

    assert_versioning(&hub.terminal_snapshot());
    assert!(repaint.len() > 64 * 4, "repaint must span several events");
    assert!(repaint.starts_with(SYNC_BEGIN), "{repaint:?}");
    assert!(repaint.ends_with(SYNC_END), "{repaint:?}");
    assert_eq!(count(&repaint, SYNC_BEGIN), 1);
    assert_eq!(count(&repaint, SYNC_END), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_repaint_is_one_bracket_on_the_alternate_screen() {
    let hub = OutputHub::new(1024, 4096, 4, 20).expect("hub");
    hub.push(b"\x1b[?1049h\x1b[Halt").expect("alternate screen");
    let mut subscriber = hub.subscribe_terminal_snapshot();

    let repaint = collect_repaint(&mut subscriber).await;

    let snapshot = hub.terminal_snapshot();
    assert_versioning(&snapshot);
    assert!(snapshot.alternate_screen, "{snapshot:?}");
    assert!(repaint.starts_with(SYNC_BEGIN), "{repaint:?}");
    assert!(repaint.ends_with(SYNC_END), "{repaint:?}");
    assert_eq!(count(&repaint, SYNC_BEGIN), 1);
    assert_eq!(count(&repaint, SYNC_END), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn program_set_synchronized_output_is_not_reset_by_the_repaint() {
    let hub = OutputHub::new(1024, 4096, 4, 20).expect("hub");
    // The set sequence is combined with another mode and split across two reads.
    hub.push(b"frame\x1b[?25h\x1b[?1;20").expect("first half");
    hub.push(b"26h more").expect("second half");
    let mut subscriber = hub.subscribe_terminal_snapshot();
    let repaint = collect_repaint(&mut subscriber).await;
    assert!(repaint.starts_with(SYNC_BEGIN), "{repaint:?}");
    assert_eq!(count(&repaint, SYNC_END), 0, "{repaint:?}");

    // Once the program ends its frame the repaint closes its own bracket again.
    hub.push(b"\x1b[?2026l").expect("program reset");
    let mut subscriber = hub.subscribe_terminal_snapshot();
    let repaint = collect_repaint(&mut subscriber).await;
    assert!(repaint.ends_with(SYNC_END), "{repaint:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_del_byte_inside_the_set_sequence_does_not_hide_it() {
    let hub = OutputHub::new(1024, 4096, 4, 20).expect("hub");
    hub.push(b"\x1b[?20\x7f26h").expect("set with a DEL inside");
    let mut subscriber = hub.subscribe_terminal_snapshot();
    let repaint = collect_repaint(&mut subscriber).await;
    assert!(repaint.starts_with(SYNC_BEGIN), "{repaint:?}");
    assert_eq!(count(&repaint, SYNC_END), 0, "{repaint:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mode_number_with_leading_zeros_is_the_same_mode() {
    let hub = OutputHub::new(1024, 4096, 4, 20).expect("hub");
    hub.push(b"\x1b[?002026h").expect("set with leading zeros");
    let mut subscriber = hub.subscribe_terminal_snapshot();
    let repaint = collect_repaint(&mut subscriber).await;
    assert_eq!(count(&repaint, SYNC_END), 0, "{repaint:?}");

    hub.push(b"\x1b[?02026l").expect("reset with leading zeros");
    let mut subscriber = hub.subscribe_terminal_snapshot();
    let repaint = collect_repaint(&mut subscriber).await;
    assert!(repaint.ends_with(SYNC_END), "{repaint:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_reset_clears_a_program_set_synchronized_output() {
    // `RIS`, and a reset that follows a stray `ESC`.
    for reset in [&b"\x1bc"[..], &b"\x1b\x1b[?2026l"[..]] {
        let hub = OutputHub::new(1024, 4096, 4, 20).expect("hub");
        hub.push(b"\x1b[?2026h").expect("program set");
        hub.push(reset).expect("reset");
        let mut subscriber = hub.subscribe_terminal_snapshot();
        let repaint = collect_repaint(&mut subscriber).await;
        assert!(repaint.ends_with(SYNC_END), "{reset:?} {repaint:?}");
    }
}

// ---------------------------------------------------------------------------
// Wire-compatibility rows 13-17 of the issue #2538 P3a case table: the
// snapshot's `model`/`format_version` fields are additive on the worker
// wire (worker protocol stays 9, window 8..=9), so no protocol bump.

/// Row 13 (new worker + new daemon): the wire snapshot carries both
/// versioning fields — `model = "libghostty"`, `format_version = 1` — and
/// round-trips through `serde_json` byte-identically.
#[test]
fn wire_snapshot_carries_model_and_format_version() {
    let wire = wire_terminal(&tracked_snapshot(ScreenModelId::Libghostty)).expect("conversion");
    assert_eq!(wire.model, Some(WireScreenModelId::Libghostty));
    assert_eq!(wire.format_version, Some(1));
    let json = serde_json::to_value(&wire).expect("serialize");
    assert_eq!(json["model"], "libghostty");
    assert_eq!(json["format_version"], 1);
    let decoded: WireTerminalSnapshot =
        serde_json::from_value(json.clone()).expect("decode the new wire shape");
    assert_eq!(decoded, wire, "the wire snapshot round-trips");
    assert_eq!(serde_json::to_value(&decoded).expect("serialize"), json);
}

/// Row 14 (new worker + N-1 daemon): a receiver pinned to the pre-P3 derive
/// — no `model`/`format_version` members and no `deny_unknown_fields`, like
/// every `TerminalSnapshot` in the 8..=9 window — decodes the new wire JSON,
/// ignoring the new fields.
#[test]
fn a_pre_p3_receiver_ignores_the_new_wire_fields() {
    /// Test-only copy of the pre-P3 `worker_protocol::TerminalSnapshot`
    /// derive (issue #2538 §3.2 direction A).
    #[derive(serde::Deserialize)]
    #[allow(
        dead_code,
        reason = "the fields exist so the decode pins the pre-P3 member list; serde reads them"
    )]
    struct PreP3TerminalSnapshot {
        watermark: u64,
        dimensions: Dimensions,
        cursor: Cursor,
        alternate_screen: bool,
        title: Option<String>,
        progress: Option<String>,
        visible_lines: Vec<String>,
        // `links` is skipped when empty on the wire (pre-existing attr).
        #[serde(default)]
        links: Vec<TerminalLink>,
    }

    let wire = wire_terminal(&tracked_snapshot(ScreenModelId::Libghostty)).expect("conversion");
    let json = serde_json::to_value(&wire).expect("serialize");
    assert!(json.get("model").is_some() && json.get("format_version").is_some());
    serde_json::from_value::<PreP3TerminalSnapshot>(json)
        .expect("the N-1 daemon's types decode the new snapshot tolerantly");
}

/// Row 15 (old worker + new daemon): a pre-P3 wire JSON — no `model`, no
/// `format_version` — decodes with both fields `None` and serializes back to
/// exactly the pre-P3 shape, so the daemon omits both from `session.screen`.
#[test]
fn a_pre_p3_wire_snapshot_decodes_with_none_and_keeps_its_shape() {
    let wire = wire_terminal(&tracked_snapshot(ScreenModelId::Vt100)).expect("conversion");
    let mut legacy = serde_json::to_value(&wire).expect("serialize");
    // Emulate an old worker: the fields are absent from its bytes.
    assert_eq!(
        legacy.as_object_mut().expect("object").remove("model"),
        Some(serde_json::json!("vt100"))
    );
    assert_eq!(
        legacy
            .as_object_mut()
            .expect("object")
            .remove("format_version"),
        Some(serde_json::json!(1))
    );
    let decoded: WireTerminalSnapshot =
        serde_json::from_value(legacy.clone()).expect("decode the pre-P3 shape");
    assert_eq!(decoded.model, None);
    assert_eq!(decoded.format_version, None);
    assert_eq!(
        serde_json::to_value(&decoded).expect("serialize"),
        legacy,
        "absent fields stay absent: an N-1 worker produces exactly today's reply shape"
    );
}

/// Row 16 (worker restarted across an upgrade, live PTY): the snapshot is
/// rebuilt from the live tracker and never persisted — a fresh hub (the
/// restarted worker) reports the versioning fields again from its own
/// tracker, on the wire as well.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_worker_rebuilds_the_snapshot_with_its_versioning_fields() {
    let hub = OutputHub::new(1024, 4096, 24, 80).expect("hub");
    hub.push(b"\x1b[2J\x1b[Hbefore")
        .expect("push before restart");
    assert_versioning(&hub.terminal_snapshot());
    drop(hub);

    // The restarted worker on the same live PTY output.
    let hub = OutputHub::new(1024, 4096, 24, 80).expect("hub");
    hub.push(b"\x1b[2J\x1b[Hafter").expect("push after restart");
    let after = hub.terminal_snapshot();
    assert_versioning(&after);
    let wire = wire_terminal(&after).expect("conversion");
    assert_eq!(wire.model, Some(wire_model(default_model())));
    assert_eq!(wire.format_version, Some(CURRENT_FORMAT_VERSION));
}

/// Row 17 (reattach to a vt100-model worker from a libghostty-era CLI): the
/// repaint payload is opaque bytes to the client, and the wire snapshot
/// reports the worker's model — `vt100` in this slice's default build, where
/// the flip is still off.
#[tokio::test(flavor = "multi_thread")]
async fn a_vt100_model_worker_reports_its_model_and_an_opaque_repaint() {
    let hub = OutputHub::new(1024, 4096, 4, 20).expect("hub");
    hub.push(b"\x1b[2J\x1b[Hscreen").expect("push");
    let snapshot = hub.terminal_snapshot();
    assert_versioning(&snapshot);
    let wire = wire_terminal(&snapshot).expect("conversion");
    let expected = if cfg!(feature = "libghostty") {
        WireScreenModelId::Libghostty
    } else {
        WireScreenModelId::Vt100
    };
    assert_eq!(
        wire.model,
        Some(expected),
        "session.screen reports the worker's model"
    );
    assert_eq!(wire.format_version, Some(CURRENT_FORMAT_VERSION));

    // The attach repaint reaches the client as opaque bytes: exactly the
    // snapshot's ansi, nothing typed.
    let mut subscriber = hub.subscribe_terminal_snapshot();
    let repaint = collect_repaint(&mut subscriber).await;
    assert_eq!(
        repaint, snapshot.ansi,
        "the repaint payload is opaque bytes"
    );
}

//! A terminal snapshot is built on demand and stays at its ring watermark.
//!
//! The hub serves subscribers, resyncs and snapshot requests, so a scenario
//! that only reads pushes and then asks for a repaint exercises the path where
//! no consumer observed the output while it was produced. Each state change
//! (resize, output) is followed by its own repaint request, so a repaint left
//! over from before that change is visible in the result.

// Rust guideline compliant 2026-06-26

use pohunek_session_worker::{OutputEvent, OutputHub};

#[tokio::test(flavor = "multi_thread")]
async fn unobserved_reads_leave_the_repaint_at_their_watermark() {
    let hub = OutputHub::new(1024, 4096, 24, 80).expect("hub");
    hub.push(b"\x1b[2J\x1b[Hone").expect("push first line");
    hub.push(b"\r\ntwo").expect("push second line");

    let first = hub.terminal_snapshot();
    assert_eq!(first.watermark, hub.next_offset());
    assert_eq!((first.rows, first.cols), (24, 80));
    assert!(first.visible_text.contains("one"), "{first:?}");
    assert!(first.visible_text.contains("two"), "{first:?}");

    hub.resize(12, 60);
    let resized = hub.terminal_snapshot();
    assert_eq!(
        (resized.rows, resized.cols),
        (12, 60),
        "a repaint after resize must have the new size"
    );
    assert_eq!(resized.watermark, first.watermark);
    assert!(resized.visible_text.contains("two"), "{resized:?}");

    hub.push(b"\r\nthree").expect("push after resize");
    let second = hub.terminal_snapshot();
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

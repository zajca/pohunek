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

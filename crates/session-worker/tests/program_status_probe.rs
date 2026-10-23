//! The worker answers the OSC 7501 program-status probe of its PTY child.
//!
//! Each scenario runs a real shell on a real PTY. The shell switches the
//! terminal to raw mode, prints a probe, and dumps the bytes that arrive on its
//! standard input as hex, so the scenario sees exactly what the worker (and an
//! attached client) wrote to the program.

// Rust guideline compliant 2026-06-26

use std::path::Path;
use std::time::Duration;

use pohunek_session_worker::{
    Command, InputFragment, InputPlan, OutputEvent, PtyOwner, WorkerConfig,
};
use pohunek_test_support::tempdir;

/// Upper bound for any single wait on child output.
const OUTPUT_WAIT: Duration = Duration::from_secs(10);
/// The reply the worker writes for an answered probe, as hex.
const REPLY_HEX: &str = "1b5d373530313b3f1b5c";
/// Byte length of the reply.
const REPLY_BYTES: usize = 10;
/// The probe as the program prints it, ST-terminated.
const PROBE_ST: &str = r"\033]7501;?\033\\";
/// The probe as the program prints it, BEL-terminated.
const PROBE_BEL: &str = r"\033]7501;?\007";
/// Control stall bound of the worker's input coordinator.
const INPUT_STALL: Duration = Duration::from_secs(20);
/// Bytes an attach-style write pushes at a program that does not read; far
/// more than the PTY input queue holds.
const STUCK_WRITE_BYTES: usize = 200_000;
/// Echoed bytes that show the PTY input queue is full.
const QUEUE_FULL_ECHO: usize = 4_000;
/// Hold the paced plan keeps the input lock for, longer than the time the
/// reply task needs to queue behind it.
const PLAN_HOLD: Duration = Duration::from_millis(400);

fn spawn(script: &str, cwd: &Path, answers_probe: bool) -> PtyOwner {
    let config = WorkerConfig::new();
    PtyOwner::spawn(
        Command {
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            env: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
            cwd: cwd.to_path_buf(),
            cols: 80,
            rows: 24,
        },
        config.history_bytes,
        config.subscriber_bytes,
        config.input_dedup_entries,
        Some(INPUT_STALL),
        answers_probe,
    )
    .expect("spawn PTY")
}

/// Waits until the retained and live output contains `needle`.
async fn output_until(pty: &PtyOwner, needle: &str) -> String {
    let mut subscriber = pty.subscribe_output(None).expect("subscribe to output");
    let mut collected = Vec::new();
    tokio::time::timeout(OUTPUT_WAIT, async {
        while !String::from_utf8_lossy(&collected).contains(needle) {
            match subscriber.recv().await {
                Some(OutputEvent::Replay(chunk) | OutputEvent::Output(chunk)) => {
                    collected.extend(chunk.bytes);
                }
                Some(_) => {}
                None => break,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}"));
    String::from_utf8_lossy(&collected).into_owned()
}

/// The hex the shell dumped between `DUMP<n>:` and `:END<n>`.
fn dump(output: &str, number: usize) -> String {
    let start = format!("DUMP{number}:");
    let from = output.find(&start).expect("dump start") + start.len();
    let end = output[from..]
        .find(&format!(":END{number}"))
        .expect("dump end");
    output[from..from + end].to_owned()
}

/// Shell fragment that reads `count` bytes and prints them as `DUMP<n>:<hex>:END<n>`.
fn read_dump(number: usize, count: usize) -> String {
    format!(
        "printf 'DUMP{number}:'; head -c {count} | od -An -v -tx1 | tr -d ' \\n'; printf ':END{number}\\n'; "
    )
}

async fn write_stream(pty: &PtyOwner, bytes: &[u8]) {
    pty.input()
        .execute_stream(vec![InputFragment {
            bytes: bytes.to_vec(),
            delay_after: Duration::ZERO,
        }])
        .await
        .expect("write attach-style input");
}

#[tokio::test]
async fn a_program_status_probe_is_answered_once_when_the_session_opts_in() {
    for probe in [PROBE_ST, PROBE_BEL] {
        let cwd = tempdir().expect("temp dir");
        let script = format!(
            "stty raw -echo; printf '{probe}'; {}",
            read_dump(1, REPLY_BYTES)
        );
        let pty = spawn(&script, cwd.path(), true);

        let output = output_until(&pty, ":END1").await;

        assert_eq!(dump(&output, 1), REPLY_HEX, "probe {probe}");
    }
}

#[tokio::test]
async fn a_probe_split_across_reads_is_answered() {
    let cwd = tempdir().expect("temp dir");
    // The second half is printed only after the test saw the first half in the
    // output, so it reaches the worker in a later read.
    let script = format!(
        "stty raw -echo; printf '\\033]7501'; head -c 1 >/dev/null; \
         printf ';?\\033\\\\'; {}",
        read_dump(1, REPLY_BYTES)
    );
    let pty = spawn(&script, cwd.path(), true);

    output_until(&pty, "\u{1b}]7501").await;
    write_stream(&pty, b"g").await;
    let output = output_until(&pty, ":END1").await;

    assert_eq!(dump(&output, 1), REPLY_HEX);
}

#[tokio::test]
async fn a_probe_after_other_output_is_answered_again() {
    let cwd = tempdir().expect("temp dir");
    let script = format!(
        "stty raw -echo; printf '{PROBE_ST}'; {} printf 'x'; printf '{PROBE_ST}'; {}",
        read_dump(1, REPLY_BYTES),
        read_dump(2, REPLY_BYTES)
    );
    let pty = spawn(&script, cwd.path(), true);

    let output = output_until(&pty, ":END2").await;

    assert_eq!(dump(&output, 1), REPLY_HEX);
    assert_eq!(dump(&output, 2), REPLY_HEX);
}

#[tokio::test]
async fn a_session_without_the_opt_in_never_answers_a_probe() {
    let cwd = tempdir().expect("temp dir");
    // Cooked mode: the line the shell reads is whatever reached its input
    // before the newline of the sentinel.
    let script = format!(
        "stty -echo; printf '{PROBE_ST}'; printf READY; IFS= read -r line; \
         printf 'LINE:%s:ENDLINE' \"$line\""
    );
    let pty = spawn(&script, cwd.path(), false);

    output_until(&pty, "READY").await;
    write_stream(&pty, b"SENTINEL\n").await;
    let output = output_until(&pty, ":ENDLINE").await;

    assert!(
        output.contains("LINE:SENTINEL:ENDLINE"),
        "nothing may be typed into a session that did not opt in: {output:?}"
    );
}

#[tokio::test]
async fn an_echoed_probe_reply_is_not_answered_again() {
    let cwd = tempdir().expect("temp dir");
    // `cat` copies its input to its output, so the worker's reply comes back as
    // a probe with no other byte in between.
    let script = format!("stty raw -echo; printf '{PROBE_ST}'; cat");
    let pty = spawn(&script, cwd.path(), true);
    let probe = "\u{1b}]7501;?\u{1b}\\";

    // The program's own probe plus the echo of the one reply.
    let mut output = String::new();
    let mut subscriber = pty.subscribe_output(None).expect("subscribe");
    tokio::time::timeout(OUTPUT_WAIT, async {
        while output.matches(probe).count() < 2 {
            if let Some(OutputEvent::Replay(chunk) | OutputEvent::Output(chunk)) =
                subscriber.recv().await
            {
                output.push_str(&String::from_utf8_lossy(&chunk.bytes));
            }
        }
    })
    .await
    .expect("the reply is echoed back once");
    // Whatever the worker would still answer reaches `cat` before the marker.
    write_stream(&pty, b"MARKER").await;
    tokio::time::timeout(OUTPUT_WAIT, async {
        while !output.contains("MARKER") {
            if let Some(OutputEvent::Replay(chunk) | OutputEvent::Output(chunk)) =
                subscriber.recv().await
            {
                output.push_str(&String::from_utf8_lossy(&chunk.bytes));
            }
        }
    })
    .await
    .expect("the marker is echoed");

    assert_eq!(
        output.matches(probe).count(),
        2,
        "no reply loop: {output:?}"
    );
}

#[tokio::test]
async fn a_probe_reply_waits_for_a_paced_plan_and_coalesces() {
    let cwd = tempdir().expect("temp dir");
    // The shell prints a probe after reading the plan's first fragment and a
    // second one once the test opened the gate; the plan holds the input lock
    // for its delay, so the second probe finds the first reply still pending.
    let gate = cwd.path().join("go");
    let script = format!(
        "stty raw -echo; head -c 1 >/dev/null; printf '{PROBE_ST}'; printf P1; \
         while [ ! -e '{gate}' ]; do :; done; printf 'x{PROBE_ST}'; {dump1} {dump2}",
        gate = gate.display(),
        dump1 = read_dump(1, 1 + REPLY_BYTES),
        dump2 = read_dump(2, 1)
    );
    let pty = spawn(&script, cwd.path(), true);

    let plan = InputPlan {
        write_id: "paced-plan".to_owned(),
        fragments: vec![
            InputFragment {
                bytes: b"a".to_vec(),
                delay_after: PLAN_HOLD,
            },
            InputFragment {
                bytes: b"b".to_vec(),
                delay_after: Duration::ZERO,
            },
        ],
    };
    let paced = {
        let pty = pty.clone();
        tokio::spawn(async move { pty.input().execute_control("namespace", 1, plan).await })
    };
    output_until(&pty, "P1").await;
    std::fs::write(&gate, b"").expect("open the gate");
    paced.await.expect("plan task").expect("paced plan");
    let output = output_until(&pty, ":END1").await;

    assert_eq!(
        dump(&output, 1),
        format!("62{REPLY_HEX}"),
        "the reply follows the whole plan"
    );
    // A second reply would sit in the input ahead of the marker.
    write_stream(&pty, b"Z").await;
    let output = output_until(&pty, ":END2").await;
    assert_eq!(
        dump(&output, 2),
        "5a",
        "the two probes coalesced into one reply"
    );
}

#[tokio::test]
async fn a_probe_reply_stuck_behind_a_full_pty_never_blocks_output() {
    let cwd = tempdir().expect("temp dir");
    let gate = cwd.path().join("go");
    // Raw mode with echo: every byte the PTY accepts is echoed, which shows
    // when the input queue is full. The program reads nothing until the gate
    // opens, then prints a probe and drains its input.
    let script = format!(
        "stty raw echo; printf READY; while [ ! -e '{gate}' ]; do :; done; \
         stty -echo; printf '{PROBE_ST}'; printf P1; \
         printf 'DUMP1:'; head -c {total} | tr -d z | od -An -v -tx1 | tr -d ' \\n'; printf ':END1\\n'",
        gate = gate.display(),
        total = STUCK_WRITE_BYTES + REPLY_BYTES
    );
    let pty = spawn(&script, cwd.path(), true);
    output_until(&pty, "READY").await;

    let writer = {
        let pty = pty.clone();
        tokio::spawn(async move { write_stream(&pty, &vec![b'z'; STUCK_WRITE_BYTES]).await })
    };
    output_until(&pty, &"z".repeat(QUEUE_FULL_ECHO)).await;
    std::fs::write(&gate, b"").expect("open the gate");
    // Output keeps flowing while the reply waits behind the stuck write.
    output_until(&pty, "P1").await;
    writer.await.expect("writer task");
    let output = output_until(&pty, ":END1").await;

    assert_eq!(dump(&output, 1), REPLY_HEX);
}

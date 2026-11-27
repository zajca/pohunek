//! `pohunek attach` against the real daemon and worker: the repaint a client
//! receives on attach is one synchronized-output bracket, and a program that
//! holds DEC mode 2026 keeps it.
//!
//! The session runs a real shell. The program's own `CSI ? 2026 h` is written
//! before the client attaches, so the repaint is the only source of the bytes
//! the client sees on attach. The client's own terminal teardown (which resets
//! mode 2026) runs only on a terminal, so the piped scenarios do not cover it;
//! the last scenario runs the client on a pseudo terminal for that.

#![cfg(unix)]

// Rust guideline compliant 2026-11-26

use std::io::{Read as _, Write as _};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use pohunek_test_support::wait::{guard, wait_until};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::Harness;

/// Begins a synchronized-output bracket.
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";

/// Ends a synchronized-output bracket.
const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Clears the screen first in a repaint's body, after the bracket begins.
const REPAINT_BODY_START: &[u8] = b"\x1b[2J\x1b[H";

/// Detach shortcut byte of the attach client.
const DETACH_BYTE: u8 = 0x1d;

/// Output the shell produces once the program has written its mode sequences.
/// The arithmetic keeps the marker out of the typed command line.
const READY_COMMAND_SUFFIX: &str = "echo ready-$((6*7))";

/// The marker the shell prints for `READY_COMMAND_SUFFIX`.
const READY_MARKER: &[u8] = b"ready-42";

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Starts a shell session, makes it write `program_output` (printf escapes)
/// and returns its id once the shell printed the ready marker.
async fn session_that_wrote(harness: &Harness, program_output: &str) -> String {
    let (code, created) = harness
        .json(&["session", "new", "--agent", "shell", "--name", "sync"])
        .await;
    assert_eq!(code, 0, "{created}");
    let id = created["ok"]["session"]["id"]
        .as_str()
        .or_else(|| created["ok"]["id"].as_str())
        .expect("created session id")
        .to_owned();
    let command = format!("printf '{program_output}'; {READY_COMMAND_SUFFIX}");
    let (code, sent) = harness.json(&["session", "input", &id, &command]).await;
    assert_eq!(code, 0, "{sent}");
    wait_until("the shell to print the ready marker", || async {
        let (code, read) = harness.json(&["session", "read", &id]).await;
        assert_eq!(code, 0, "{read}");
        read.to_string().contains("ready-42").then_some(())
    })
    .await;
    id
}

/// An attached `pohunek attach` process with its piped stdio.
struct Attached {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    seen: Vec<u8>,
}

impl Attached {
    fn start(harness: &Harness, id: &str) -> Self {
        let mut command = harness
            .env
            .tokio_command(pohunek_test_support::bin_exe("pohunek"));
        command
            .args(["attach", id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("spawn pohunek attach");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        Self {
            child,
            stdin,
            stdout,
            seen: Vec::new(),
        }
    }

    /// Reads the client's output until `done` holds for everything seen.
    async fn read_until(&mut self, what: &str, done: impl Fn(&[u8]) -> bool) {
        let mut buffer = vec![0_u8; 16 * 1024];
        while !done(&self.seen) {
            let count = guard(what, self.stdout.read(&mut buffer))
                .await
                .expect("read the client output");
            assert!(count > 0, "the client ended before {what}: {:?}", self.seen);
            self.seen.extend_from_slice(&buffer[..count]);
        }
    }

    /// Detaches with the shortcut and returns everything the client wrote.
    async fn detach(mut self) -> Vec<u8> {
        self.stdin
            .write_all(&[DETACH_BYTE])
            .await
            .expect("write the detach shortcut");
        let mut rest = Vec::new();
        guard(
            "the client output to end after the detach",
            self.stdout.read_to_end(&mut rest),
        )
        .await
        .expect("read the client output to its end");
        self.seen.extend_from_slice(&rest);
        let status = guard("the client to exit", self.child.wait())
            .await
            .expect("wait for the client");
        assert!(status.success(), "the client exits cleanly: {status}");
        self.seen
    }
}

#[tokio::test]
async fn the_attach_repaint_is_one_bracket_the_client_receives() {
    let harness = Harness::start().await;
    // The program sets and resets mode 2026 around its own frame.
    let id = session_that_wrote(&harness, r"\033[?2026h\033[?2026l").await;

    let mut attached = Attached::start(&harness, &id);
    attached
        .read_until("the repaint to end its bracket", |seen| {
            find(seen, SYNC_END).is_some()
        })
        .await;

    let begin = find(&attached.seen, SYNC_BEGIN).expect("the repaint begins a bracket");
    let end = find(&attached.seen, SYNC_END).expect("the repaint ends its bracket");
    assert!(
        attached.seen[begin..].starts_with(&[SYNC_BEGIN, REPAINT_BODY_START].concat()),
        "{:?}",
        attached.seen
    );
    assert!(begin < end, "{:?}", attached.seen);
    let marker = find(&attached.seen, READY_MARKER).expect("the repaint carries the screen");
    assert!(begin < marker && marker < end, "{:?}", attached.seen);
    assert_eq!(
        attached
            .seen
            .windows(SYNC_BEGIN.len())
            .filter(|w| *w == SYNC_BEGIN)
            .count(),
        1,
        "{:?}",
        attached.seen
    );
    drop(attached.detach().await);

    harness.stop().await;
}

#[tokio::test]
async fn a_program_held_mode_survives_the_repaint() {
    let harness = Harness::start().await;
    // The program sets mode 2026 and has not reset it when the client attaches.
    let id = session_that_wrote(&harness, r"\033[?2026h").await;

    let mut attached = Attached::start(&harness, &id);
    attached
        .read_until("the repaint to show the screen", |seen| {
            find(seen, READY_MARKER).is_some()
        })
        .await;
    let begin = find(&attached.seen, SYNC_BEGIN).expect("the repaint begins a bracket");
    assert!(
        attached.seen[begin..].starts_with(&[SYNC_BEGIN, REPAINT_BODY_START].concat()),
        "{:?}",
        attached.seen
    );
    // The end of the repaint is not marked when the mode stays set, so the
    // whole output up to the detach is checked for a reset.
    let all = attached.detach().await;
    assert_eq!(
        find(&all, SYNC_END),
        None,
        "the repaint must not reset the program's mode: {all:?}"
    );

    harness.stop().await;
}

#[tokio::test]
async fn the_client_teardown_resets_a_program_held_mode_on_a_terminal() {
    let harness = Harness::start().await;
    let id = session_that_wrote(&harness, r"\033[?2026h").await;

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a pty");
    let std_command = harness
        .env
        .command(pohunek_test_support::bin_exe("pohunek"));
    let mut command = CommandBuilder::new(std_command.get_program());
    command.env_clear();
    for (name, value) in std_command.get_envs() {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    command.args(["attach", &id]);
    command.cwd(harness.env.cwd());
    let mut child = pair.slave.spawn_command(command).expect("start the attach");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let mut writer = pair.master.take_writer().expect("pty writer");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let transcript = std::thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        // The read fails with EIO once the client has closed the terminal.
        while let Ok(read) = reader.read(&mut chunk) {
            if read == 0 {
                break;
            }
            sink.lock()
                .expect("transcript lock")
                .extend_from_slice(&chunk[..read]);
        }
    });
    let snapshot = || seen.lock().expect("transcript lock").clone();

    wait_until("the repaint to show the screen", || async {
        find(&snapshot(), READY_MARKER).map(|_| ())
    })
    .await;
    assert_eq!(
        find(&snapshot(), SYNC_END),
        None,
        "the repaint must not reset the program's mode: {:?}",
        snapshot()
    );

    writer.write_all(&[DETACH_BYTE]).expect("type the detach");
    let status = guard(
        "the client to exit",
        tokio::task::spawn_blocking(move || child.wait()),
    )
    .await
    .expect("join the wait")
    .expect("wait for the client");
    assert!(status.success(), "the client exits cleanly: {status:?}");
    drop(writer);
    drop(pair.master);
    guard(
        "the transcript to end",
        tokio::task::spawn_blocking(move || transcript.join()),
    )
    .await
    .expect("join the transcript")
    .expect("the transcript thread");

    // The teardown resets the mode the program left set, after its last set.
    let all = snapshot();
    let last_set = all
        .windows(SYNC_BEGIN.len())
        .rposition(|w| w == SYNC_BEGIN)
        .expect("the repaint set the mode");
    let last_reset = all
        .windows(SYNC_END.len())
        .rposition(|w| w == SYNC_END)
        .unwrap_or_else(|| panic!("the teardown resets the mode: {all:?}"));
    assert!(last_set < last_reset, "{all:?}");

    harness.stop().await;
}

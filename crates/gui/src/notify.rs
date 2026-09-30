//! Desktop notification backends and their outcome classification.
//!
//! Two backends exist. A configured (or, off macOS, defaulted) command such as
//! `notify-send` receives the title and body as two positional arguments. On
//! macOS the default is `/usr/bin/osascript`, which shows the notification
//! through the `AppleScript` `display notification` command; title and body are
//! only ever `argv` items of the script, never spliced into script text.
//!
//! # What an outcome can and cannot say
//!
//! [`NotificationOutcome::Submitted`] means the backend accepted the request
//! and exited 0. `osascript` exits 0 whether the notification was shown or was
//! suppressed by System Settings (Notifications, Focus), and an unbundled
//! binary cannot query that state (`UNUserNotificationCenter` requires an app
//! bundle). Delivery is therefore unconfirmable until the GUI ships as a
//! bundle. [`NotificationOutcome::Denied`] is reported only for the single
//! permission signal a backend documents: `osascript` printing error `-1743`
//! (`errAEEventNotPermitted`). Every other failure is
//! [`NotificationOutcome::Unavailable`].

// Rust guideline compliant 2026-09-30
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use crate::bin_resolver::BinResolver;
use crate::terminal::{run_bounded, BoundedError};

/// The `osascript` executable; it lives at this fixed path on every macOS release.
pub(crate) const SYSTEM_OSASCRIPT: &str = "/usr/bin/osascript";

/// Default command outside macOS; the freedesktop notification CLI.
pub(crate) const DEFAULT_NOTIFY_COMMAND: &str = "notify-send";

/// `osascript` error code for a refused Apple-event permission
/// (`errAEEventNotPermitted`), the one denial signal it documents.
const OSASCRIPT_DENIED_MARKER: &[u8] = b"(-1743)";

/// Script lines: `argv` item 1 is the title, item 2 the body.
const OSASCRIPT_SCRIPT: [&str; 3] = [
    "on run argv",
    "display notification (item 2 of argv) with title (item 1 of argv)",
    "end run",
];

/// Result of asking a backend to show one notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotificationOutcome {
    /// The backend accepted the request and exited successfully.
    Submitted,
    /// The backend positively reported a permission denial.
    Denied(String),
    /// The backend could not run, timed out, or failed.
    Unavailable(String),
}

/// Coarse state used to notify the user once per change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum NotificationHealth {
    /// No notification has completed yet.
    #[default]
    Unknown,
    Working,
    Denied,
    Unavailable,
}

/// Records `outcome` and returns the status text to show when the state changed.
///
/// Repeated identical failures return `None`, so a broken backend does not
/// overwrite the status line once per notification. Recovery from a failure is
/// announced once.
pub(crate) fn apply_outcome(
    health: &mut NotificationHealth,
    outcome: &NotificationOutcome,
) -> Option<String> {
    let next = match outcome {
        NotificationOutcome::Submitted => NotificationHealth::Working,
        NotificationOutcome::Denied(_) => NotificationHealth::Denied,
        NotificationOutcome::Unavailable(_) => NotificationHealth::Unavailable,
    };
    let previous = std::mem::replace(health, next);
    if previous == next {
        return None;
    }
    match outcome {
        NotificationOutcome::Submitted => matches!(
            previous,
            NotificationHealth::Denied | NotificationHealth::Unavailable
        )
        .then(|| "desktop notifications work again".to_owned()),
        NotificationOutcome::Denied(reason) => {
            Some(format!("desktop notifications are denied: {reason}"))
        }
        NotificationOutcome::Unavailable(reason) => {
            Some(format!("desktop notifications are unavailable: {reason}"))
        }
    }
}

/// Which backend produced an exit status; the classifier reads it differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackendKind {
    Osascript,
    Command,
}

/// Classifies a finished backend process.
///
/// `code` is `None` when a signal ended the process. `stderr` is inspected only
/// for `osascript`, whose denial marker is the sole detection implemented.
pub(crate) fn classify_exit(
    kind: BackendKind,
    code: Option<i32>,
    stderr: &[u8],
) -> NotificationOutcome {
    match code {
        Some(0) => NotificationOutcome::Submitted,
        Some(code) => {
            if kind == BackendKind::Osascript && contains(stderr, OSASCRIPT_DENIED_MARKER) {
                NotificationOutcome::Denied(
                    "osascript reported that the notification is not permitted".to_owned(),
                )
            } else {
                NotificationOutcome::Unavailable(format!("the backend exited with code {code}"))
            }
        }
        None => NotificationOutcome::Unavailable("the backend was ended by a signal".to_owned()),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Where a notification is sent.
#[derive(Debug, Clone)]
pub(crate) enum NotificationBackend {
    /// `osascript` at `executable`.
    Osascript { executable: PathBuf },
    /// A command that takes title and body as two positional arguments.
    Command { resolver: Arc<BinResolver> },
}

/// A backend plus the deadline every notification must meet.
#[derive(Debug, Clone)]
pub(crate) struct Notifier {
    pub(crate) backend: NotificationBackend,
    pub(crate) timeout: Duration,
}

/// The `osascript` arguments for one notification.
///
/// The `--` ends option parsing, so a title that starts with `-` stays an
/// argument.
fn osascript_arguments(title: &str, body: &str) -> Vec<OsString> {
    let mut arguments = Vec::new();
    for line in OSASCRIPT_SCRIPT {
        arguments.push(OsString::from("-e"));
        arguments.push(OsString::from(line));
    }
    arguments.push(OsString::from("--"));
    arguments.push(OsString::from(title));
    arguments.push(OsString::from(body));
    arguments
}

impl Notifier {
    /// Shows one notification and waits at most the deadline.
    ///
    /// Blocks on executable resolution and the child, so callers run it off
    /// the UI thread. The child is killed and reaped on timeout.
    pub(crate) fn notify(&self, title: &str, body: &str) -> NotificationOutcome {
        if title.contains('\0') || body.contains('\0') {
            return NotificationOutcome::Unavailable(
                "the notification text contains a NUL byte".to_owned(),
            );
        }
        let (kind, mut command) = match &self.backend {
            NotificationBackend::Osascript { executable } => {
                let mut command = Command::new(executable);
                command
                    .args(osascript_arguments(title, body))
                    .stderr(Stdio::piped());
                (BackendKind::Osascript, command)
            }
            NotificationBackend::Command { resolver } => {
                let program = match resolver.resolve() {
                    Ok(program) => program,
                    Err(error) => return NotificationOutcome::Unavailable(error.to_string()),
                };
                let mut command = Command::new(program);
                command.arg(title).arg(body).stderr(Stdio::null());
                (BackendKind::Command, command)
            }
        };
        command.stdin(Stdio::null()).stdout(Stdio::null());
        match run_bounded(&mut command, self.timeout) {
            Ok(output) => classify_exit(kind, output.status.code(), &output.stderr),
            Err(BoundedError::Spawn(error)) => {
                NotificationOutcome::Unavailable(format!("cannot start the backend: {error}"))
            }
            Err(BoundedError::Wait(error)) => {
                NotificationOutcome::Unavailable(format!("waiting for the backend failed: {error}"))
            }
            Err(BoundedError::Timeout) => NotificationOutcome::Unavailable(format!(
                "the backend did not finish within {:?} and was killed",
                self.timeout
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use super::*;
    use crate::bin_resolver::BinError;

    fn script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod script");
    }

    /// Writes a fake backend that records its NUL-terminated argv, then runs `tail`.
    fn recording_backend(dir: &Path, tail: &str) -> (PathBuf, PathBuf) {
        let record = dir.join("argv");
        let program = dir.join("backend");
        script(
            &program,
            &format!("printf '%s\\0' \"$@\" > '{}'\n{tail}", record.display()),
        );
        (program, record)
    }

    fn read_argv(path: &Path) -> Vec<Vec<u8>> {
        let bytes = fs::read(path).expect("read argv");
        let mut words: Vec<Vec<u8>> = bytes.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect();
        assert_eq!(words.pop(), Some(Vec::new()));
        words
    }

    fn command_notifier(program: &Path, timeout: Duration) -> Notifier {
        Notifier {
            backend: NotificationBackend::Command {
                resolver: Arc::new(BinResolver::with_discovery(
                    program.to_str().expect("utf8"),
                    || Err(BinError::SearchPath("must not run".to_owned())),
                )),
            },
            timeout,
        }
    }

    fn osascript_notifier(program: &Path, timeout: Duration) -> Notifier {
        Notifier {
            backend: NotificationBackend::Osascript {
                executable: program.to_path_buf(),
            },
            timeout,
        }
    }

    const LONG: Duration = Duration::from_secs(30);

    const DIFFICULT: [&str; 9] = [
        "it's \"quoted\"",
        "$(echo INJECTED) `id`",
        "line one\nline two",
        "\u{10d}esk\u{fd} \u{65e5}\u{672c}\u{8a9e} \u{1f980}",
        "",
        "-n",
        "--help",
        "{title};echo INJECTED",
        "back\\slash 'x'",
    ];

    #[test]
    fn classification_maps_exit_codes_and_the_denial_marker() {
        assert_eq!(
            classify_exit(BackendKind::Command, Some(0), b""),
            NotificationOutcome::Submitted
        );
        assert_eq!(
            classify_exit(BackendKind::Osascript, Some(0), b"ignored (-1743)"),
            NotificationOutcome::Submitted
        );
        assert!(matches!(
            classify_exit(
                BackendKind::Osascript,
                Some(1),
                b"execution error: x (-1743)\n"
            ),
            NotificationOutcome::Denied(_)
        ));
        assert!(matches!(
            classify_exit(BackendKind::Osascript, Some(1), b"syntax error (-2740)"),
            NotificationOutcome::Unavailable(_)
        ));
        // A generic command's stderr is never interpreted as a denial.
        assert!(matches!(
            classify_exit(BackendKind::Command, Some(1), b"(-1743)"),
            NotificationOutcome::Unavailable(_)
        ));
        assert!(matches!(
            classify_exit(BackendKind::Command, None, b""),
            NotificationOutcome::Unavailable(_)
        ));
    }

    #[test]
    fn health_reports_only_state_changes() {
        let mut health = NotificationHealth::default();
        let unavailable = NotificationOutcome::Unavailable("gone".to_owned());
        let denied = NotificationOutcome::Denied("no".to_owned());

        assert_eq!(
            apply_outcome(&mut health, &NotificationOutcome::Submitted),
            None
        );
        assert!(apply_outcome(&mut health, &unavailable)
            .expect("first failure")
            .contains("unavailable: gone"));
        assert_eq!(apply_outcome(&mut health, &unavailable), None);
        assert!(apply_outcome(&mut health, &denied)
            .expect("state change")
            .contains("denied: no"));
        assert_eq!(apply_outcome(&mut health, &denied), None);
        assert!(apply_outcome(&mut health, &NotificationOutcome::Submitted)
            .expect("recovery")
            .contains("work again"));
        assert_eq!(
            apply_outcome(&mut health, &NotificationOutcome::Submitted),
            None
        );
    }

    #[test]
    fn a_first_failure_is_reported_even_before_any_success() {
        let mut health = NotificationHealth::default();
        assert!(
            apply_outcome(&mut health, &NotificationOutcome::Unavailable("x".into())).is_some()
        );
    }

    #[test]
    fn command_backend_receives_title_and_body_as_two_positional_arguments() {
        for title in DIFFICULT {
            for body in DIFFICULT {
                let dir = tempfile::tempdir().expect("tempdir");
                let (program, record) = recording_backend(dir.path(), "exit 0");

                let outcome = command_notifier(&program, LONG).notify(title, body);

                assert_eq!(outcome, NotificationOutcome::Submitted);
                assert_eq!(
                    read_argv(&record),
                    vec![title.as_bytes().to_vec(), body.as_bytes().to_vec()]
                );
            }
        }
    }

    #[test]
    fn osascript_backend_passes_values_only_as_argv_after_the_terminator() {
        for title in DIFFICULT {
            let dir = tempfile::tempdir().expect("tempdir");
            let (program, record) = recording_backend(dir.path(), "exit 0");
            let body = "body $(x) 'y'\n-z";

            let outcome = osascript_notifier(&program, LONG).notify(title, body);

            assert_eq!(outcome, NotificationOutcome::Submitted);
            let argv = read_argv(&record);
            let expected: Vec<Vec<u8>> = [
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
                "--",
            ]
            .iter()
            .map(|word| word.as_bytes().to_vec())
            .chain([title.as_bytes().to_vec(), body.as_bytes().to_vec()])
            .collect();
            assert_eq!(argv, expected);
            assert!(
                !argv[..7]
                    .iter()
                    .any(|word| word.windows(6).any(|w| w == b"INJECT")),
                "script text carries no notification data"
            );
        }
    }

    #[test]
    fn a_non_zero_exit_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (program, _record) = recording_backend(dir.path(), "exit 4");

        let outcome = command_notifier(&program, LONG).notify("t", "b");

        assert_eq!(
            outcome,
            NotificationOutcome::Unavailable("the backend exited with code 4".to_owned())
        );
    }

    #[test]
    fn osascript_denial_marker_on_stderr_is_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (program, _record) = recording_backend(
            dir.path(),
            "echo 'execution error: Not authorized. (-1743)' >&2\nexit 1",
        );

        let outcome = osascript_notifier(&program, LONG).notify("t", "b");

        assert!(
            matches!(outcome, NotificationOutcome::Denied(_)),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_missing_executable_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no-such-backend");

        let osascript = osascript_notifier(&missing, LONG).notify("t", "b");
        assert!(
            matches!(&osascript, NotificationOutcome::Unavailable(reason) if reason.contains("cannot start")),
            "{osascript:?}"
        );

        let command = command_notifier(&missing, LONG).notify("t", "b");
        assert!(
            matches!(&command, NotificationOutcome::Unavailable(reason) if reason.contains("not an executable") || reason.contains("unusable")),
            "{command:?}"
        );
    }

    #[test]
    fn an_unresolvable_bare_command_is_unavailable_not_healthy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let search = pohunek_platform::shell_env::SearchPath::new(vec![dir.path().to_path_buf()])
            .expect("search path");
        let notifier = Notifier {
            backend: NotificationBackend::Command {
                resolver: Arc::new(BinResolver::with_discovery("notify-send", move || {
                    Ok(search.clone())
                })),
            },
            timeout: LONG,
        };

        let outcome = notifier.notify("t", "b");

        assert!(
            matches!(&outcome, NotificationOutcome::Unavailable(reason) if reason.contains("notify-send")),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_hanging_backend_is_killed_at_the_deadline() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A stopped process never exits by itself; only SIGKILL ends it.
        let (program, _record) = recording_backend(dir.path(), "kill -STOP $$");

        let outcome = command_notifier(&program, Duration::from_millis(200)).notify("t", "b");

        assert!(
            matches!(&outcome, NotificationOutcome::Unavailable(reason) if reason.contains("killed")),
            "{outcome:?}"
        );
    }

    #[test]
    fn nul_bytes_are_rejected_before_spawning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (program, record) = recording_backend(dir.path(), "exit 0");

        let outcome = command_notifier(&program, LONG).notify("a\0b", "body");

        assert!(matches!(outcome, NotificationOutcome::Unavailable(_)));
        assert!(!record.exists());
    }

    #[test]
    fn osascript_arguments_place_terminator_before_values() {
        let arguments = osascript_arguments("-t", "b");
        assert_eq!(arguments[6], OsString::from("--"));
        assert_eq!(arguments[7].as_bytes(), b"-t");
    }
}

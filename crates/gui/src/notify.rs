//! Desktop notification backends.
//!
//! Two backends exist. A configured (or, off macOS, defaulted) command such as
//! `notify-send` receives the title and body as two positional arguments. On
//! macOS the default is `/usr/bin/osascript`, which shows the notification
//! through the `AppleScript` `display notification` command; title and body are
//! only ever `argv` items of the script, never spliced into script text.
//!
//! # What an outcome can and cannot say
//!
//! [`NotificationOutcome::Submitted`] means the backend accepted the request and
//! exited 0. `osascript` exits 0 whether the notification was shown or was
//! suppressed by System Settings (Notifications, Focus), and an unbundled
//! binary has no API to ask: `UNUserNotificationCenter` authorization needs an
//! app bundle (issue #104). Delivery, and a user's denial, are therefore
//! unobservable until the GUI ships as a bundle; every failure the GUI can
//! observe is [`NotificationOutcome::Unavailable`].

// Rust guideline compliant 2026-10-01
#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use crate::bin_resolver::BinResolver;
use crate::terminal::{run_bounded, BoundedError};

/// The `osascript` executable; it lives at this fixed path on every macOS release.
pub(crate) const SYSTEM_OSASCRIPT: &str = "/usr/bin/osascript";

/// Default command outside macOS; the freedesktop notification CLI.
pub(crate) const DEFAULT_NOTIFY_COMMAND: &str = "notify-send";

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
    /// The backend could not run, timed out, or failed.
    Unavailable(String),
}

/// Coarse state used to tell the user once per change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum NotificationHealth {
    /// No notification has completed yet.
    #[default]
    Unknown,
    Working,
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
        NotificationOutcome::Unavailable(_) => NotificationHealth::Unavailable,
    };
    let previous = std::mem::replace(health, next);
    if previous == next {
        return None;
    }
    match outcome {
        NotificationOutcome::Submitted => (previous == NotificationHealth::Unavailable)
            .then(|| "desktop notifications work again".to_owned()),
        NotificationOutcome::Unavailable(reason) => {
            Some(format!("desktop notifications are unavailable: {reason}"))
        }
    }
}

/// The command backend's executable, resolved once per session.
///
/// The first notification resolves the program within the notification
/// deadline; the result, including a miss or a timeout, is kept. Concurrent
/// notifications wait on that one resolution instead of starting their own
/// login-shell probes.
#[derive(Debug)]
pub(crate) struct CommandResolution {
    resolver: Arc<BinResolver>,
    program: OsString,
    resolved: OnceLock<Result<PathBuf, String>>,
}

impl CommandResolution {
    pub(crate) fn new(resolver: Arc<BinResolver>, program: &str) -> Self {
        Self {
            resolver,
            program: OsString::from(program),
            resolved: OnceLock::new(),
        }
    }

    fn resolve(&self, deadline: Duration) -> &Result<PathBuf, String> {
        self.resolved.get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            let resolver = Arc::clone(&self.resolver);
            let program = self.program.clone();
            let spawned = thread::Builder::new()
                .name("pohunek-gui-notify-resolve".to_owned())
                .spawn(move || {
                    let _ = sender.send(resolver.resolve_name(&program));
                });
            if let Err(error) = spawned {
                return Err(format!("cannot start the executable lookup: {error}"));
            }
            match receiver.recv_timeout(deadline) {
                Ok(Ok(path)) => Ok(path),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err(format!(
                    "finding `{}` did not finish within {deadline:?}",
                    self.program.to_string_lossy()
                )),
            }
        })
    }
}

/// Where a notification is sent.
#[derive(Debug, Clone)]
pub(crate) enum NotificationBackend {
    /// `osascript` at `executable`.
    Osascript { executable: PathBuf },
    /// A command that takes title and body as two positional arguments.
    Command { resolution: Arc<CommandResolution> },
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

/// The arguments a command backend receives.
///
/// `notify-send` parses options, and the text comes from the daemon, so `--`
/// precedes it. Other commands keep the plain positional contract
/// (`<command> <title> <body>`).
fn command_arguments(program: &std::path::Path, title: &str, body: &str) -> Vec<OsString> {
    let mut arguments = Vec::new();
    if program.file_name() == Some(OsStr::new(DEFAULT_NOTIFY_COMMAND)) {
        arguments.push(OsString::from("--"));
    }
    arguments.push(OsString::from(title));
    arguments.push(OsString::from(body));
    arguments
}

impl Notifier {
    /// Shows one notification and waits at most the deadline.
    ///
    /// Blocks on the first executable resolution and on the child, so callers
    /// run it off the UI thread. The child is killed and reaped on timeout.
    pub(crate) fn notify(&self, title: &str, body: &str) -> NotificationOutcome {
        if title.contains('\0') || body.contains('\0') {
            return NotificationOutcome::Unavailable(
                "the notification text contains a NUL byte".to_owned(),
            );
        }
        let mut command = match &self.backend {
            NotificationBackend::Osascript { executable } => {
                let mut command = Command::new(executable);
                command.args(osascript_arguments(title, body));
                command
            }
            NotificationBackend::Command { resolution } => {
                let program = match resolution.resolve(self.timeout) {
                    Ok(program) => program,
                    Err(reason) => return NotificationOutcome::Unavailable(reason.clone()),
                };
                let mut command = Command::new(program);
                command.args(command_arguments(program, title, body));
                command
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match run_bounded(&mut command, self.timeout) {
            Ok(output) => match output.status.code() {
                Some(0) => NotificationOutcome::Submitted,
                Some(code) => {
                    NotificationOutcome::Unavailable(format!("the backend exited with code {code}"))
                }
                None => {
                    NotificationOutcome::Unavailable("the backend was ended by a signal".to_owned())
                }
            },
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::bin_resolver::BinError;

    fn script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod script");
    }

    /// Writes a fake backend that records its NUL-terminated argv, then runs `tail`.
    fn recording_backend(dir: &Path, name: &str, tail: &str) -> (PathBuf, PathBuf) {
        let record = dir.join("argv");
        let program = dir.join(name);
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

    fn resolver_failing() -> Arc<BinResolver> {
        Arc::new(BinResolver::with_discovery("unused", || {
            Err(BinError::SearchPath("must not run".to_owned()))
        }))
    }

    fn command_notifier(program: &Path, timeout: Duration) -> Notifier {
        Notifier {
            backend: NotificationBackend::Command {
                resolution: Arc::new(CommandResolution::new(
                    resolver_failing(),
                    program.to_str().expect("utf8"),
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
    fn health_reports_only_state_changes() {
        let mut health = NotificationHealth::default();
        let unavailable = NotificationOutcome::Unavailable("gone".to_owned());

        assert_eq!(
            apply_outcome(&mut health, &NotificationOutcome::Submitted),
            None
        );
        assert!(apply_outcome(&mut health, &unavailable)
            .expect("first failure")
            .contains("unavailable: gone"));
        assert_eq!(apply_outcome(&mut health, &unavailable), None);
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
        assert!(apply_outcome(
            &mut health,
            &NotificationOutcome::Unavailable("x".to_owned())
        )
        .is_some());
    }

    #[test]
    fn a_custom_command_receives_title_and_body_as_two_positional_arguments() {
        for title in DIFFICULT {
            for body in DIFFICULT {
                let dir = tempfile::tempdir().expect("tempdir");
                let (program, record) = recording_backend(dir.path(), "my-notify", "exit 0");

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
    fn notify_send_gets_an_option_terminator_before_the_text() {
        for title in ["-t", "--help", "plain"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let (program, record) = recording_backend(dir.path(), "notify-send", "exit 0");

            let outcome = command_notifier(&program, LONG).notify(title, "-body");

            assert_eq!(outcome, NotificationOutcome::Submitted);
            assert_eq!(
                read_argv(&record),
                vec![b"--".to_vec(), title.as_bytes().to_vec(), b"-body".to_vec()]
            );
        }
    }

    #[test]
    fn osascript_backend_passes_values_only_as_argv_after_the_terminator() {
        for title in DIFFICULT {
            let dir = tempfile::tempdir().expect("tempdir");
            let (program, record) = recording_backend(dir.path(), "osascript", "exit 0");
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
        }
    }

    #[test]
    fn a_non_zero_exit_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (program, _record) = recording_backend(dir.path(), "backend", "exit 4");

        let outcome = command_notifier(&program, LONG).notify("t", "b");

        assert_eq!(
            outcome,
            NotificationOutcome::Unavailable("the backend exited with code 4".to_owned())
        );
    }

    #[test]
    fn a_signalled_backend_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (program, _record) = recording_backend(dir.path(), "backend", "kill -KILL $$");

        assert!(matches!(
            command_notifier(&program, LONG).notify("t", "b"),
            NotificationOutcome::Unavailable(reason) if reason.contains("signal")
        ));
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
            matches!(&command, NotificationOutcome::Unavailable(reason) if reason.contains("unusable")),
            "{command:?}"
        );
    }

    #[test]
    fn an_unresolvable_bare_command_is_unavailable_not_healthy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let search = pohunek_platform::shell_env::SearchPath::new(vec![dir.path().to_path_buf()])
            .expect("search path");
        let resolver = Arc::new(BinResolver::with_discovery("pohunek", move || {
            Ok(search.clone())
        }));
        let notifier = Notifier {
            backend: NotificationBackend::Command {
                resolution: Arc::new(CommandResolution::new(resolver, "notify-send")),
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
    fn the_command_is_resolved_once_and_a_miss_is_remembered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let search = pohunek_platform::shell_env::SearchPath::new(vec![dir.path().to_path_buf()])
            .expect("search path");
        let resolver = Arc::new(BinResolver::with_discovery("pohunek", move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(search.clone())
        }));
        let notifier = Notifier {
            backend: NotificationBackend::Command {
                resolution: Arc::new(CommandResolution::new(resolver, "notify-send")),
            },
            timeout: LONG,
        };

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let notifier = notifier.clone();
                thread::spawn(move || notifier.notify("t", "b"))
            })
            .collect();
        for handle in handles {
            assert!(matches!(
                handle.join().expect("thread"),
                NotificationOutcome::Unavailable(_)
            ));
        }
        // The cached miss answers without another discovery, even if the
        // program appears later in the session.
        script(&dir.path().join("notify-send"), "exit 0");
        assert!(matches!(
            notifier.notify("t", "b"),
            NotificationOutcome::Unavailable(_)
        ));

        // One discovery served every notification.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_resolution_that_exceeds_the_deadline_is_unavailable() {
        let (release, wait) = mpsc::channel::<()>();
        let wait = std::sync::Mutex::new(wait);
        let resolver = Arc::new(BinResolver::with_discovery("pohunek", move || {
            // Blocks until the test drops the sender at its end.
            let _ = wait.lock().expect("lock").recv();
            Err(BinError::SearchPath("released".to_owned()))
        }));
        let notifier = Notifier {
            backend: NotificationBackend::Command {
                resolution: Arc::new(CommandResolution::new(resolver, "notify-send")),
            },
            timeout: Duration::from_millis(100),
        };

        let outcome = notifier.notify("t", "b");

        assert!(
            matches!(&outcome, NotificationOutcome::Unavailable(reason) if reason.contains("did not finish")),
            "{outcome:?}"
        );
        drop(release);
    }

    #[test]
    fn a_hanging_backend_is_killed_at_the_deadline() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A stopped process never exits by itself; only SIGKILL ends it.
        let (program, _record) = recording_backend(dir.path(), "backend", "kill -STOP $$");

        let outcome = command_notifier(&program, Duration::from_millis(200)).notify("t", "b");

        assert!(
            matches!(&outcome, NotificationOutcome::Unavailable(reason) if reason.contains("killed")),
            "{outcome:?}"
        );
    }

    #[test]
    fn nul_bytes_are_rejected_before_spawning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (program, record) = recording_backend(dir.path(), "backend", "exit 0");

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

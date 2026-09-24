//! Supervises session workers and the daemon agent through launchd.
//!
//! Jobs are registered and retired with the fixed `/bin/launchctl`
//! (`bootstrap`, `bootout`) in the `gui/<uid>` domain. Presence comes only from
//! the exit status of `launchctl print gui/<uid>/<label>`: `0` is loaded, `113`
//! is absent. Human-readable output is never parsed. Everything else a caller
//! learns about a job comes from Pohunek's own evidence: the private definition
//! file written here and Darwin process inspection.
//!
//! Worker definitions live only in the private `<state>/pohunek/launchd/`
//! directory, never in `~/Library/LaunchAgents`, so launchd never resurrects a
//! worker at login. Only the daemon agent is written to `~/Library/LaunchAgents`.
//!
//! The `launchctl` runner and the plist rendering are target-neutral and are
//! unit-tested on every host; the backends themselves exist only on macOS.

use super::Error;

// Rust guideline compliant 2026-09-24

mod launchctl;
mod plist;

#[cfg(target_os = "macos")]
mod backend;

#[cfg(target_os = "macos")]
#[doc(inline)]
pub use backend::{
    Discovery, ExternalVolume, LaunchdDaemon, LaunchdSupervisor, MAX_DISCOVERED_DEFINITIONS,
};

use launchctl::{Completion, RunError, Status};

/// Whether `launchctl print` found a label in its domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    /// `print` exited with 0.
    Loaded,
    /// `print` exited with 113.
    Absent,
}

/// Classifies a `print` result; every other status is an error.
fn presence(
    operation: &'static str,
    domain: &str,
    completion: &Completion,
) -> Result<Presence, Error> {
    match completion.status {
        Status::Success => Ok(Presence::Loaded),
        Status::NoSuchService => Ok(Presence::Absent),
        _ => Err(status_error(operation, domain, completion)),
    }
}

/// Maps a status that the calling operation does not handle itself.
///
/// A missing domain and an operation in progress keep their own variants so
/// callers can report an absent GUI session or retry; anything else is an
/// [`Error::Operation`] whose source carries the bounded diagnostics.
fn status_error(operation: &'static str, domain: &str, completion: &Completion) -> Error {
    match completion.status {
        Status::NoSuchDomain => Error::DomainUnavailable {
            domain: domain.to_owned(),
        },
        Status::InProgress => Error::Race { operation },
        Status::Success
        | Status::NoSuchProcess
        | Status::InputOutput
        | Status::NoSuchService
        | Status::Unmapped => Error::Operation {
            operation,
            source: Box::new(completion.failure()),
        },
    }
}

/// Maps a runner failure that produced no exit status.
fn run_error(operation: &'static str, error: RunError) -> Error {
    match error {
        RunError::Timeout { .. } => Error::Timeout { operation },
        RunError::Spawn { .. } | RunError::Io { .. } => Error::Unavailable {
            operation,
            source: Box::new(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::launchctl::Launchctl;
    use super::*;

    const DOMAIN: &str = "gui/501";

    fn completion(code: i32) -> Completion {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime starts")
            .block_on(
                Launchctl::with_program(
                    Path::new("/bin/sh"),
                    &[
                        "-c",
                        &format!("echo diagnostic >&2; exit {code}"),
                        "launchctl",
                    ],
                    Duration::from_secs(10),
                )
                .run("print", &[]),
            )
            .expect("fake launchctl exits")
    }

    #[test]
    fn print_presence_is_keyed_on_the_exit_status_alone() {
        assert_eq!(
            presence("inspect", DOMAIN, &completion(0)).expect("loaded"),
            Presence::Loaded
        );
        assert_eq!(
            presence("inspect", DOMAIN, &completion(113)).expect("absent"),
            Presence::Absent
        );
        assert!(matches!(
            presence("inspect", DOMAIN, &completion(112)),
            Err(Error::DomainUnavailable { domain }) if domain == DOMAIN
        ));
        assert!(matches!(
            presence("inspect", DOMAIN, &completion(37)),
            Err(Error::Race {
                operation: "inspect"
            })
        ));
        for code in [3, 5, 1, 255] {
            assert!(
                matches!(
                    presence("inspect", DOMAIN, &completion(code)),
                    Err(Error::Operation {
                        operation: "inspect",
                        ..
                    })
                ),
                "status {code}"
            );
        }
    }

    #[test]
    fn unhandled_statuses_keep_bounded_diagnostics_as_the_source() {
        let Error::Operation { source, .. } = status_error("retire", DOMAIN, &completion(42))
        else {
            panic!("unmapped status must be an operation error");
        };
        assert_eq!(
            source.to_string(),
            "launchctl print exited with status 42: diagnostic"
        );
    }

    #[test]
    fn runner_failures_map_to_timeout_or_unavailable() {
        assert!(matches!(
            run_error(
                "start",
                RunError::Timeout {
                    subcommand: "bootstrap"
                }
            ),
            Error::Timeout { operation: "start" }
        ));
        assert!(matches!(
            run_error(
                "start",
                RunError::Spawn {
                    subcommand: "bootstrap",
                    source: std::io::Error::from(std::io::ErrorKind::NotFound),
                }
            ),
            Error::Unavailable {
                operation: "start",
                ..
            }
        ));
    }
}

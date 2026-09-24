use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use rustix::process::Signal;

use super::{
    sweep_runtime, sweep_with, verify, Delivery, Fault, SkipReason, Skipped, SweepError,
    SweepReport, SweepRequest, MAX_RUNTIME_ID_BYTES, MAX_SWEEP_GRACE,
};
use crate::process::{
    Error, ExitWatch, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity, ProcessInspector,
    StartIdentity,
};

const RUNTIME: &str = "runtime-a";
const OWNER: u32 = 501;
/// Far from any PID the test process could have, so a scripted own-PID check
/// never collides with a scripted target.
const NOT_OWN_PID: Pid = 999_999;
const SCRIPTED_GRACE: Duration = Duration::from_millis(50);
const SCRIPTED_POLL: Duration = Duration::from_millis(5);

/// Scripted answer for one process's ownership markers.
#[derive(Debug, Clone)]
enum MarkerAnswer {
    Markers(OwnershipMarkers),
    Denied,
    Unobservable,
    Failed,
}

/// Inspector whose process table, markers, and identities are scripted.
///
/// Each identity query pops the next scripted answer for that PID and repeats
/// the last one, which reproduces PID reuse at an exact point in the sweep.
#[derive(Debug, Default)]
struct ScriptedInspector {
    facts: Vec<ProcessFact>,
    markers: HashMap<Pid, MarkerAnswer>,
    identities: Mutex<HashMap<Pid, VecDeque<Option<ProcessIdentity>>>>,
    running: Mutex<HashSet<ProcessIdentity>>,
    failing_liveness: bool,
}

impl ScriptedInspector {
    fn with_process(mut self, identity: ProcessIdentity, markers: MarkerAnswer) -> Self {
        self.facts.push(ProcessFact {
            pid: identity.pid,
            pgid: identity.pid,
            ppid: 1,
            start_identity: identity.start_identity,
            comm: "agent".to_owned(),
            cmdline: Vec::new(),
        });
        self.markers.insert(identity.pid, markers);
        self.identities
            .get_mut()
            .expect("scripted identities are not poisoned")
            .insert(identity.pid, VecDeque::from([Some(identity)]));
        self.running
            .get_mut()
            .expect("scripted liveness is not poisoned")
            .insert(identity);
        self
    }

    fn with_identities(
        self,
        pid: Pid,
        answers: impl IntoIterator<Item = Option<ProcessIdentity>>,
    ) -> Self {
        self.identities
            .lock()
            .expect("scripted identities are not poisoned")
            .insert(pid, answers.into_iter().collect());
        self
    }

    fn exit(&self, identity: ProcessIdentity) {
        self.running
            .lock()
            .expect("scripted liveness is not poisoned")
            .remove(&identity);
    }
}

impl ProcessInspector for ScriptedInspector {
    fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, Error> {
        let mut identities = self
            .identities
            .lock()
            .expect("scripted identities are not poisoned");
        let Some(answers) = identities.get_mut(&pid) else {
            return Ok(None);
        };
        Ok(if answers.len() > 1 {
            answers.pop_front().flatten()
        } else {
            answers.front().copied().flatten()
        })
    }

    fn is_running(&self, identity: ProcessIdentity) -> Result<bool, Error> {
        if self.failing_liveness {
            return Err(Error::Io {
                operation: "scripted_liveness",
                source: std::io::Error::other("scripted liveness failure"),
            });
        }
        Ok(self
            .running
            .lock()
            .expect("scripted liveness is not poisoned")
            .contains(&identity))
    }

    fn parent_pid(&self, _pid: Pid) -> Result<Option<Pid>, Error> {
        Ok(None)
    }

    fn process(&self, _pid: Pid) -> Result<Option<ProcessFact>, Error> {
        Ok(None)
    }

    fn same_user_processes(&self) -> Result<Vec<ProcessFact>, Error> {
        Ok(self.facts.clone())
    }

    fn descendants(&self, _root: Pid) -> Result<Vec<ProcessFact>, Error> {
        Ok(Vec::new())
    }

    fn cwd(&self, _pid: Pid) -> Result<PathBuf, Error> {
        Err(Error::Unavailable {
            operation: "scripted_cwd",
        })
    }

    fn executable(&self, _pid: Pid) -> Result<Option<PathBuf>, Error> {
        Ok(None)
    }

    fn exit_watch(&self, _identity: ProcessIdentity) -> Result<ExitWatch, Error> {
        Err(Error::Unavailable {
            operation: "scripted_exit_watch",
        })
    }

    fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, Error> {
        match self.markers.get(&pid) {
            None => Err(Error::Race {
                operation: "scripted_markers",
            }),
            Some(MarkerAnswer::Markers(markers)) => Ok(markers.clone()),
            Some(MarkerAnswer::Denied) => Err(Error::PermissionDenied {
                operation: "scripted_markers",
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            }),
            Some(MarkerAnswer::Unobservable) => Err(Error::Unobservable {
                operation: "scripted_markers",
            }),
            Some(MarkerAnswer::Failed) => Err(Error::Io {
                operation: "scripted_markers",
                source: std::io::Error::other("scripted marker failure"),
            }),
        }
    }

    fn foreground_process_group(&self, _root_pid: Pid) -> Result<Option<Pid>, Error> {
        Ok(None)
    }
}

/// Signal sender that never reaches the operating system.
///
/// It applies the production identity recheck, records every signal that
/// would have been sent, and makes a process exit on the signals listed in
/// `fatal`.
#[derive(Debug, Default)]
struct RecordingSender {
    sent: Mutex<Vec<(ProcessIdentity, Signal)>>,
    fatal: Vec<Signal>,
    fail_for: Option<Pid>,
}

impl RecordingSender {
    fn fatal_on(signals: &[Signal]) -> Self {
        Self {
            fatal: signals.to_vec(),
            ..Self::default()
        }
    }

    fn deliver(
        &self,
        inspector: &ScriptedInspector,
        identity: ProcessIdentity,
        signal: Signal,
    ) -> Result<Delivery, Fault> {
        if self.fail_for == Some(identity.pid) {
            return Err(Fault::Signal(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            )));
        }
        if let Some(outcome) = verify(inspector, identity)? {
            return Ok(outcome);
        }
        self.sent
            .lock()
            .expect("recorded signals are not poisoned")
            .push((identity, signal));
        if self.fatal.contains(&signal) {
            inspector.exit(identity);
        }
        Ok(Delivery::Sent)
    }

    fn sent(&self) -> Vec<(ProcessIdentity, Signal)> {
        self.sent
            .lock()
            .expect("recorded signals are not poisoned")
            .clone()
    }
}

async fn scripted_sweep(
    inspector: &ScriptedInspector,
    sender: &RecordingSender,
) -> Result<SweepReport, SweepError> {
    sweep_with(
        inspector,
        &request(RUNTIME),
        OWNER,
        NOT_OWN_PID,
        |_, identity, signal| sender.deliver(inspector, identity, signal),
    )
    .await
}

fn request(runtime_id: &str) -> SweepRequest {
    SweepRequest::new(runtime_id, OWNER, SCRIPTED_GRACE, SCRIPTED_POLL).expect("valid request")
}

fn identity(pid: Pid, start: u64) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_identity: StartIdentity::new(start),
    }
}

fn marked(runtime_id: &str) -> MarkerAnswer {
    MarkerAnswer::Markers(OwnershipMarkers {
        runtime_id: Some(runtime_id.to_owned()),
        ..OwnershipMarkers::default()
    })
}

#[test]
fn request_validation_rejects_invalid_parameters() {
    let valid =
        |runtime_id: &str| SweepRequest::new(runtime_id, OWNER, SCRIPTED_GRACE, SCRIPTED_POLL);
    valid("runtime_01.test-value").expect("identifier alphabet is accepted");
    valid(&"r".repeat(MAX_RUNTIME_ID_BYTES)).expect("the bound is inclusive");
    for invalid in [
        String::new(),
        ".".to_owned(),
        "..".to_owned(),
        "runtime a".to_owned(),
        "runtime/a".to_owned(),
        "runtime\u{0}a".to_owned(),
        "r".repeat(MAX_RUNTIME_ID_BYTES + 1),
    ] {
        assert!(
            matches!(valid(&invalid), Err(SweepError::InvalidRuntimeId)),
            "{invalid:?} must be rejected"
        );
    }
    for grace in [Duration::ZERO, MAX_SWEEP_GRACE + Duration::from_nanos(1)] {
        assert!(matches!(
            SweepRequest::new(RUNTIME, OWNER, grace, SCRIPTED_POLL),
            Err(SweepError::InvalidGrace)
        ));
    }
    for poll in [Duration::ZERO, SCRIPTED_GRACE + Duration::from_nanos(1)] {
        assert!(matches!(
            SweepRequest::new(RUNTIME, OWNER, SCRIPTED_GRACE, poll),
            Err(SweepError::InvalidPoll)
        ));
    }
}

#[tokio::test]
async fn a_foreign_owner_is_rejected_before_inspection() {
    let target = identity(10, 100);
    let inspector = ScriptedInspector::default().with_process(target, MarkerAnswer::Failed);
    let effective_uid = rustix::process::geteuid().as_raw();
    let foreign = SweepRequest::new(
        RUNTIME,
        effective_uid.wrapping_add(1),
        SCRIPTED_GRACE,
        SCRIPTED_POLL,
    )
    .expect("valid request");

    let error = sweep_runtime(&inspector, &foreign)
        .await
        .expect_err("a foreign owner must be rejected");

    assert!(matches!(
        error,
        SweepError::ForeignOwner { owner_uid, effective_uid: actual }
            if owner_uid == effective_uid.wrapping_add(1) && actual == effective_uid
    ));
}

#[tokio::test]
async fn an_inspection_error_during_selection_signals_nothing() {
    let target = identity(10, 100);
    let unreadable = identity(11, 110);
    let inspector = ScriptedInspector::default()
        .with_process(target, marked(RUNTIME))
        .with_process(unreadable, MarkerAnswer::Failed);
    let sender = RecordingSender::fatal_on(&[Signal::TERM, Signal::KILL]);

    let error = scripted_sweep(&inspector, &sender)
        .await
        .expect_err("uncertain evidence must abort the sweep");

    assert!(matches!(
        error,
        SweepError::Inspection { ref progress, .. } if **progress == SweepReport::default()
    ));
    assert!(sender.sent().is_empty());
    assert!(inspector.is_running(target).expect("scripted liveness"));
}

#[tokio::test]
async fn pid_reuse_between_selection_and_signal_is_skipped() {
    let target = identity(10, 100);
    let reused = identity(10, 999);
    let inspector = ScriptedInspector::default()
        .with_process(target, marked(RUNTIME))
        // Selection sees the marked process; the pre-signal check sees a new one.
        .with_identities(target.pid, [Some(target), Some(reused)]);
    let sender = RecordingSender::fatal_on(&[Signal::TERM, Signal::KILL]);

    let report = scripted_sweep(&inspector, &sender).await.expect("sweep");

    assert!(sender.sent().is_empty());
    assert_eq!(
        report.skipped,
        vec![Skipped {
            identity: target,
            reason: SkipReason::IdentityChanged,
        }]
    );
    assert!(report.terminated.is_empty() && report.killed.is_empty());
}

#[tokio::test]
async fn pid_reuse_during_selection_is_skipped() {
    let target = identity(10, 100);
    let inspector = ScriptedInspector::default()
        .with_process(target, marked(RUNTIME))
        .with_identities(target.pid, [Some(identity(10, 999))]);
    let sender = RecordingSender::fatal_on(&[Signal::TERM, Signal::KILL]);

    let report = scripted_sweep(&inspector, &sender).await.expect("sweep");

    assert!(sender.sent().is_empty());
    assert_eq!(
        report.skipped,
        vec![Skipped {
            identity: target,
            reason: SkipReason::IdentityChanged,
        }]
    );
}

#[tokio::test]
async fn a_process_that_vanished_before_the_signal_is_skipped() {
    let target = identity(10, 100);
    let inspector = ScriptedInspector::default()
        .with_process(target, marked(RUNTIME))
        .with_identities(target.pid, [Some(target), None]);
    let sender = RecordingSender::fatal_on(&[Signal::TERM]);

    let report = scripted_sweep(&inspector, &sender).await.expect("sweep");

    assert!(sender.sent().is_empty());
    assert_eq!(
        report.skipped,
        vec![Skipped {
            identity: target,
            reason: SkipReason::Vanished,
        }]
    );
}

#[tokio::test]
async fn the_sweeping_process_is_never_signalled() {
    let own = identity(10, 100);
    let inspector = ScriptedInspector::default().with_process(own, marked(RUNTIME));
    let sender = RecordingSender::fatal_on(&[Signal::TERM]);

    let report = sweep_with(
        &inspector,
        &request(RUNTIME),
        OWNER,
        own.pid,
        |_, id, signal| sender.deliver(&inspector, id, signal),
    )
    .await
    .expect("sweep");

    assert!(sender.sent().is_empty());
    assert_eq!(
        report.skipped,
        vec![Skipped {
            identity: own,
            reason: SkipReason::CurrentProcess,
        }]
    );
}

#[tokio::test]
async fn unreadable_markers_are_reported_and_never_signalled() {
    let target = identity(10, 100);
    let hidden = identity(11, 110);
    let inspector = ScriptedInspector::default()
        .with_process(target, marked(RUNTIME))
        .with_process(hidden, MarkerAnswer::Denied);
    let sender = RecordingSender::fatal_on(&[Signal::TERM]);

    let report = scripted_sweep(&inspector, &sender).await.expect("sweep");

    assert_eq!(sender.sent(), vec![(target, Signal::TERM)]);
    assert_eq!(report.terminated, vec![target]);
    assert_eq!(
        report.skipped,
        vec![Skipped {
            identity: hidden,
            reason: SkipReason::MarkersUnreadable,
        }]
    );
    assert!(inspector.is_running(hidden).expect("scripted liveness"));
}

#[tokio::test]
async fn a_process_between_images_is_skipped_and_the_rest_still_classified() {
    let before = identity(10, 100);
    let exec_ing = identity(11, 110);
    let after = identity(12, 120);
    let inspector = ScriptedInspector::default()
        .with_process(before, marked(RUNTIME))
        .with_process(exec_ing, MarkerAnswer::Unobservable)
        .with_process(after, marked(RUNTIME));
    let sender = RecordingSender::fatal_on(&[Signal::TERM]);

    let report = scripted_sweep(&inspector, &sender).await.expect("sweep");

    assert_eq!(
        sender.sent(),
        vec![(before, Signal::TERM), (after, Signal::TERM)]
    );
    assert_eq!(report.terminated, vec![before, after]);
    assert_eq!(
        report.skipped,
        vec![Skipped {
            identity: exec_ing,
            reason: SkipReason::MarkersUnreadable,
        }]
    );
    assert!(inspector.is_running(exec_ing).expect("scripted liveness"));
}

#[tokio::test]
async fn selection_matches_the_runtime_marker_exactly() {
    let target = identity(10, 100);
    let longer = identity(11, 110);
    let shorter = identity(12, 120);
    let session_only = identity(13, 130);
    let inspector = ScriptedInspector::default()
        .with_process(target, marked(RUNTIME))
        .with_process(longer, marked(&format!("{RUNTIME}B")))
        .with_process(shorter, marked("runtime-"))
        .with_process(
            session_only,
            MarkerAnswer::Markers(OwnershipMarkers {
                session_id: Some(RUNTIME.to_owned()),
                daemon_id: Some(RUNTIME.to_owned()),
                runtime_id: None,
            }),
        );
    let sender = RecordingSender::fatal_on(&[Signal::TERM]);

    let report = scripted_sweep(&inspector, &sender).await.expect("sweep");

    assert_eq!(sender.sent(), vec![(target, Signal::TERM)]);
    assert_eq!(report.terminated, vec![target]);
    assert!(report.skipped.is_empty());
}

#[tokio::test]
async fn a_term_ignoring_process_is_killed_after_the_grace() {
    let target = identity(10, 100);
    let inspector = ScriptedInspector::default().with_process(target, marked(RUNTIME));
    let sender = RecordingSender::fatal_on(&[Signal::KILL]);

    let report = scripted_sweep(&inspector, &sender).await.expect("sweep");

    assert_eq!(
        sender.sent(),
        vec![(target, Signal::TERM), (target, Signal::KILL)]
    );
    assert_eq!(report.killed, vec![target]);
    assert!(report.terminated.is_empty());
    assert!(report.is_complete());
}

#[tokio::test]
async fn a_process_surviving_sigkill_is_unconfirmed() {
    let target = identity(10, 100);
    let inspector = ScriptedInspector::default().with_process(target, marked(RUNTIME));
    let sender = RecordingSender::fatal_on(&[]);

    let report = scripted_sweep(&inspector, &sender).await.expect("sweep");

    assert_eq!(report.unconfirmed, vec![target]);
    assert!(!report.is_complete());
}

#[tokio::test]
async fn a_signal_failure_stops_every_later_signal() {
    let first = identity(10, 100);
    let second = identity(11, 110);
    let third = identity(12, 120);
    let inspector = ScriptedInspector::default()
        .with_process(first, marked(RUNTIME))
        .with_process(second, marked(RUNTIME))
        .with_process(third, marked(RUNTIME));
    let sender = RecordingSender {
        fail_for: Some(second.pid),
        ..RecordingSender::fatal_on(&[Signal::TERM])
    };

    let error = scripted_sweep(&inspector, &sender)
        .await
        .expect_err("a delivery failure must abort");

    assert_eq!(sender.sent(), vec![(first, Signal::TERM)]);
    let SweepError::Signal { progress, .. } = error else {
        panic!("expected a signal failure, got {error:?}");
    };
    assert_eq!(progress.unconfirmed, vec![first]);
    assert_eq!(
        progress.skipped,
        vec![
            Skipped {
                identity: second,
                reason: SkipReason::Aborted,
            },
            Skipped {
                identity: third,
                reason: SkipReason::Aborted,
            },
        ]
    );
}

#[tokio::test]
async fn a_liveness_failure_after_sigterm_prevents_sigkill() {
    let target = identity(10, 100);
    let inspector = ScriptedInspector {
        failing_liveness: true,
        ..ScriptedInspector::default()
    }
    .with_process(target, marked(RUNTIME));
    let sender = RecordingSender::fatal_on(&[]);

    let error = scripted_sweep(&inspector, &sender)
        .await
        .expect_err("a liveness failure must abort");

    assert_eq!(sender.sent(), vec![(target, Signal::TERM)]);
    assert!(matches!(
        error,
        SweepError::Inspection { ref progress, .. } if progress.unconfirmed == vec![target]
    ));
}

/// Sweeps against real processes through the host inspector and real signals.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod host {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use crate::process::{HostInspector, ProcessIdentity, ProcessInspector};

    use super::super::{sweep_runtime, SweepRequest};

    /// Short enough to keep the suite fast, long enough for a loaded runner
    /// to deliver `SIGTERM` and schedule the exiting process.
    const HOST_GRACE: Duration = Duration::from_secs(2);
    const HOST_POLL: Duration = Duration::from_millis(20);
    /// Bound on waiting for a fixture shell to `exec` its final image.
    const READY_TIMEOUT: Duration = Duration::from_secs(10);
    const READY_POLL: Duration = Duration::from_millis(10);

    /// Ignores hangup and termination, like an agent that outlives its PTY.
    const STUBBORN: &str = "trap '' HUP TERM; exec sleep 300";
    /// Exits on the default `SIGTERM` disposition.
    const COMPLIANT: &str = "exec sleep 300";

    /// Spawned fixture that is killed and reaped when dropped.
    #[derive(Debug)]
    struct Fixture {
        child: Child,
        identity: ProcessIdentity,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // The sweep may already have killed the child; only reaping matters.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn spawn(inspector: HostInspector, script: &str, runtime_id: Option<&str>) -> Fixture {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .env_remove("POHUNEK_RUNTIME_ID")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(runtime_id) = runtime_id {
            command.env("POHUNEK_RUNTIME_ID", runtime_id);
        }
        let child = command.spawn().expect("spawn fixture");
        let pid = child.id();
        let mut fixture = Fixture {
            child,
            identity: ProcessIdentity {
                pid,
                start_identity: crate::process::StartIdentity::new(0),
            },
        };
        // The trap must be installed before any signal arrives, so wait until
        // the shell has replaced itself with `sleep`.
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let fact = inspector.process(pid).expect("inspect fixture");
            if let Some(fact) = fact.filter(|fact| {
                fact.cmdline
                    .first()
                    .is_some_and(|argument| argument.ends_with("sleep"))
            }) {
                fixture.identity = fact.identity();
                return fixture;
            }
            assert!(
                Instant::now() < deadline,
                "fixture {pid} never exec'd sleep"
            );
            std::thread::sleep(READY_POLL);
        }
    }

    /// Returns a runtime ID no other test or process uses.
    fn unique_runtime(tag: &str) -> String {
        format!("sweep-test-{}-{tag}", std::process::id())
    }

    fn request(runtime_id: &str) -> SweepRequest {
        SweepRequest::new(
            runtime_id,
            rustix::process::geteuid().as_raw(),
            HOST_GRACE,
            HOST_POLL,
        )
        .expect("valid request")
    }

    fn assert_untouched(inspector: HostInspector, report: &super::SweepReport, fixture: &Fixture) {
        assert!(
            inspector
                .is_running(fixture.identity)
                .expect("inspect survivor"),
            "{:?} must survive the sweep",
            fixture.identity
        );
        let listed = report
            .terminated
            .iter()
            .chain(&report.killed)
            .chain(&report.unconfirmed)
            .chain(report.skipped.iter().map(|skipped| &skipped.identity))
            .any(|identity| *identity == fixture.identity);
        assert!(!listed, "{:?} must not be selected", fixture.identity);
    }

    #[tokio::test]
    async fn a_hangup_ignoring_process_is_killed_and_other_runtimes_survive() {
        let inspector = HostInspector::new();
        let runtime_a = unique_runtime("a");
        let runtime_b = unique_runtime("b");
        let stubborn = spawn(inspector, STUBBORN, Some(&runtime_a));
        let other_runtime = spawn(inspector, STUBBORN, Some(&runtime_b));
        let unmarked = spawn(inspector, STUBBORN, None);

        let report = sweep_runtime(&inspector, &request(&runtime_a))
            .await
            .expect("sweep runtime a");

        assert_eq!(report.killed, vec![stubborn.identity]);
        assert!(report.terminated.is_empty());
        assert!(report.is_complete());
        assert!(!inspector
            .is_running(stubborn.identity)
            .expect("inspect swept process"));
        assert_untouched(inspector, &report, &other_runtime);
        assert_untouched(inspector, &report, &unmarked);
    }

    #[tokio::test]
    async fn a_term_honoring_process_is_terminated_by_sigterm() {
        let inspector = HostInspector::new();
        let runtime = unique_runtime("compliant");
        let compliant = spawn(inspector, COMPLIANT, Some(&runtime));

        let report = sweep_runtime(&inspector, &request(&runtime))
            .await
            .expect("sweep runtime");

        assert_eq!(report.terminated, vec![compliant.identity]);
        assert!(report.killed.is_empty());
        assert!(report.is_complete());
    }

    #[tokio::test]
    async fn a_runtime_id_prefix_does_not_match_a_longer_runtime_id() {
        let inspector = HostInspector::new();
        let runtime = unique_runtime("prefix");
        let exact = spawn(inspector, COMPLIANT, Some(&runtime));
        let longer = spawn(inspector, COMPLIANT, Some(&format!("{runtime}B")));

        let report = sweep_runtime(&inspector, &request(&runtime))
            .await
            .expect("sweep runtime");

        assert_eq!(report.terminated, vec![exact.identity]);
        assert_untouched(inspector, &report, &longer);
    }
}

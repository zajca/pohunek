//! Resource evidence for idle and concurrent PTY sessions.
//!
//! This is its own test binary so descriptor counts and CPU time describe the
//! sessions under test alone, not unrelated tests sharing the process.

// Rust guideline compliant 2026-09-23

use std::path::Path;
use std::time::{Duration, Instant};

use nix::sys::resource::{getrusage, UsageWho};
use pohunek_session_worker::{Command, EnvBase, PtyOwner, WorkerConfig};

/// Idle sessions held open at once.
///
/// Several rather than one, so a per-session leak shows up as a multiple
/// instead of disappearing into noise.
const IDLE_SESSIONS: usize = 4;
/// Window over which the idle process's CPU time is sampled.
///
/// A reader thread spinning on a readiness wake burns nearly the whole window
/// per session, so one second separates spinning from blocking by orders of
/// magnitude.
const IDLE_WINDOW: Duration = Duration::from_secs(1);
/// CPU time the whole process may spend across the idle window.
///
/// Blocked readers spend essentially none. The slack absorbs runtime
/// housekeeping on a loaded CI runner while staying a small fraction of what a
/// single spinning reader would burn.
const IDLE_CPU_BUDGET: Duration = Duration::from_millis(100);
/// Grace each stop gives its root before escalating.
const STOP_GRACE: Duration = Duration::from_millis(200);
/// Bounds every wait on a session reaching a lifecycle state.
const LIFECYCLE_DEADLINE: Duration = Duration::from_secs(5);
/// Pause between descriptor recounts while released handles close.
const RECOUNT_INTERVAL: Duration = Duration::from_millis(10);

fn shell(script: &str) -> Command {
    Command {
        program: "/bin/sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
        base: EnvBase::Inherited,
        env: Vec::new(),
        cwd: std::env::temp_dir(),
        cols: 80,
        rows: 24,
    }
}

fn spawn(config: &WorkerConfig, script: &str) -> PtyOwner {
    PtyOwner::spawn(
        shell(script),
        config.history_bytes,
        config.subscriber_bytes,
        config.input_dedup_entries,
    )
    .expect("spawn PTY")
}

/// Counts this process's open descriptors.
///
/// `/dev/fd` lists them on both Linux and Darwin; the directory handle used to
/// list it is itself one entry and is subtracted.
fn open_descriptors() -> usize {
    std::fs::read_dir(Path::new("/dev/fd"))
        .expect("list open descriptors")
        .count()
        .checked_sub(1)
        .expect("the listing handle is always present")
}

fn process_cpu_time() -> Duration {
    let usage = getrusage(UsageWho::RUSAGE_SELF).expect("read process CPU usage");
    let as_duration = |time: nix::sys::time::TimeVal| {
        Duration::from_secs(u64::try_from(time.tv_sec()).expect("non-negative seconds"))
            + Duration::from_micros(u64::try_from(time.tv_usec()).expect("non-negative micros"))
    };
    as_duration(usage.user_time()) + as_duration(usage.system_time())
}

/// Idle and hung-up sessions neither spin nor leak descriptors.
///
/// The hung-up session's child has exited, so its master reports hangup for
/// good; its reader must end on the resulting EOF instead of waking forever.
/// After every session stops, the process must hold exactly the descriptors it
/// held before the first spawn.
#[tokio::test]
async fn idle_sessions_do_not_spin_or_leak_descriptors() {
    let config = WorkerConfig::new();
    let baseline = open_descriptors();

    let idle: Vec<PtyOwner> = (0..IDLE_SESSIONS)
        .map(|_| spawn(&config, "sleep 30"))
        .collect();
    let hung_up = spawn(&config, "exit 0");
    tokio::time::timeout(LIFECYCLE_DEADLINE, hung_up.wait_exit())
        .await
        .expect("hung-up root exit deadline")
        .expect("hung-up root exit");

    let sessions = IDLE_SESSIONS + 1;
    let held = open_descriptors().saturating_sub(baseline);
    let cpu_before = process_cpu_time();
    tokio::time::sleep(IDLE_WINDOW).await;
    let idle_cpu = process_cpu_time().saturating_sub(cpu_before);
    eprintln!(
        "resource evidence: sessions={sessions} descriptors_held={held} \
         descriptors_per_session={:.1} idle_cpu_ms={} over_ms={}",
        f64::from(u32::try_from(held).expect("descriptor count fits u32"))
            / f64::from(u32::try_from(sessions).expect("session count fits u32")),
        idle_cpu.as_millis(),
        IDLE_WINDOW.as_millis(),
    );
    assert!(
        idle_cpu < IDLE_CPU_BUDGET,
        "{sessions} idle sessions used {idle_cpu:?} of CPU in {IDLE_WINDOW:?}; a reader is spinning"
    );

    for pty in idle.iter().chain(std::iter::once(&hung_up)) {
        tokio::time::timeout(LIFECYCLE_DEADLINE, pty.stop("resources", STOP_GRACE))
            .await
            .expect("stop deadline")
            .expect("stop session");
    }
    drop(idle);
    drop(hung_up);

    let deadline = Instant::now() + LIFECYCLE_DEADLINE;
    let mut remaining = open_descriptors();
    while remaining != baseline && Instant::now() < deadline {
        tokio::time::sleep(RECOUNT_INTERVAL).await;
        remaining = open_descriptors();
    }

    eprintln!("resource evidence: descriptors_before={baseline} descriptors_after={remaining}");
    assert_eq!(
        remaining, baseline,
        "stopped and dropped sessions must release every descriptor they held"
    );
}

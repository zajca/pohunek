//! A version-probe clock for scenarios that reach a probe deadline without waiting.

/// How far [`stepping_probe_clock`] moves on every read: past the 2 s version
/// probe deadline a probe reaches at its first poll, yet small enough that the
/// handful of reads a scenario makes stays far below an aggregate probe budget
/// measured on the same clock.
const PROBE_CLOCK_STEP: std::time::Duration = std::time::Duration::from_secs(3);

/// A version-probe clock that moves forward by [`PROBE_CLOCK_STEP`] on every
/// read, so a probe that never exits reaches its full deadline without the
/// test waiting for it. A reader that measures a budget on it spends a budget
/// of seconds after a few reads, while a large aggregate budget still outlasts
/// the probes of one scenario.
pub(crate) fn stepping_probe_clock() -> pohunek_daemon::capabilities::ProbeClock {
    let start = std::time::Instant::now();
    let reads = std::sync::atomic::AtomicU32::new(0);
    pohunek_daemon::capabilities::ProbeClock::new(move || {
        start + PROBE_CLOCK_STEP * reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    })
}

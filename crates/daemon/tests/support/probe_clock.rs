//! A version-probe clock for scenarios that reach a probe deadline without waiting.

/// How far [`stepping_probe_clock`] moves on every read: past any version
/// probe deadline, so the first poll of a probe finds its deadline passed.
const PROBE_CLOCK_STEP: std::time::Duration = std::time::Duration::from_secs(3600);

/// A version-probe clock that moves forward by [`PROBE_CLOCK_STEP`] on every
/// read, so a probe that never exits reaches its full deadline without the
/// test waiting for it.
pub(crate) fn stepping_probe_clock() -> pohunek_daemon::capabilities::ProbeClock {
    let start = std::time::Instant::now();
    let reads = std::sync::atomic::AtomicU32::new(0);
    pohunek_daemon::capabilities::ProbeClock::new(move || {
        start + PROBE_CLOCK_STEP * reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    })
}

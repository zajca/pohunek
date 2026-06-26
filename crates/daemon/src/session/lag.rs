//! Detector PTY-output-lag WARN rate limiting.

use super::{warn, Duration, Instant, SessionId};

/// Rate limiter for the per-session "PTY output lag" WARN.
///
/// A runaway session (e.g. a self-feeding attach loop, before the
/// [`attach_self_feedback`] guard catches it, or any other output storm)
/// overflows the detector's bounded broadcast channel continuously, so logging
/// every overflow would bury the log. The first lag of a *storm* logs
/// immediately; every further lag less than `interval` after the previous one is
/// folded into the storm and reported as ONE summary per window — flushed by
/// [`Self::poll`] (the detector's periodic tick, once the window elapses) or by
/// [`Self::flush`] (session teardown), so a quiesced or killed-mid-storm session
/// still reports its trailing batch instead of silently dropping it. A lag that
/// arrives at least `interval` after the previous one starts a *new* storm and so
/// logs immediately again. At most one line per `interval`. It is pure and
/// `Instant`-fed, so it is unit-testable without real time; the detector still
/// calls `resync_after_lag()` on every lag, so only the logging is throttled,
/// never the recovery.
#[derive(Debug)]
pub(super) struct LagWarnThrottle {
    interval: Duration,
    /// Start of the current summary window; `None` when no window is open (before
    /// the first lag, and after a window is flushed/closed). Drives [`Self::poll`]
    /// timing only — *not* the first-vs-fold decision, which uses
    /// [`Self::last_lag_at`] so a fresh storm after a quiet gap still logs a
    /// `First` even while a previous window is technically still open.
    window_started: Option<Instant>,
    /// Instant of the most recently observed lag; `None` until the first. Used to
    /// decide whether a lag continues the current storm (gap `< interval`) or
    /// starts a new one (gap `>= interval`).
    last_lag_at: Option<Instant>,
    /// Lag events folded into the current window since its first (already-logged)
    /// lag.
    pending_events: u64,
    /// Total chunks skipped across [`Self::pending_events`].
    pending_skipped: u64,
}

/// A line the detector loop should log for PTY output lag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LagWarn {
    /// First lag of a storm: log it immediately with its own skip count.
    First { skipped: u64 },
    /// One summary of the lags folded into a window after its first.
    Summary { events: u64, skipped: u64 },
}

impl LagWarnThrottle {
    pub(super) fn new(interval: Duration) -> Self {
        Self {
            interval,
            window_started: None,
            last_lag_at: None,
            pending_events: 0,
            pending_skipped: 0,
        }
    }

    /// Record one lag observed at `now` that dropped `skipped` chunks.
    ///
    /// Returns the line to log, if any: the first lag of a storm logs immediately;
    /// later lags (less than `interval` after the previous one) fold silently and
    /// are reported by [`Self::poll`] or [`Self::flush`]. A lag at least `interval`
    /// after the previous one is treated as a new storm and logs immediately. A
    /// zero interval disables folding entirely (every lag logs immediately).
    pub(super) fn observe(&mut self, now: Instant, skipped: u64) -> Option<LagWarn> {
        if self.interval.is_zero() {
            return Some(LagWarn::First { skipped });
        }
        // A lag continues the current storm only if the previous lag is less than
        // one interval old; otherwise (no prior lag, or a long quiet gap) it is the
        // first lag of a fresh storm and must log immediately. Keying this on the
        // last lag — not on `window_started` — means a new burst after silence is
        // never mislabeled as a continuation just because `poll` left a window open.
        let continues_storm = match self.last_lag_at {
            Some(last) => now.saturating_duration_since(last) < self.interval,
            None => false,
        };
        self.last_lag_at = Some(now);
        if continues_storm {
            self.pending_events = self.pending_events.saturating_add(1);
            self.pending_skipped = self.pending_skipped.saturating_add(skipped);
            if self.window_started.is_none() {
                self.window_started = Some(now);
            }
            None
        } else {
            self.window_started = Some(now);
            self.pending_events = 0;
            self.pending_skipped = 0;
            Some(LagWarn::First { skipped })
        }
    }

    /// Flush a window whose `interval` has elapsed, called on the detector's
    /// periodic tick so a folded batch is reported even when no further lag
    /// arrives. Emits a summary if any lags were folded, then opens a fresh
    /// window; if the elapsed window folded nothing it is simply closed (its first
    /// lag was already logged), so the next lag logs as a fresh `First`.
    pub(super) fn poll(&mut self, now: Instant) -> Option<LagWarn> {
        let started = self.window_started?;
        if now.saturating_duration_since(started) < self.interval {
            return None;
        }
        if self.pending_events > 0 {
            let summary = LagWarn::Summary {
                events: self.pending_events,
                skipped: self.pending_skipped,
            };
            self.window_started = Some(now);
            self.pending_events = 0;
            self.pending_skipped = 0;
            Some(summary)
        } else {
            self.window_started = None;
            None
        }
    }

    /// Flush any folded lags unconditionally (used at session teardown, when the
    /// window may never elapse because the session died mid-storm). Emits the
    /// trailing summary if any lags were folded, then resets.
    pub(super) fn flush(&mut self) -> Option<LagWarn> {
        (self.pending_events > 0).then(|| {
            let summary = LagWarn::Summary {
                events: self.pending_events,
                skipped: self.pending_skipped,
            };
            self.window_started = None;
            self.pending_events = 0;
            self.pending_skipped = 0;
            summary
        })
    }
}

/// Emit the WARN line for one [`LagWarn`] decision, tagged with the session id.
pub(super) fn log_lag_warn(session_id: &SessionId, warn_kind: LagWarn) {
    match warn_kind {
        LagWarn::First { skipped } => warn!(
            session_id = %session_id.0,
            skipped,
            "resyncing detector state after PTY output lag"
        ),
        LagWarn::Summary { events, skipped } => warn!(
            session_id = %session_id.0,
            lag_events = events,
            skipped,
            "PTY output lag persisting; detector kept resyncing (summary since last log)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{Duration, Instant, LagWarn, LagWarnThrottle};

    #[test]
    fn lag_warn_throttle_logs_first_then_flushes_one_summary_per_window() {
        let interval = Duration::from_secs(5);
        let mut throttle = LagWarnThrottle::new(interval);
        let t0 = Instant::now();

        // First lag of a window logs immediately with its own skip count.
        assert_eq!(throttle.observe(t0, 2), Some(LagWarn::First { skipped: 2 }));
        // Further lags inside the window fold silently.
        assert_eq!(throttle.observe(t0 + Duration::from_secs(1), 3), None);
        assert_eq!(throttle.observe(t0 + Duration::from_secs(2), 4), None);
        // A tick before the window elapses flushes nothing.
        assert_eq!(throttle.poll(t0 + Duration::from_secs(3)), None);
        // Once the window elapses, the tick flushes ONE summary of the folded lags
        // (events 2 = the two folded; skipped 3 + 4), excluding the already-logged
        // first.
        assert_eq!(
            throttle.poll(t0 + interval),
            Some(LagWarn::Summary {
                events: 2,
                skipped: 7,
            })
        );
        // The window reopens at the flush; with nothing folded yet, the next tick
        // past the interval just closes it (no spurious summary)...
        assert_eq!(throttle.poll(t0 + interval + interval), None);
        // ...and the next lag after a closed window logs as a fresh First.
        assert_eq!(
            throttle.observe(t0 + interval + interval + Duration::from_secs(1), 9),
            Some(LagWarn::First { skipped: 9 })
        );
    }

    #[test]
    fn lag_warn_throttle_flush_emits_trailing_batch_on_teardown() {
        let interval = Duration::from_secs(5);
        let mut throttle = LagWarnThrottle::new(interval);
        let t0 = Instant::now();

        assert_eq!(throttle.observe(t0, 1), Some(LagWarn::First { skipped: 1 }));
        assert_eq!(throttle.observe(t0 + Duration::from_secs(1), 2), None);
        assert_eq!(throttle.observe(t0 + Duration::from_secs(2), 3), None);
        // Session torn down mid-window: flush reports the folded tail (events 2,
        // skipped 2 + 3) instead of silently dropping it.
        assert_eq!(
            throttle.flush(),
            Some(LagWarn::Summary {
                events: 2,
                skipped: 5,
            })
        );
        // A second flush with nothing pending is a no-op.
        assert_eq!(throttle.flush(), None);
    }

    #[test]
    fn lag_warn_throttle_zero_interval_logs_every_lag() {
        let mut throttle = LagWarnThrottle::new(Duration::ZERO);
        let t0 = Instant::now();
        // Folding disabled: every lag logs immediately, never folded or dropped.
        assert_eq!(throttle.observe(t0, 1), Some(LagWarn::First { skipped: 1 }));
        assert_eq!(throttle.observe(t0, 7), Some(LagWarn::First { skipped: 7 }));
        assert_eq!(throttle.flush(), None);
    }

    #[test]
    fn lag_warn_throttle_relogs_first_after_a_quiet_gap() {
        let interval = Duration::from_secs(5);
        let mut throttle = LagWarnThrottle::new(interval);
        let t0 = Instant::now();

        // A storm: first lag logged, a second folded within the window.
        assert_eq!(throttle.observe(t0, 1), Some(LagWarn::First { skipped: 1 }));
        assert_eq!(throttle.observe(t0 + Duration::from_secs(1), 2), None);
        // The window elapses and the tick flushes the folded lag, reopening it.
        assert_eq!(
            throttle.poll(t0 + interval),
            Some(LagWarn::Summary {
                events: 1,
                skipped: 2,
            })
        );

        // A long silence, then a brand-new lag more than an interval after the
        // previous one: it is a fresh storm and must log as a First again — never
        // folded/mislabeled as a continuation just because poll left a window open.
        let later = t0 + interval + interval + Duration::from_secs(1);
        assert_eq!(
            throttle.observe(later, 4),
            Some(LagWarn::First { skipped: 4 })
        );
    }
}

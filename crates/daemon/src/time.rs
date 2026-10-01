//! Shared time helpers: the daemon monotonic clock and RFC3339 formatting.

use ::time::format_description::well_known::Rfc3339;
use ::time::OffsetDateTime;

/// Current UTC time as an RFC3339 string for persisted daemon metadata.
///
/// Uses `now_utc()` because resolving the local offset can fail. Formatting a
/// valid `OffsetDateTime` as RFC3339 cannot fail in practice; the fallback only
/// guards against a future API change.
#[must_use]
pub(crate) fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// Current monotonic instant for session state that crosses async tasks.
///
/// Reads tokio's clock and converts it to a [`std::time::Instant`], so detector
/// ticks, procwatch claim TTLs and cwd evidence ordering follow the same clock as
/// the `tokio::time` timers that drive them. Without a paused clock this is
/// the std monotonic clock: tokio's `Instant::now` falls back to
/// `std::time::Instant::now` when `test-util` is disabled, when the clock was
/// never paused, and when called outside a runtime. Under `start_paused` or
/// `tokio::time::pause` the value advances only with `tokio::time::advance`.
///
/// Every producer and consumer of one compared instant must read this clock;
/// comparing it with a raw `std::time::Instant::now()` is meaningless while the
/// tokio clock is paused.
#[must_use]
pub(crate) fn now() -> std::time::Instant {
    tokio::time::Instant::now().into_std()
}

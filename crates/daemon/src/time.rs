//! Shared time formatting helpers.

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

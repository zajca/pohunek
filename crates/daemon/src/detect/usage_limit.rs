//! Passive screen evidence for a provider usage limit.
//!
//! A usage limit cannot be triggered on demand, so the detector names the
//! usage-limit screen it sees and lets the session loop journal the OSC 7501
//! evidence held at that moment (#2101). The journal changes no activity, no
//! attention and no input readiness: it exists so the next real limit hit
//! leaves evidence in the daemon log.

// Rust guideline compliant 2026-11-25

/// Most usage-limit screens one detector journals.
///
/// The sighting is diagnostics, and a transcript can quote a marker phrase at
/// will, so the detector bounds how many lines one session can write. After
/// the cap the detector queues one suppression line and journals no further
/// sighting, while the re-armed latch keeps the screen diagnostic silent.
pub const MAX_LIMIT_SIGHTINGS: usize = 16;

/// Phrases that name a provider usage limit on screen.
///
/// The list is deliberately short and specific: a phrase a transcript could
/// quote costs at most one diagnostic log line up to `MAX_LIMIT_SIGHTINGS` per
/// detector, never a state change. A provider that words its limit differently
/// is a prompt to extend this list with the captured screen. Matching is ASCII
/// case-insensitive over the visible screen text.
const USAGE_LIMIT_MARKERS: [&str; 8] = [
    "usage limit reached",
    "hit your usage limit",
    "hit your limit",
    "usage limit will reset",
    "usage limit resets",
    "5-hour limit",
    "weekly limit",
    "usage limit",
];

/// Returns the first usage-limit phrase `text` names.
///
/// The specific phrases come first so the journal names the most informative
/// one; the bare `usage limit` last catches a wording the list does not know
/// yet. `None` means the screen shows no known usage-limit text.
#[must_use]
pub(crate) fn marker(text: &str) -> Option<&'static str> {
    let lowered = text.to_ascii_lowercase();
    USAGE_LIMIT_MARKERS
        .iter()
        .copied()
        .find(|marker| lowered.contains(marker))
}

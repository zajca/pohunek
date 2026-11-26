//! Passive screen evidence for a provider usage limit.
//!
//! A usage limit cannot be triggered on demand, so the detector names the
//! usage-limit screen it sees and lets the session loop journal the OSC 7501
//! evidence held at that moment (#2101). The journal changes no activity, no
//! attention and no input readiness: it exists so the next real limit hit
//! leaves evidence in the daemon log.

// Rust guideline compliant 2026-11-25

/// Most usage-limit screens naming a generic phrase one detector journals.
///
/// The sighting is diagnostics, and a transcript can quote a bare phrase at
/// will, so the detector bounds how many lines one runtime launch can write.
/// After the cap the detector queues one suppression line for the class and
/// journals no further generic sighting, while the re-armed latch keeps the
/// screen diagnostic silent. The specific phrases have their own budget, so
/// quoted bare phrases cannot spend it.
pub const MAX_GENERIC_LIMIT_SIGHTINGS: usize = 16;

/// Most usage-limit screens naming a specific phrase one detector journals.
///
/// Only the specific phrases spend this budget, so a real limit hit is still
/// recorded after a transcript quoted bare phrases many times.
pub const MAX_SPECIFIC_LIMIT_SIGHTINGS: usize = 16;

/// How informative the phrase a usage-limit screen named is.
///
/// The order is the rank the sighting latch compares: a screen that first
/// names a generic phrase and later a specific one is sighted again, a screen
/// that falls back to a generic phrase is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MarkerClass {
    /// A bare phrase (`usage limit`, `weekly limit`) a transcript quotes easily.
    Generic,
    /// A phrase that states the limit was hit or when it resets.
    Specific,
}

impl MarkerClass {
    /// Most sightings one detector journals for this class.
    #[must_use]
    pub const fn budget(self) -> usize {
        match self {
            Self::Generic => MAX_GENERIC_LIMIT_SIGHTINGS,
            Self::Specific => MAX_SPECIFIC_LIMIT_SIGHTINGS,
        }
    }

    /// The token the journal writes for the class.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Specific => "specific",
        }
    }
}

/// Phrases that name a provider usage limit on screen, with their class.
///
/// The list is deliberately short: a phrase a transcript could quote costs at
/// most one diagnostic log line up to the class budget per detector, never a
/// state change. The specific phrases come first so the journal names the most
/// informative one; the bare phrases last catch a wording the list does not
/// know yet. A provider that words its limit differently is a prompt to extend
/// this list with the captured screen. Matching is ASCII case-insensitive over
/// the visible screen text.
const USAGE_LIMIT_MARKERS: [(&str, MarkerClass); 8] = [
    ("usage limit reached", MarkerClass::Specific),
    ("hit your usage limit", MarkerClass::Specific),
    ("hit your limit", MarkerClass::Specific),
    ("usage limit will reset", MarkerClass::Specific),
    ("usage limit resets", MarkerClass::Specific),
    ("5-hour limit", MarkerClass::Specific),
    ("weekly limit", MarkerClass::Generic),
    ("usage limit", MarkerClass::Generic),
];

/// Returns the first usage-limit phrase `text` names, with its class.
///
/// `None` means the screen shows no known usage-limit text.
#[must_use]
pub(crate) fn marker(text: &str) -> Option<(&'static str, MarkerClass)> {
    let lowered = text.to_ascii_lowercase();
    USAGE_LIMIT_MARKERS
        .iter()
        .copied()
        .find(|(marker, _)| lowered.contains(marker))
}

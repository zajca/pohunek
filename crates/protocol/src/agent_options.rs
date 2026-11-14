//! Agent profile options as `host.inspect` reports them.
//!
//! A host profile may declare options: closed enumerations of launch choices
//! (a model, an effort level, a permission mode) with a default each. A
//! `session.new` selects choices by id through
//! [`SessionNewParams::agent_options`](crate::SessionNewParams::agent_options);
//! every value a choice adds to the launch stays in the profile file on the
//! host, so these types carry ids, labels and availability only.

// Rust guideline compliant 2026-11-14

use serde::{Deserialize, Serialize};

/// One option a host profile declares, in the profile's option-id order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "AgentOption.ts"))]
pub struct AgentOption {
    /// Option id, the key a `session.new` selection uses.
    pub id: String,
    /// Picker label.
    pub label: String,
    /// Id of the choice a launch takes when the selection does not name the
    /// option.
    pub default: String,
    /// The choices, in choice-id order.
    pub choices: Vec<AgentOptionChoice>,
}

/// One choice of an [`AgentOption`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "AgentOptionChoice.ts"))]
pub struct AgentOptionChoice {
    /// Choice id, the value a `session.new` selection uses.
    pub id: String,
    /// Picker entry label.
    pub label: String,
    /// Whether a launch may select the choice now. A choice whose
    /// `min_runtime` the runtime does not meet, or whose runtime reports no
    /// version, is unavailable and `session.new` refuses it with
    /// `agent_option_choice_unavailable`.
    pub available: bool,
    /// Why the choice is unavailable; absent when it is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub reason: Option<AgentOptionUnavailable>,
    /// Lowest runtime version the choice needs, as the profile declares it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub min_runtime: Option<String>,
}

/// Why an [`AgentOptionChoice`] is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", remote = "Self")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "AgentOptionUnavailable.ts"))]
pub enum AgentOptionUnavailable {
    /// The runtime's probed version is lower than the choice's
    /// `min_runtime`.
    RuntimeTooOld,
    /// The runtime reports no version (it declares no version probe, is the
    /// host shell, or its probe gave none), so a floor cannot be met.
    RuntimeVersionUnknown,
    /// A value a newer daemon sent that this build does not know. Only the
    /// client's [`tolerant`](crate::tolerant) decode produces it; plain decoding
    /// rejects the value and the daemon never holds or emits one.
    #[doc(hidden)]
    #[serde(rename = "unknown", skip_deserializing)]
    #[cfg_attr(feature = "ts", ts(skip))]
    Unknown,
}

crate::tolerant_enum!(AgentOptionUnavailable);

impl std::fmt::Display for AgentOptionUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RuntimeTooOld => "runtime_too_old",
            Self::RuntimeVersionUnknown => "runtime_version_unknown",
            Self::Unknown => "unknown",
        })
    }
}

//! Typed payloads for assistant-specific protocol methods.

use serde::{Deserialize, Serialize};

/// Parameters for `assistant.materialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "AssistantMaterializeParams.ts")
)]
pub struct AssistantMaterializeParams {
    /// Redacted live snapshot JSON to persist beside the materialized bundle.
    pub snapshot: String,
}

/// Result returned by `assistant.materialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "AssistantMaterializeResult.ts")
)]
pub struct AssistantMaterializeResult {
    /// Host-local path to the materialized assistant knowledge bundle.
    pub bundle_path: String,
    /// Host-local path to the persisted redacted snapshot.
    pub snapshot_path: String,
    /// Pohunek version that produced the embedded bundle.
    pub version: String,
    /// Stable content hash for the materialized bundle.
    pub content_hash: String,
    /// Allowlisted concept metadata used by callers to build the assistant TOC.
    pub concepts: Vec<ConceptMeta>,
}

/// Public-safe concept metadata exposed through the protocol.
///
/// This mirrors the allowlisted knowledge bundle index fields without depending
/// on the knowledge crate, keeping the protocol contract self-contained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ConceptMeta.ts"))]
pub struct ConceptMeta {
    /// Concept type from the knowledge frontmatter.
    #[serde(rename = "type")]
    pub r#type: ConceptType,
    /// Stable concept id.
    pub id: String,
    /// Human-readable concept title.
    pub title: String,
    /// Short concept description.
    pub description: String,
    /// Assistant intents this concept can support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub intents: Option<Vec<ConceptIntent>>,
    /// First Pohunek version this concept applies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub since: Option<String>,
    /// Last Pohunek version where this concept changed materially.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub changed_in: Option<Vec<String>>,
    /// Whether the concept is deprecated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub deprecated: Option<ConceptDeprecation>,
}

/// Protocol-local copy of the knowledge concept type enum.
///
/// Mirrors `knowledge::ConceptType`; the two are bridged in the knowledge
/// crate's `protocol_conversions.rs` and guarded by a parity test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ConceptType.ts"))]
#[serde(rename_all = "PascalCase")]
pub enum ConceptType {
    /// General explanatory concept.
    Concept,
    /// Step-by-step how-to guide.
    Guide,
    /// Operational runbook.
    Runbook,
    /// Troubleshooting reference.
    Troubleshooting,
    /// Safety policy the assistant must enforce.
    SafetyPolicy,
    /// Documentation for a CLI command.
    CliCommand,
    /// Documentation for a configuration option.
    ConfigReference,
    /// Documentation for a protocol method.
    ProtocolMethod,
    /// Documentation for a protocol event.
    ProtocolEvent,
    /// Asset used during environment setup.
    SetupAsset,
    /// Reusable prompt template.
    PromptTemplate,
    /// Mapping from concepts to their source material.
    SourceMap,
    /// Template for live snapshot content.
    SnapshotTemplate,
    /// Release note entry.
    ReleaseNote,
}

/// Protocol-local copy of assistant intent names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ConceptIntent.ts"))]
#[serde(rename_all = "kebab-case")]
pub enum ConceptIntent {
    Setup,
    Project,
    Update,
    Debug,
    Help,
}

/// Deprecation metadata exposed for concepts that describe retired behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ConceptDeprecation.ts"))]
#[serde(untagged)]
pub enum ConceptDeprecation {
    Version(String),
    Details {
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        successor: Option<String>,
    },
}

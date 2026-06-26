//! Schema types for markdown knowledge concepts.

use serde::{Deserialize, Serialize};

/// Schema version for concept frontmatter understood by this crate.
pub const CONCEPT_SCHEMA_VERSION: u32 = 1;

/// Frontmatter attached to a non-reserved markdown concept file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Concept {
    /// Concept classification driving routing and validation rules.
    #[serde(rename = "type")]
    pub r#type: ConceptType,
    /// Stable, unique concept identifier.
    pub id: String,
    /// Human-readable concept title.
    pub title: String,
    /// Short concept description.
    pub description: String,
    /// Whether the concept is manual, generated, or a snapshot template.
    pub source_kind: SourceKind,
    /// Free-form tags for grouping and search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Assistant intents this concept can support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intents: Option<Vec<Intent>>,
    /// Source path or identifier a generated concept came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_from: Option<String>,
    /// First Pohunek version this concept applies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Versions where this concept changed materially.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_in: Option<Vec<String>>,
    /// Deprecation metadata when the concept is retired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<Deprecation>,
    /// Supporting citations for the concept content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<Citation>>,
}

/// Deprecation metadata for behavior-bearing concepts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Deprecation {
    Version(String),
    Details {
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        successor: Option<String>,
    },
}

/// Closed set of concept types accepted by the Phase 1 schema.
///
/// This enum is mirrored by `protocol::ConceptType` for the wire contract;
/// the two are bridged in `protocol_conversions.rs`. Keep both in sync — a
/// parity test guards against silent drift when a variant is added.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
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

impl ConceptType {
    #[must_use]
    pub const fn requires_since(self) -> bool {
        matches!(
            self,
            Self::CliCommand
                | Self::ConfigReference
                | Self::ProtocolMethod
                | Self::ProtocolEvent
                | Self::Runbook
        )
    }
}

/// Source class for a concept.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// Hand-authored content.
    Manual,
    /// Content produced by a generator.
    Generated,
    /// Template rendered against a live snapshot.
    SnapshotTemplate,
}

/// Assistant intents a concept can support.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Intent {
    /// Initial environment setup.
    Setup,
    /// Working within a project.
    Project,
    /// Updating an existing setup.
    Update,
    /// Diagnosing a problem.
    Debug,
    /// General help and orientation.
    Help,
}

/// Citation metadata. String citations are accepted for simple manual content;
/// structured citations can carry title and URL when generators provide them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Citation {
    Text(String),
    Link { title: String, url: String },
}

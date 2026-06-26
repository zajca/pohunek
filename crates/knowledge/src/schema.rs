//! Schema types for markdown knowledge concepts.

use serde::{Deserialize, Serialize};

/// Schema version for concept frontmatter understood by this crate.
pub const CONCEPT_SCHEMA_VERSION: u32 = 1;

/// Frontmatter attached to a non-reserved markdown concept file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Concept {
    #[serde(rename = "type")]
    pub r#type: ConceptType,
    pub id: String,
    pub title: String,
    pub description: String,
    pub source_kind: SourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intents: Option<Vec<Intent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_in: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<Deprecation>,
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum ConceptType {
    Concept,
    Guide,
    Runbook,
    Troubleshooting,
    SafetyPolicy,
    CliCommand,
    ConfigReference,
    ProtocolMethod,
    ProtocolEvent,
    SetupAsset,
    PromptTemplate,
    SourceMap,
    SnapshotTemplate,
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
    Manual,
    Generated,
    SnapshotTemplate,
}

/// Assistant intents a concept can support.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Intent {
    Setup,
    Project,
    Update,
    Debug,
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

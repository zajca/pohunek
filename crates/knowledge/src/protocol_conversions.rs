//! Protocol conversions for assistant concept metadata.

use crate::{ConceptMeta, ConceptType, Deprecation, Intent};

impl From<ConceptMeta> for protocol::ConceptMeta {
    fn from(meta: ConceptMeta) -> Self {
        Self {
            r#type: meta.r#type.into(),
            id: meta.id,
            title: meta.title,
            description: meta.description,
            intents: meta
                .intents
                .map(|intents| intents.into_iter().map(Into::into).collect()),
            since: meta.since,
            changed_in: meta.changed_in,
            deprecated: meta.deprecated.map(Into::into),
        }
    }
}

impl From<ConceptType> for protocol::ConceptType {
    fn from(value: ConceptType) -> Self {
        match value {
            ConceptType::Concept => Self::Concept,
            ConceptType::Guide => Self::Guide,
            ConceptType::Runbook => Self::Runbook,
            ConceptType::Troubleshooting => Self::Troubleshooting,
            ConceptType::SafetyPolicy => Self::SafetyPolicy,
            ConceptType::CliCommand => Self::CliCommand,
            ConceptType::ConfigReference => Self::ConfigReference,
            ConceptType::ProtocolMethod => Self::ProtocolMethod,
            ConceptType::ProtocolEvent => Self::ProtocolEvent,
            ConceptType::SetupAsset => Self::SetupAsset,
            ConceptType::PromptTemplate => Self::PromptTemplate,
            ConceptType::SourceMap => Self::SourceMap,
            ConceptType::SnapshotTemplate => Self::SnapshotTemplate,
            ConceptType::ReleaseNote => Self::ReleaseNote,
        }
    }
}

impl From<Intent> for protocol::ConceptIntent {
    fn from(value: Intent) -> Self {
        match value {
            Intent::Setup => Self::Setup,
            Intent::Project => Self::Project,
            Intent::Update => Self::Update,
            Intent::Debug => Self::Debug,
            Intent::Help => Self::Help,
        }
    }
}

impl From<Deprecation> for protocol::ConceptDeprecation {
    fn from(value: Deprecation) -> Self {
        match value {
            Deprecation::Version(version) => Self::Version(version),
            Deprecation::Details { version, successor } => Self::Details { version, successor },
        }
    }
}

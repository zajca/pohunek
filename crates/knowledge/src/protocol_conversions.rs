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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every [`ConceptType`] variant. Listed explicitly so the exhaustive match
    /// below fails to compile if a variant is added without updating it, and so
    /// the parity test iterates the full set.
    const ALL_KNOWLEDGE_CONCEPT_TYPES: &[ConceptType] = &[
        ConceptType::Concept,
        ConceptType::Guide,
        ConceptType::Runbook,
        ConceptType::Troubleshooting,
        ConceptType::SafetyPolicy,
        ConceptType::CliCommand,
        ConceptType::ConfigReference,
        ConceptType::ProtocolMethod,
        ConceptType::ProtocolEvent,
        ConceptType::SetupAsset,
        ConceptType::PromptTemplate,
        ConceptType::SourceMap,
        ConceptType::SnapshotTemplate,
        ConceptType::ReleaseNote,
    ];

    /// Map a protocol concept type back to the knowledge enum.
    ///
    /// This mirror of [`From<ConceptType> for protocol::ConceptType`] is an
    /// exhaustive `match`, so adding a variant to either enum without updating
    /// the taxonomy bridge stops compilation here — the desync guard.
    fn knowledge_concept_type_from(value: protocol::ConceptType) -> ConceptType {
        match value {
            protocol::ConceptType::Concept => ConceptType::Concept,
            protocol::ConceptType::Guide => ConceptType::Guide,
            protocol::ConceptType::Runbook => ConceptType::Runbook,
            protocol::ConceptType::Troubleshooting => ConceptType::Troubleshooting,
            protocol::ConceptType::SafetyPolicy => ConceptType::SafetyPolicy,
            protocol::ConceptType::CliCommand => ConceptType::CliCommand,
            protocol::ConceptType::ConfigReference => ConceptType::ConfigReference,
            protocol::ConceptType::ProtocolMethod => ConceptType::ProtocolMethod,
            protocol::ConceptType::ProtocolEvent => ConceptType::ProtocolEvent,
            protocol::ConceptType::SetupAsset => ConceptType::SetupAsset,
            protocol::ConceptType::PromptTemplate => ConceptType::PromptTemplate,
            protocol::ConceptType::SourceMap => ConceptType::SourceMap,
            protocol::ConceptType::SnapshotTemplate => ConceptType::SnapshotTemplate,
            protocol::ConceptType::ReleaseNote => ConceptType::ReleaseNote,
        }
    }

    #[test]
    fn concept_type_round_trips_through_protocol() {
        for &concept_type in ALL_KNOWLEDGE_CONCEPT_TYPES {
            let protocol_type: protocol::ConceptType = concept_type.into();
            assert_eq!(
                knowledge_concept_type_from(protocol_type),
                concept_type,
                "ConceptType {concept_type:?} must round-trip through protocol unchanged",
            );
        }
    }
}

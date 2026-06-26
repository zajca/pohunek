//! Knowledge bundle primitives for the universal pohunek assistant.

#![forbid(unsafe_code)]

pub mod assistant;
mod index;
mod materializer;
#[cfg(feature = "protocol")]
mod protocol_conversions;
mod schema;
mod validation;

pub use assistant::{
    assistant_launch_id, bundle_content_hash, embedded_bundle, materialized_version_hash,
    sha256_for_bytes, EmbeddedBundle, BUNDLE_VERSION,
};
pub use index::{bundle_index, BundleIndexError, ConceptMeta};
pub use materializer::{gc, materialize};
pub use schema::{
    Citation, Concept, ConceptType, Deprecation, Intent, SourceKind, CONCEPT_SCHEMA_VERSION,
};
pub use validation::{
    validate_bundle, BundleValidationError, BundleValidationIssue, BundleValidationReport,
};

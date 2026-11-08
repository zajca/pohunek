//! Knowledge bundle primitives for the universal pohunek assistant.

#![forbid(unsafe_code)]

pub mod assistant;
mod index;
pub mod log;
mod prompt_contract;
#[cfg(feature = "protocol")]
mod protocol_conversions;
mod schema;
mod validation;

#[doc(inline)]
pub use assistant::{
    assistant_keyed_launch_id, assistant_launch_id, sha256_for_bytes, BUNDLE_VERSION,
};
#[doc(inline)]
pub use index::{bundle_index, BundleIndexError, ConceptMeta};
#[doc(inline)]
pub use prompt_contract::{
    read_prompt_contract, PromptContract, PromptContractError, PROMPT_CONTRACT_PATH,
};
#[doc(inline)]
pub use schema::{
    Citation, Concept, ConceptType, Deprecation, Intent, SourceKind, CONCEPT_SCHEMA_VERSION,
};
#[doc(inline)]
pub use validation::{
    validate_bundle, BundleValidationError, BundleValidationIssue, BundleValidationReport,
};

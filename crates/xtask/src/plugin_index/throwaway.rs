//! `index throwaway`: a self-contained plugin index trust for proofs.
//!
//! The trust documents are written by the kit
//! ([`plugin_kit::throwaway::write`]); this module keeps the `xtask`
//! entries-tree input shape, reads the tree through the index-host reader in
//! [`super::build`], and enforces that the output lies outside the checkout
//! and outside a release tree before delegating the signing to the kit.

// Rust guideline compliant 2026-11-06

use std::path::PathBuf;

use super::build::entries_of;
use crate::test_runtime::check_output_location;
use crate::XtaskError;

/// The inputs of `index throwaway`.
pub(crate) struct ThrowawayArgs {
    pub(crate) entries: PathBuf,
    pub(crate) output: PathBuf,
    pub(crate) schema_version: u64,
}

/// What `index throwaway` reports.
pub(crate) struct Written {
    pub(crate) entries: usize,
    pub(crate) endorser: package::KeyId,
    pub(crate) signer: package::KeyId,
}

/// Writes the anchor, the channel and the signed index into `args.output`.
///
/// The output must be an existing empty directory outside the checkout and
/// outside a release tree. The tree is read with the index-host reader, then
/// the documents are signed by the kit.
pub(crate) fn write(args: &ThrowawayArgs) -> Result<Written, XtaskError> {
    check_output_location(&crate::repo_root(), &args.output)?;
    let entries = entries_of(&args.entries, args.schema_version)?;
    let written = plugin_kit::throwaway::write(&plugin_kit::throwaway::ThrowawayArgs {
        entries,
        output: args.output.clone(),
        schema_version: args.schema_version,
    })
    .map_err(XtaskError::from)?;
    Ok(Written {
        entries: written.entries,
        endorser: written.endorser,
        signer: written.signer,
    })
}

//! `plugin prove`: a model-free install of one archive through the index path.
//!
//! The proof engine lives in the kit crate
//! ([`plugin_kit::prove_archive`]); this module keeps the `xtask` argument
//! shape and the evidence contract, and converts the kit error into the
//! `xtask` one so the CLI, flags, exit codes and evidence JSON stay
//! unchanged.

// Rust guideline compliant 2026-11-06

use std::path::PathBuf;

/// The inputs of `plugin prove`.
pub(crate) struct ProveArgs {
    pub(crate) archive: PathBuf,
    pub(crate) entry: PathBuf,
    pub(crate) release_dir: PathBuf,
    pub(crate) evidence: PathBuf,
    /// Install the package disabled (it stays selected), so `inspect`
    /// must refuse it for being disabled.
    pub(crate) no_enable: bool,
    /// Install this extra archive first through a digest trust, so the
    /// proof runs alongside another installed version: any other version of
    /// the same package leaves the proved version unselected. The archive
    /// must name the entry's package id; a foreign archive is refused.
    pub(crate) pre_install: Option<PathBuf>,
}

/// A failed proof, as the kit reports it.
pub(crate) type Verdict = plugin_kit::Verdict;

/// Proves the archive and writes the evidence through the kit proof engine.
pub(crate) fn prove(args: &ProveArgs) -> Result<String, crate::XtaskError> {
    plugin_kit::prove_archive(&plugin_kit::ProveArgs {
        archive: args.archive.clone(),
        entry: args.entry.clone(),
        release_dir: args.release_dir.clone(),
        evidence: args.evidence.clone(),
        no_enable: args.no_enable,
        pre_install: args.pre_install.clone(),
    })
    .map_err(crate::XtaskError::from)
}

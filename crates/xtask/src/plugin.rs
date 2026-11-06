//! Plugin release tooling: `plugin build` and `plugin prove`.
//!
//! `build` turns a package directory into the canonical archive a plugin
//! release attests; `prove` installs an archive through the index path on an
//! isolated daemon and reports a verdict as evidence. Neither command reads a
//! key file or contacts the network, and `prove` never launches the runtime
//! program of the package.

// Rust guideline compliant 2026-11-06

mod build;
mod host;
mod prove;

pub(crate) use build::{build, BuildArgs};
pub(crate) use prove::{prove, ProveArgs, Verdict};

//! Owner-side plugin kit: lint, build, verify, detect-check, smoke and prove
//! for plugin repositories.
//!
//! The kit owns the commands `xtask plugin …` wraps: the `xtask` commands
//! stay as thin wrappers over this crate so existing docs stay true. Every
//! bound is a named `const`; every wait is bounded (`smoke` never waits on a
//! real production deadline).

pub mod archive;
pub mod build;
#[cfg(feature = "daemon")]
pub mod detect_check;
pub mod entry;
pub mod error;
pub mod files;
pub mod isolated;
#[cfg(feature = "daemon")]
pub mod lint;
pub mod prove;
pub mod smoke;
pub mod throwaway;
pub mod verify;

pub use build::{build, BuildArgs, Built, ARCHIVE_SUFFIX};
pub use entry::{entry_of, read_entry_file};
pub use error::{Error, FAILURE_EXIT_CODE, INDEX_REFUSED_EXIT_CODE, PROVE_FAILED_EXIT_CODE};
pub use prove::{prove_archive, prove_package, ProveArgs, ProveReport, Verdict};
pub use smoke::{smoke, SmokeArgs, SmokeReport};

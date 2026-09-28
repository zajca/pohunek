//! Re-exports shared process observation contracts.

// Rust guideline compliant 2026-09-28

#[doc(inline)]
pub use pohunek_platform::process::{
    Error, ExitWatch, HostInspector, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity,
    ProcessInspector, StartIdentity,
};

#[cfg(test)]
pub(crate) mod readable_host;

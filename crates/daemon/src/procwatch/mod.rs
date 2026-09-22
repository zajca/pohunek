//! Re-exports shared process observation contracts.

// Rust guideline compliant 2026-09-22

#[doc(inline)]
pub use pohunek_platform::process::{
    Error, ExitWatch, HostInspector, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity,
    ProcessInspector, StartIdentity,
};

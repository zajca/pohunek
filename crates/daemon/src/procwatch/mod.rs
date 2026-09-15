//! Re-exports shared process observation contracts.

// Rust guideline compliant 2026-09-13

#[doc(inline)]
pub use pohunek_platform::process::{
    Error, ExitWatch, LinuxInspector, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity,
    ProcessInspector, StartIdentity,
};

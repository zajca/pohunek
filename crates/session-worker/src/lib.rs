//! Owns one durable PTY runtime independently from `pohunekd`.
//!
//! The worker keeps the PTY master, child identity, output history, terminal
//! model, input deduplication state, and durable outcome journal alive while the
//! control-plane daemon is absent. Public clients never connect to this crate
//! directly; [`Server`] exposes the private owner-only Unix protocol used by
//! `pohunekd`.

// Two helpers in `pty` opt back in with a localized `#[expect(unsafe_code)]`
// and document the invariant they rely on: `portable-pty` exposes the PTY
// master only as a raw descriptor, and Darwin's `TIOCOUTQ` query is reachable
// only through rustix's unsafe typed `ioctl`. The rest of the crate stays free
// of unsafe code.
#![deny(unsafe_code)]

// Rust guideline compliant 2026-09-24

mod config;
mod error;
mod identity;
mod input;
mod journal;
mod launch;
mod lease;
mod notification;
mod output;
mod pty;
mod server;
mod service;
#[cfg(test)]
mod test_support;

#[doc(inline)]
pub use config::{ConfigError, WorkerConfig};
#[doc(inline)]
pub use error::WorkerError;
#[doc(inline)]
pub use identity::RejectReason;
#[doc(inline)]
pub use input::{InputError, InputFragment, InputPlan, WriteCoordinator};
#[doc(inline)]
pub use journal::{
    ActiveIdentity, ChildIdentity, Journal, JournalError, JournalRecord, LaunchIdentity,
    LoadedJournal, NativeReferenceClaim, OutdatedJournal, PendingLaunchClaim, ReleasedIdentity,
    RuntimeOutcome, RuntimePhase, WorkerOrigin, JOURNAL_SCHEMA_VERSION,
};
#[doc(inline)]
pub use lease::{ControllerLease, LeaseError, LeaseOwner};
#[doc(inline)]
pub use output::{
    ObservationPage, OutputChunk, OutputEvent, OutputHub, OutputSnapshot, OutputSubscriber,
    RingError, TerminalChunk,
};
#[doc(inline)]
pub use pohunek_terminal::TerminalSnapshot;
#[doc(inline)]
pub use pty::{Command, EnvBase, Exit, ProcessIdentity, PtyError, PtyOwner};
#[doc(inline)]
pub use server::{run, Server, ServerArgs};
#[doc(inline)]
pub use service::{load_service_config, ServiceConfigError};

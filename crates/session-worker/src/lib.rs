//! Owns one durable PTY runtime independently from `pohunekd`.
//!
//! The worker keeps the PTY master, child identity, output history, terminal
//! model, input deduplication state, and durable outcome journal alive while the
//! control-plane daemon is absent. Public clients never connect to this crate
//! directly; [`Server`] exposes the private owner-only Unix protocol used by
//! `pohunekd`.

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-07-23

mod config;
mod error;
mod input;
mod journal;
mod lease;
mod output;
mod pty;
mod server;

#[doc(inline)]
pub use config::{ConfigError, WorkerConfig};
#[doc(inline)]
pub use error::WorkerError;
#[doc(inline)]
pub use input::{InputError, InputFragment, InputPlan, WriteCoordinator};
#[doc(inline)]
pub use journal::{
    ActiveIdentity, ChildIdentity, Journal, JournalError, JournalRecord, LaunchIdentity,
    RuntimeOutcome, RuntimePhase,
};
#[doc(inline)]
pub use lease::{ControllerLease, LeaseError, LeaseOwner};
#[doc(inline)]
pub use output::{
    OutputChunk, OutputEvent, OutputHub, OutputSnapshot, OutputSubscriber, RingError,
};
#[doc(inline)]
pub use pohunek_terminal::TerminalSnapshot;
#[doc(inline)]
pub use pty::{Command, Exit, ProcessIdentity, PtyError, PtyOwner};
#[doc(inline)]
pub use server::{run, Server, ServerArgs};

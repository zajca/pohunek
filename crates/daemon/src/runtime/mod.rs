//! Durable session-worker discovery and supervision.
//!
//! Production workers are sibling systemd user units. This module owns only
//! unit control and daemon-to-worker connections; it never owns a PTY or agent
//! child process.

// Rust guideline compliant 2026-07-23

mod client;
mod launcher;
mod systemd;

pub(crate) use client::WriteReservation;
pub use client::{DataStream, DimensionUpdate, Worker, WorkerError};
#[cfg(test)]
pub use launcher::InProcessWorkerLauncher;
pub use launcher::{
    SubprocessWorkerEnvironment, SubprocessWorkerLauncher, SystemdWorkerLauncher,
    WorkerLaunchError, WorkerLaunchFuture, WorkerLaunchMode, WorkerLauncher,
};
pub use systemd::{UnitInfo, UnitTemplate, Units, UnitsError, DEFAULT_WORKER_UNIT_TEMPLATE};

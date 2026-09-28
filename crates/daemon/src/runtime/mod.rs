//! Durable session-worker supervision and connections.
//!
//! Production workers are native jobs of the target's service manager
//! (transient systemd user units on Linux, launchd jobs on macOS), one job per
//! worker generation. This module owns job lifecycles and daemon-to-worker
//! connections; it never owns a PTY or agent child process.

// Rust guideline compliant 2026-09-24

mod client;
pub(crate) mod environment;
mod launcher;
pub mod lifecycle;

pub(crate) use client::WriteReservation;
pub use client::{DataStream, DimensionUpdate, Worker, WorkerError};
#[cfg(test)]
pub use launcher::InProcessWorkerLauncher;
pub use launcher::{
    SubprocessWorkerLauncher, WorkerLaunchError, WorkerLaunchFuture, WorkerLauncher,
};
pub use lifecycle::{SubprocessWorkerEnvironment, SupervisionConfig};

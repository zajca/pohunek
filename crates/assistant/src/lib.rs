//! Assistant launch orchestration and the host connection types it needs.
//!
//! The crate prepares and starts a Pohunek assistant session against a local or
//! remote daemon using only ordinary protocol requests: it selects an agent
//! runtime, materializes the knowledge bundle and a redacted live snapshot,
//! composes the opening prompt and calls `session.new`. [`launch`] holds the
//! orchestration, [`HostConfig`] and [`ConnectionOptions`] describe how to reach
//! a daemon, and [`AssistantError`] is the single error type of that path.
//!
//! # Examples
//!
//! ```
//! use pohunek_assistant::HostConfig;
//!
//! let host = HostConfig::local("local", "/run/user/1000/pohunek/daemon.sock");
//! assert_eq!(host.id.as_str(), "local");
//! assert!(host.attach_host().is_empty());
//! ```

// Rust guideline compliant 2026-10-03
#![forbid(unsafe_code)]

mod error;
mod host;
pub mod launch;

#[doc(inline)]
pub use error::AssistantError;
#[doc(inline)]
pub use host::{
    connect_client, runtime_is_assistant_capable, runtime_is_launchable, ConnectionOptions,
    HostConfig, HostId, HostTransport,
};

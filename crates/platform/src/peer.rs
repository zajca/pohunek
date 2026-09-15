//! Defines kernel-derived Unix peer credentials.

// Rust guideline compliant 2026-09-13

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::credentials;

/// Kernel-derived identity of one connected Unix peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credentials {
    /// Effective user identifier reported by the kernel.
    pub uid: u32,
    /// Effective group identifier reported by the kernel.
    pub gid: u32,
    /// Connecting process identifier reported by the kernel.
    pub pid: u32,
}

/// Peer-credential lookup failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The kernel rejected credential lookup.
    #[error("failed to read Unix peer credentials: {0}")]
    Socket(std::io::Error),
    /// The kernel returned a process identifier outside the shared range.
    #[error("Unix peer PID is outside the supported range")]
    InvalidPid,
}

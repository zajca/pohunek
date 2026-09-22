//! Defines narrow operating-system contracts for Pohunek.
//!
//! This crate owns process facts and kernel peer identity shared by the daemon
//! and session worker. It deliberately excludes session policy, provider
//! interpretation, public wire types, GUI state, and supervisor policy.

// The Darwin process and peer backends wrap `libproc`, `sysctl`, kqueue, and
// the `LOCAL_PEERCRED`/`LOCAL_PEERPID` socket options, which have no safe
// equivalent; each opts back in with a localized `#[expect(unsafe_code)]` and
// documents the invariant every block relies on. Every other target stays free
// of unsafe code.
#![cfg_attr(not(target_os = "macos"), forbid(unsafe_code))]
#![cfg_attr(target_os = "macos", deny(unsafe_code))]

// Rust guideline compliant 2026-09-22

#[cfg(unix)]
pub mod filesystem;
pub mod peer;
pub mod process;
pub mod supervisor;

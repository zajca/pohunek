//! Shared protocol framing limits.
//!
//! Control traffic is newline-delimited JSON, so every peer must agree on the
//! maximum accepted line length to avoid asymmetric framing failures.

/// Maximum accepted control line length, in bytes.
///
/// The 1 MiB cap bounds per-connection buffering for malformed or malicious
/// unterminated JSON lines while leaving ample room for legitimate control
/// envelopes. Raw PTY bytes use the separate attach transport and are not
/// constrained by this line-framing limit.
pub const MAX_CONTROL_LINE_BYTES: usize = 1024 * 1024;

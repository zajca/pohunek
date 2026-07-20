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

/// Maximum size of the JSON-escaped `diff` string in a `session.diff` result,
/// in bytes.
///
/// Chosen as half of [`MAX_CONTROL_LINE_BYTES`] so the full response envelope
/// (the escaped diff plus the surrounding envelope fields and JSON escaping
/// overhead) is guaranteed to fit in one control line even in the worst case.
/// The daemon truncates the diff at a file boundary and sets `truncated: true`
/// on the `session.diff` result when the underlying diff exceeds this cap;
/// raising this value without also raising `MAX_CONTROL_LINE_BYTES` risks the
/// daemon producing a response it cannot itself transmit.
pub const MAX_SESSION_DIFF_BYTES: usize = 512 * 1024;

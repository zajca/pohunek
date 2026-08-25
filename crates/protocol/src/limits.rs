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

/// Maximum UTF-8 bytes accepted as one programmatic session-input payload.
///
/// One quarter of the control-line ceiling leaves room for the request envelope
/// and worst-case JSON escaping without letting any client bypass the CLI's
/// bounded-stdin contract.
pub const MAX_SESSION_INPUT_BYTES: usize = MAX_CONTROL_LINE_BYTES / 4;

/// Maximum UTF-8 bytes in one request correlation identifier.
///
/// Envelope constructors additionally restrict IDs to unescaped ASCII wire
/// characters, making their serialized contribution exactly their byte length.
pub const MAX_REQUEST_ID_BYTES: usize = 128;

/// Maximum UTF-8 bytes in a logical session identifier carried by observation results.
///
/// Current daemon-generated ULIDs are substantially shorter. The larger public
/// ceiling leaves room for deterministic fixture and imported identifiers while
/// keeping output-response metadata bounded.
pub const MAX_SESSION_ID_BYTES: usize = 128;

/// Maximum UTF-8 bytes in a PTY runtime identifier.
///
/// Runtime identifiers are opaque but participate in every cursor coordinate,
/// so their wire contribution must remain bounded independently of provider.
pub const MAX_RUNTIME_ID_BYTES: usize = 128;

/// Maximum bounded `session.wait` duration accepted on the public wire.
///
/// Eight seconds releases abandoned daemon waiter slots promptly while still
/// amortizing automation round trips.
pub const MAX_SESSION_WAIT_MS: u32 = 8_000;

/// Maximum lifetime of a provider identity claim, measured from receipt.
///
/// Hooks normally issue 30-second claims. A 60-second hard ceiling tolerates
/// scheduling delay while preventing a captured future-dated claim from
/// pinning provider identity indefinitely.
pub const MAX_IDENTITY_CLAIM_TTL_SECS: u64 = 60;

/// Largest fixed success-envelope overhead for any public protocol version.
///
/// The literal uses the longest possible `u32` protocol version and an empty
/// JSON result. A validated request ID adds at most [`MAX_REQUEST_ID_BYTES`]
/// bytes without JSON escaping.
pub const MAX_SUCCESS_RESPONSE_ENVELOPE_BYTES: usize =
    r#"{"v":4294967295,"id":"","ok":}"#.len() + MAX_REQUEST_ID_BYTES;

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

/// Bytes reserved for the public response envelope around observation data.
///
/// Observation responses include a request id, negotiated protocol version,
/// runtime metadata, JSON punctuation, and an error-safe margin. Keeping this
/// reserve explicit means a fully populated response can never exceed
/// [`MAX_CONTROL_LINE_BYTES`] merely because the envelope grew.
pub const OBSERVATION_RESPONSE_ENVELOPE_HEADROOM_BYTES: usize = MAX_SUCCESS_RESPONSE_ENVELOPE_BYTES;

/// Serialized-result bytes reserved for output metadata around `data_base64`.
///
/// Runtime identities, all decimal offsets, booleans, gap metadata, and JSON
/// punctuation fit inside this reserve under their protocol field bounds.
pub const SESSION_OUTPUT_METADATA_HEADROOM_BYTES: usize = 4 * 1024;

/// Maximum raw PTY bytes carried by one `session.output` response.
///
/// Base64 expands every three input bytes to four output bytes. This value is
/// therefore the largest multiple of three whose encoded form fits after the
/// response envelope reserve. Daemon configuration may choose a lower limit.
pub const MAX_SESSION_OUTPUT_BYTES: usize = ((MAX_CONTROL_LINE_BYTES
    - OBSERVATION_RESPONSE_ENVELOPE_HEADROOM_BYTES
    - SESSION_OUTPUT_METADATA_HEADROOM_BYTES)
    / 4)
    * 3;

/// Maximum serialized `session.screen` result bytes.
///
/// Screen text can require JSON escaping, so the worker must measure the
/// serialized result before the daemon adds its envelope. The dedicated reserve
/// keeps the full newline-delimited control response within the framing limit.
pub const MAX_SESSION_SCREEN_RESPONSE_BYTES: usize =
    MAX_CONTROL_LINE_BYTES - OBSERVATION_RESPONSE_ENVELOPE_HEADROOM_BYTES;

/// Maximum line count accepted and returned by `session.read`.
///
/// One thousand rows covers every supported terminal geometry while bounding
/// JSON escaping and daemon-side allocation independently of terminal size.
pub const MAX_SESSION_READ_LINES: u32 = 1_000;

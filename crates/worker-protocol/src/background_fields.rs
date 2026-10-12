// Rust guideline compliant 2026-10-13

//! Field rules of a background-task snapshot.
//!
//! The Claude state hook sanitizes each field before it crosses the hook
//! boundary. The session worker checks every field again before it journals
//! it, and the daemon checks every field again before it publishes it. Each
//! rule is defined once here, so a field the worker keeps is a field the daemon
//! keeps, and the two cannot drift apart.

use crate::control::{
    BACKGROUND_AGENT_TYPE_MAX_BYTES, BACKGROUND_LABEL_MAX_BYTES, BACKGROUND_PROGRAM_MAX_BYTES,
    BACKGROUND_TASK_ID_MAX_BYTES, BACKGROUND_TASK_TYPE_MAX_BYTES,
};
use crate::text::is_hidden_or_separator;

/// Whether `value` is a provider task text: non-empty, within `max_bytes`, and
/// free of control characters.
///
/// A snapshot claim carries its task identifiers, type labels and statuses
/// under this rule. A control character would break the one-line logs and the
/// journal that record them.
#[must_use]
pub fn is_task_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

/// Whether `value` is a task identifier the worker keeps.
///
/// Its bound is [`BACKGROUND_TASK_ID_MAX_BYTES`]; the rule is [`is_task_text`].
#[must_use]
pub fn is_valid_task_id(value: &str) -> bool {
    is_task_text(value, BACKGROUND_TASK_ID_MAX_BYTES)
}

/// Whether `value` is a label the worker keeps.
///
/// It is non-empty, within [`BACKGROUND_LABEL_MAX_BYTES`], and free of
/// controls, separators and format characters, so a client is never shown text
/// that hides or reorders other text.
#[must_use]
pub fn is_valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= BACKGROUND_LABEL_MAX_BYTES
        && !value.chars().any(is_hidden_or_separator)
}

/// Whether `value` is a program name the worker keeps.
///
/// It is within [`BACKGROUND_PROGRAM_MAX_BYTES`], starts with an ASCII
/// alphanumeric byte so it cannot read as an option or a path fragment, and
/// holds only the separators tool names use.
#[must_use]
pub fn is_valid_program(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= BACKGROUND_PROGRAM_MAX_BYTES
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

/// Whether `value` is a provider type label the worker may show to clients.
///
/// It is non-empty, within [`BACKGROUND_TASK_TYPE_MAX_BYTES`], and free of
/// controls, separators and format characters.
#[must_use]
pub fn is_valid_provider_kind(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= BACKGROUND_TASK_TYPE_MAX_BYTES
        && !value.chars().any(is_hidden_or_separator)
}

/// Whether `value` is an agent type the worker keeps.
///
/// It is non-empty, within [`BACKGROUND_AGENT_TYPE_MAX_BYTES`], and free of
/// controls, separators and format characters.
#[must_use]
pub fn is_valid_agent_type(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= BACKGROUND_AGENT_TYPE_MAX_BYTES
        && !value.chars().any(is_hidden_or_separator)
}

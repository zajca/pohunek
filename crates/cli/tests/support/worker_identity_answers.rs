//! Bounded, payload-free listing of the worker's identity-report latency events.

use std::fs;
use std::path::{Path, PathBuf};

use pohunek_logging::config::WORKER_MAX_FILES;
use serde_json::Value;

/// Most events a failure report lists.
const MAX_EVENTS: usize = 32;

/// Message template of the worker's per-report latency event.
const IDENTITY_ANSWERED_MESSAGE: &str = "identity report answered in ";

/// Lists the last worker `worker.identity.answered` events found under the
/// worker log directory, across sessions and rotated files. Only the typed
/// latency and outcome fields are printed, never a free-text field.
pub(crate) fn identity_answered_events(logs: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(logs) else {
        return vec!["worker identity answers: log directory unavailable".to_owned()];
    };
    let mut bases: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("pohunek-session-"))
        })
        .collect();
    bases.sort();
    let mut events = Vec::new();
    for base in &bases {
        for rotated in (1..WORKER_MAX_FILES).rev() {
            let older = PathBuf::from(format!("{}.{rotated}", base.display()));
            if let Ok(log) = fs::read_to_string(older) {
                events.extend(identity_answered_lines(&log));
            }
        }
        if let Ok(log) = fs::read_to_string(base) {
            events.extend(identity_answered_lines(&log));
        }
    }
    let omitted = events.len().saturating_sub(MAX_EVENTS);
    events.drain(..omitted);
    let mut result = vec![format!(
        "worker identity answers: {} shown, {omitted} earlier omitted",
        events.len()
    )];
    result.extend(events);
    result
}

fn identity_answered_lines(log: &str) -> impl Iterator<Item = String> + '_ {
    log.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| {
            let fields = &event["fields"];
            fields["message"]
                .as_str()?
                .starts_with(IDENTITY_ANSWERED_MESSAGE)
                .then(|| {
                    format!(
                        "worker event: identity answered accepted={:?} delivered={:?} lock_wait_us={:?} persist_us={:?} answer_us={:?}",
                        fields["identity.accepted"].as_bool(),
                        fields["identity.delivered"].as_bool(),
                        fields["identity.lock_wait_us"].as_u64(),
                        fields["identity.persist_us"].as_u64(),
                        fields["identity.answer_us"].as_u64(),
                    )
                })
        })
}

//! Bounded, payload-free diagnostics for a missing Codex native reference.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pohunek_logging::config::WORKER_MAX_FILES;
use serde_json::Value;

/// Keep enough worker history to identify the admission boundary without
/// printing user-controlled log fields or the native reference itself.
pub(crate) const MAX_EVENTS: usize = 32;

/// Watches one readiness wait and prints its last observation on panic.
pub(crate) struct NativeReferenceDiagnostics {
    worker_state: PathBuf,
    worker_logs: PathBuf,
    daemon_log: Option<PathBuf>,
    session_id: String,
    last_inspect: Mutex<Option<String>>,
}

impl NativeReferenceDiagnostics {
    pub(crate) fn new(
        worker_state: PathBuf,
        worker_logs: PathBuf,
        daemon_log: Option<PathBuf>,
        session_id: &str,
    ) -> Self {
        Self {
            worker_state,
            worker_logs,
            daemon_log,
            session_id: session_id.to_owned(),
            last_inspect: Mutex::new(None),
        }
    }

    pub(crate) fn observe(&self, record: &Value) {
        let inspect = format!(
            "activity={:?} runtime_state={:?} active_agent_present={} active_agent_session_id_present={} native_session_id_present={} native_session_path_present={}",
            record["activity"].as_str(),
            record["runtime"]["state"].as_str(),
            record["active_agent"].is_string(),
            record["active_agent_session_id"].is_string(),
            record["native_session_id"].is_string(),
            record["native_session_path"].is_string(),
        );
        *self.last_inspect.lock().expect("inspection lock") = Some(inspect);
    }

    pub(crate) fn describe(&self) -> String {
        let mut lines = Vec::new();
        let last = self.last_inspect.try_lock().ok();
        lines.push(format!(
            "last inspect: {:?}",
            last.as_deref().and_then(Option::as_deref)
        ));
        let journals = self.worker_state.join(&self.session_id);
        match fs::read_dir(&journals) {
            Ok(entries) => {
                let mut files: Vec<_> = entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                    .collect();
                files.sort();
                lines.push(format!("worker journals: {}", files.len()));
                for path in files.iter().take(MAX_EVENTS) {
                    match fs::read(path)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    {
                        Some(record) => {
                            lines.push(format!(
                                "journal: phase={:?} hook_schema_present={} child_present={} active_identity_present={} active_identity_reference_present={} pending_launch_claims={} launch_identity_present={} native_reference_claim_present={}",
                                record["phase"].as_str(),
                                record["hook_schema"].is_string(),
                                record["child"].is_object(),
                                record["active_identity"].is_object(),
                                record["active_identity"]["native_reference"].is_string(),
                                record["pending_launch_claims"].as_array().map_or(0, Vec::len),
                                record["launch_identity"].is_object(),
                                record["native_reference_claim"].is_object(),
                            ));
                            lines.push(format!(
                                "process table entries: worker={:?} child={:?} active_identity={:?} launch_identity={:?}",
                                process_entry_present(record["worker_pid"].as_u64()),
                                process_entry_present(record["child"]["pid"].as_u64()),
                                process_entry_present(record["active_identity"]["process"]["pid"].as_u64()),
                                process_entry_present(record["launch_identity"]["process"]["pid"].as_u64()),
                            ));
                        }
                        None => lines.push("journal: unreadable or invalid JSON".to_owned()),
                    }
                }
            }
            Err(error) => lines.push(format!("worker journals: unavailable ({:?})", error.kind())),
        }

        let log = self
            .worker_logs
            .join(format!("pohunek-session-{}.jsonl", self.session_id));
        lines.extend(worker_events(&log));
        if let Some(path) = &self.daemon_log {
            lines.extend(daemon_events(path));
        }
        lines.join("\n")
    }
}

#[cfg(target_os = "linux")]
fn process_entry_present(pid: Option<u64>) -> Option<bool> {
    // hermetic-allowed: #636 the fixture's journaled worker process is the subject of this diagnostic.
    pid.map(|pid| Path::new("/proc").join(pid.to_string()).exists())
}

#[cfg(not(target_os = "linux"))]
fn process_entry_present(_pid: Option<u64>) -> Option<bool> {
    None
}

impl Drop for NativeReferenceDiagnostics {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("native reference diagnostics:\n{}", self.describe());
        }
    }
}

fn worker_events(path: &Path) -> Vec<String> {
    let mut events = Vec::new();
    let mut files = 0;
    for rotated in (1..WORKER_MAX_FILES).rev() {
        let older = PathBuf::from(format!("{}.{rotated}", path.display()));
        if let Ok(log) = fs::read_to_string(older) {
            files += 1;
            events.extend(filtered_events(&log));
        }
    }
    if let Ok(log) = fs::read_to_string(path) {
        files += 1;
        events.extend(filtered_events(&log));
    }
    if files == 0 {
        return vec!["worker events: log unavailable".to_owned()];
    }
    let omitted = events.len().saturating_sub(MAX_EVENTS);
    events.drain(..omitted);
    let mut result = vec![format!(
        "worker admission events: {} shown, {omitted} earlier omitted",
        events.len()
    )];
    result.extend(events);
    result
}

fn filtered_events(log: &str) -> impl Iterator<Item = String> + '_ {
    log.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| {
            let fields = &event["fields"];
            if let (Some(operation), Some(reason)) = (
                safe_code(&fields["identity.operation"]),
                safe_code(&fields["identity.reason"]),
            ) {
                return Some(format!(
                    "worker event: identity rejected operation={operation} reason={reason}"
                ));
            }
            let message = fields["message"].as_str()?;
            match message {
                message if message.starts_with("launch identity verification deferred:") => {
                    Some("worker event: launch identity verification deferred".to_owned())
                }
                "deferred launch identity verification updated" => Some(format!(
                    "worker event: deferred launch identity updated accepted={:?}",
                    fields["launch.accepted"].as_bool(),
                )),
                _ => None,
            }
        })
}

fn daemon_events(path: &Path) -> Vec<String> {
    let Ok(log) = fs::read_to_string(path) else {
        return vec!["daemon identity events: log unavailable".to_owned()];
    };
    let events: Vec<_> = log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| {
            let fields = &event["fields"];
            match fields["message"].as_str()? {
                "rejected worker identity process claim" => Some(format!(
                    "daemon event: process claim rejected reason={:?}",
                    safe_code(&fields["reason"]),
                )),
                "rejected worker identity snapshot" => Some(format!(
                    "daemon event: identity snapshot rejected reason={:?}",
                    safe_code(&fields["reason"]),
                )),
                "worker metadata remains retryable" => Some(format!(
                    "daemon event: worker metadata retryable attempts={:?}",
                    fields["attempts"].as_u64(),
                )),
                _ => None,
            }
        })
        .collect();
    let omitted = events.len().saturating_sub(MAX_EVENTS);
    let mut result = vec![format!(
        "daemon identity events: {} shown, {omitted} earlier omitted",
        events.len() - omitted
    )];
    result.extend(events.into_iter().skip(omitted));
    result
}

fn safe_code(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| {
        text.len() <= 64
            && text
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    })
}

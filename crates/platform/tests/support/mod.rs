//! Helpers shared by the real supervisor backend tests.

// Rust guideline compliant 2026-09-24

use std::sync::{Arc, Mutex};

/// JSON field that carries the name of a rejected discovery entry.
const REJECTED_ENTRY_FIELD: &str = r#""supervisor.entry":""#;

/// Collects the JSON log lines emitted on the current thread.
#[derive(Clone, Default)]
pub(crate) struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("capture buffer")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogCapture {
    /// Captures this thread's events until the guard drops.
    ///
    /// Only valid on a current-thread runtime, where every `.await` of the
    /// test resumes on the thread that installed it.
    pub(crate) fn install(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .json()
                .with_writer(move || writer.clone())
                .finish(),
        )
    }

    /// Names of the rejected entries a backend warned about, in order.
    pub(crate) fn rejected_entries(&self) -> Vec<String> {
        let bytes = self.0.lock().expect("capture buffer").clone();
        String::from_utf8(bytes)
            .expect("UTF-8 log output")
            .lines()
            .filter(|line| line.contains(r#""level":"WARN""#))
            .filter_map(|line| {
                let start = line.find(REJECTED_ENTRY_FIELD)? + REJECTED_ENTRY_FIELD.len();
                let end = line[start..].find('"')? + start;
                Some(line[start..end].to_owned())
            })
            .collect()
    }
}

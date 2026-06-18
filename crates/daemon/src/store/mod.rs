//! Minimal resume-binding store (milestone 7 precursor to the SQLite store).
//!
//! TODO(milestone 9): the full SQLite `state.db` + `user_version` migrations +
//! append-only `events/` log (see `docs/plan-phase-1.md` "SQLite Schema" and
//! Build-Order step 9) absorb this file. The `session` table's resume-binding
//! columns (`native_session_id`, `native_session_path`, `cwd`, `pty_cols`,
//! `pty_rows`, `agent`) are exactly the fields persisted here, so this is a
//! direct precursor, not a parallel design.
//!
//! Scope here is deliberately tiny: hold only what is needed to relaunch and
//! resume a session after a daemon restart. No secrets are ever written (a
//! native session id and a cwd are not secrets). Storage is newline-delimited
//! JSON (one binding per line) under the daemon data dir, rewritten atomically
//! via a temp file + rename so a crash mid-write cannot corrupt it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use protocol::AgentKind;
use serde::{Deserialize, Serialize};

/// One session's resume binding: everything needed to relaunch-and-resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeBinding {
    /// The zagentmesh session id (stable across restart).
    pub session_id: String,
    /// Agent kind backing the session.
    pub agent: AgentKind,
    /// Working directory to relaunch in.
    pub cwd: PathBuf,
    /// Terminal width at capture time.
    pub cols: u16,
    /// Terminal height at capture time.
    pub rows: u16,
    /// Captured native session id used to build the resume argv.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Captured native session path, for agents that resume from a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_path: Option<String>,
}

/// File-backed resume-binding store.
///
/// Writes are serialized by an internal lock; the file is small (one line per
/// resumable session) so a full rewrite per record is cheap.
#[derive(Debug)]
pub struct ResumeStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl ResumeStore {
    /// Open a store at `path`. The file is created lazily on first `record`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
        }
    }

    /// The backing file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load all bindings. A missing file yields an empty list; malformed lines
    /// are skipped (a corrupt line must not block resuming the rest).
    pub fn load(&self) -> io::Result<Vec<ResumeBinding>> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        Ok(content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<ResumeBinding>(line).ok())
            .collect())
    }

    /// Upsert a binding (keyed by `session_id`) and rewrite the file atomically.
    pub fn record(&self, binding: &ResumeBinding) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut bindings = self.load()?;
        if let Some(existing) = bindings
            .iter_mut()
            .find(|existing| existing.session_id == binding.session_id)
        {
            *existing = binding.clone();
        } else {
            bindings.push(binding.clone());
        }
        self.write_all(&bindings)
    }

    /// Remove a binding by session id and rewrite the file. A missing entry is a
    /// no-op.
    pub fn remove(&self, session_id: &str) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut bindings = self.load()?;
        let before = bindings.len();
        bindings.retain(|binding| binding.session_id != session_id);
        if bindings.len() == before {
            return Ok(());
        }
        self.write_all(&bindings)
    }

    /// Serialize all bindings to a temp file and rename it over the store path.
    fn write_all(&self, bindings: &[ResumeBinding]) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut body = String::new();
        for binding in bindings {
            // Our own type serializes infallibly; map any error to io for the
            // caller rather than panicking.
            let line = serde_json::to_string(binding)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            body.push_str(&line);
            body.push('\n');
        }

        let tmp = self.temp_path();
        write_owner_private(&tmp, body.as_bytes())?;
        fs::rename(&tmp, &self.path)
    }

    fn temp_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_else(|| "resume-bindings.jsonl".into());
        name.push(format!(".tmp.{}", std::process::id()));
        match self.path.parent() {
            Some(parent) => parent.join(name),
            None => PathBuf::from(name),
        }
    }
}

/// Write a file with owner-only permissions (`0600`).
fn write_owner_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use protocol::AgentKind;

    use super::{ResumeBinding, ResumeStore};

    fn temp_store_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "zagentmesh-store-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("resume-bindings.jsonl")
    }

    fn binding(session_id: &str, native: &str) -> ResumeBinding {
        ResumeBinding {
            session_id: session_id.to_owned(),
            agent: AgentKind::Claude,
            cwd: PathBuf::from("/workspace/project"),
            cols: 120,
            rows: 40,
            native_session_id: Some(native.to_owned()),
            native_session_path: None,
        }
    }

    #[test]
    fn load_missing_file_is_empty() {
        let store = ResumeStore::new(temp_store_path("missing"));
        assert!(store.load().expect("load").is_empty());
    }

    #[test]
    fn record_then_load_round_trips() {
        let store = ResumeStore::new(temp_store_path("roundtrip"));
        let b = binding("s-1", "native-1");
        store.record(&b).expect("record");

        let loaded = store.load().expect("load");
        assert_eq!(loaded, vec![b]);
    }

    #[test]
    fn record_upserts_by_session_id() {
        let store = ResumeStore::new(temp_store_path("upsert"));
        store.record(&binding("s-1", "native-1")).expect("record 1");
        store.record(&binding("s-2", "native-2")).expect("record 2");
        // Re-record s-1 with a new native id: must replace, not duplicate.
        store
            .record(&binding("s-1", "native-1-updated"))
            .expect("record 1 again");

        let loaded = store.load().expect("load");
        assert_eq!(loaded.len(), 2, "no duplicate session id: {loaded:?}");
        let s1 = loaded
            .iter()
            .find(|b| b.session_id == "s-1")
            .expect("s-1 present");
        assert_eq!(s1.native_session_id.as_deref(), Some("native-1-updated"));
    }

    #[test]
    fn remove_deletes_binding() {
        let store = ResumeStore::new(temp_store_path("remove"));
        store.record(&binding("s-1", "native-1")).expect("record");
        store.remove("s-1").expect("remove");
        assert!(store.load().expect("load").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn store_file_is_owner_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_store_path("perms");
        let store = ResumeStore::new(path.clone());
        store.record(&binding("s-1", "native-1")).expect("record");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "store file must be owner-private");
    }
}

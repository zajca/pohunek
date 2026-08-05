//! Explicit one-time migration preflight for daemon-owned PTYs.

// Rust guideline compliant 2026-07-28

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use protocol::{method, ErrorClass, ProtocolError, SessionInfo, SessionState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::Client;
use crate::error::CliError;
use crate::paths::Paths;

const MANIFEST_SCHEMA_VERSION: u32 = 1;
const OWNER_PRIVATE_FILE_MODE: u32 = 0o600;

/// Sanitized migration authority consumed by the first worker-aware daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MigrationManifest {
    pub(crate) schema_version: u32,
    pub(crate) created_at: String,
    pub(crate) store_sha256: String,
    pub(crate) accept_runtime_loss: bool,
    pub(crate) sessions: Vec<SessionInfo>,
    pub(crate) live_session_ids: Vec<String>,
    pub(crate) recoverable_session_ids: Vec<String>,
    pub(crate) unrecoverable_session_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionClassification {
    live: Vec<String>,
    recoverable: Vec<String>,
    unrecoverable: Vec<String>,
}

/// Queries the legacy daemon, atomically writes the migration manifest, and
/// refuses replacement while live PTYs exist unless loss was accepted.
pub(crate) async fn run_preflight(
    host: &str,
    paths: &Paths,
    accept_runtime_loss: bool,
    json: bool,
) -> Result<(), CliError> {
    if host != crate::target::LOCAL_HOST {
        return Err(CliError::Protocol(ProtocolError::new(
            ErrorClass::Configuration,
            "migration_local_only",
            "durable-worker migration preflight must run on the daemon host",
            Some("run `pohunek migration preflight` locally on that host".to_owned()),
        )));
    }
    let mut client = Client::connect(host, paths).await?;
    let sessions = client
        .call::<method::SessionList>(protocol::SessionListParams::default())
        .await?;
    let classification = classify_sessions(&sessions);
    let store_path = paths.data_dir.join("metadata.jsonl");
    let store_sha256 = fingerprint(&store_path)?;
    let manifest = MigrationManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        created_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| {
                CliError::Io(std::io::Error::other(format!(
                    "failed to format migration timestamp: {error}"
                )))
            })?,
        store_sha256,
        accept_runtime_loss,
        sessions,
        live_session_ids: classification.live,
        recoverable_session_ids: classification.recoverable,
        unrecoverable_session_ids: classification.unrecoverable,
    };
    write_manifest(&paths.worker_migration_manifest(), &manifest)?;

    if json {
        print!("{}", crate::commands::render_json(&manifest)?);
    } else {
        print!("{}", render_human(&manifest, paths));
    }
    if !manifest.live_session_ids.is_empty() && !accept_runtime_loss {
        return Err(CliError::Protocol(ProtocolError::new(
            ErrorClass::Configuration,
            "migration_live_sessions",
            format!(
                "legacy daemon still owns live sessions: {}",
                manifest.live_session_ids.join(", ")
            ),
            Some(
                "let them finish or rerun preflight with --accept-runtime-loss after reviewing the affected IDs"
                    .to_owned(),
            ),
        )));
    }
    Ok(())
}

fn classify_sessions(sessions: &[SessionInfo]) -> SessionClassification {
    let live = sessions.iter().filter(|session| is_live_legacy(session));
    let mut classification = SessionClassification {
        live: Vec::new(),
        recoverable: Vec::new(),
        unrecoverable: Vec::new(),
    };
    for session in live {
        classification.live.push(session.id.0.clone());
        if session.native_session_id.is_some() || session.native_session_path.is_some() {
            classification.recoverable.push(session.id.0.clone());
        } else {
            classification.unrecoverable.push(session.id.0.clone());
        }
    }
    classification
}

fn is_live_legacy(session: &SessionInfo) -> bool {
    session.external != Some(true)
        && session.runtime.is_none()
        && matches!(
            session.state,
            SessionState::Starting | SessionState::Running
        )
}

fn fingerprint(path: &Path) -> Result<String, CliError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(CliError::Io(error)),
    };
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn write_manifest(path: &Path, manifest: &MigrationManifest) -> Result<(), CliError> {
    let parent = path.parent().ok_or_else(|| {
        CliError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "migration manifest has no parent directory",
        ))
    })?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(manifest)?;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(OWNER_PRIVATE_FILE_MODE)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn render_human(manifest: &MigrationManifest, paths: &Paths) -> String {
    let mut output = format!(
        "migration manifest: {}\nlegacy live sessions: {}\n",
        paths.worker_migration_manifest().display(),
        manifest.live_session_ids.len()
    );
    if !manifest.live_session_ids.is_empty() {
        writeln!(
            &mut output,
            "affected legacy sessions: {}",
            manifest.live_session_ids.join(", ")
        )
        .expect("writing to String is infallible");
        writeln!(
            &mut output,
            "recoverable: {}\nunrecoverable: {}",
            manifest.recoverable_session_ids.len(),
            manifest.unrecoverable_session_ids.len()
        )
        .expect("writing to String is infallible");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use protocol::{AgentKind, CwdSource, RuntimeState, SessionId, SessionRuntime, StateSource};

    use super::*;

    #[test]
    fn empty_store_fingerprint_is_stable() {
        let path = std::env::temp_dir().join(format!(
            "pohunek-missing-migration-store-{}",
            std::process::id()
        ));
        assert_eq!(
            fingerprint(&path).expect("fingerprint"),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn classification_separates_recoverable_live_and_terminal_sessions() {
        let mut recoverable = session("recoverable", SessionState::Running);
        recoverable.native_session_id = Some("native-1".to_owned());
        let unrecoverable = session("unrecoverable", SessionState::Starting);
        let terminal = session("terminal", SessionState::Done);

        assert_eq!(
            classify_sessions(&[recoverable, unrecoverable, terminal]),
            SessionClassification {
                live: vec!["recoverable".to_owned(), "unrecoverable".to_owned()],
                recoverable: vec!["recoverable".to_owned()],
                unrecoverable: vec!["unrecoverable".to_owned()],
            }
        );
    }

    #[test]
    fn classification_ignores_worker_backed_and_external_live_sessions() {
        let mut worker = session("worker", SessionState::Running);
        worker.runtime = Some(SessionRuntime {
            state: RuntimeState::Live,
            worker_id: Some("worker-1".to_owned()),
            runtime_id: Some("runtime-1".to_owned()),
            started_at: None,
            last_connected_at: None,
            loss_reason: None,
        });
        worker.native_session_id = Some("native-worker".to_owned());
        let mut external = session("external", SessionState::Running);
        external.external = Some(true);
        let legacy = session("legacy", SessionState::Starting);

        assert_eq!(
            classify_sessions(&[worker, external, legacy]),
            SessionClassification {
                live: vec!["legacy".to_owned()],
                recoverable: Vec::new(),
                unrecoverable: vec!["legacy".to_owned()],
            }
        );
    }

    #[test]
    fn manifest_write_is_owner_private_and_replaces_atomically() {
        let root = std::env::temp_dir().join(format!(
            "pohunek-migration-manifest-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let path = root.join("migration.json");
        let manifest = MigrationManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            created_at: "2026-07-23T00:00:00Z".to_owned(),
            store_sha256: "00".repeat(32),
            accept_runtime_loss: true,
            sessions: vec![session("s-1", SessionState::Running)],
            live_session_ids: vec!["s-1".to_owned()],
            recoverable_session_ids: Vec::new(),
            unrecoverable_session_ids: vec!["s-1".to_owned()],
        };

        write_manifest(&path, &manifest).expect("write manifest");
        let permissions = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(permissions, OWNER_PRIVATE_FILE_MODE);
        assert_eq!(
            serde_json::from_slice::<MigrationManifest>(&fs::read(&path).expect("read manifest"))
                .expect("parse manifest"),
            manifest
        );
        assert!(
            fs::read_dir(&root)
                .expect("read manifest directory")
                .all(|entry| entry.expect("entry").path() == path),
            "atomic temporary file must not remain"
        );
        fs::remove_dir_all(root).expect("remove fixture");
    }

    fn session(id: &str, state: SessionState) -> SessionInfo {
        SessionInfo {
            name: None,
            id: SessionId(id.to_owned()),
            external: Some(false),
            agent: "codex".to_owned(),
            agent_base: AgentKind::Codex,
            cwd: PathBuf::from("/repo"),
            cwd_source: Some(CwdSource::Launch),
            pid: 42,
            cols: 80,
            rows: 24,
            state,
            state_source: StateSource::Process,
            activity: None,
            native_session_id: None,
            native_session_path: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            project_id: None,
            project_label: None,
            metadata: BTreeMap::new(),
            is_linked_worktree: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            created_at: "2026-07-23T00:00:00Z".to_owned(),
            updated_at: "2026-07-23T00:00:00Z".to_owned(),
            exit_code: None,
            runtime: None,
        }
    }
}

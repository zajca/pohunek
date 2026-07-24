//! Daemon-side assistant bundle materialization.

use std::fs;

use knowledge::{
    assistant_launch_id, bundle_content_hash, bundle_index, materialize as materialize_bundle,
    materialized_version_hash, BUNDLE_VERSION,
};
use protocol::{AssistantMaterializeResult, ProtocolError};

use crate::Paths;

const SNAPSHOT_FILE: &str = "snapshot.json";

/// Materialize the daemon's embedded assistant bundle and persist the launch snapshot.
pub fn materialize_assistant(
    paths: &Paths,
    snapshot: &str,
) -> Result<AssistantMaterializeResult, ProtocolError> {
    let version_hash = materialized_version_hash();
    let concepts: Vec<protocol::ConceptMeta> = bundle_index()
        .map_err(|err| {
            ProtocolError::materialization_failed("assistant bundle index", &err.to_string())
        })?
        .into_iter()
        .map(Into::into)
        .collect();
    let bundle_path =
        materialize_bundle(paths.cache_dir.clone(), &version_hash).map_err(|err| {
            ProtocolError::materialization_failed(
                &paths.assistant_bundle_cache_dir().display().to_string(),
                &err.to_string(),
            )
        })?;
    let launch_id = assistant_launch_id(&version_hash);
    let runtime_dir = paths.assistant_runtime_dir(&launch_id).ok_or_else(|| {
        ProtocolError::materialization_failed("assistant runtime", "invalid launch id")
    })?;
    fs::create_dir_all(&runtime_dir).map_err(|err| {
        ProtocolError::materialization_failed(&runtime_dir.display().to_string(), &err.to_string())
    })?;
    let snapshot_path = runtime_dir.join(SNAPSHOT_FILE);
    fs::write(&snapshot_path, snapshot).map_err(|err| {
        ProtocolError::materialization_failed(
            &snapshot_path.display().to_string(),
            &err.to_string(),
        )
    })?;

    Ok(AssistantMaterializeResult {
        bundle_path: bundle_path.display().to_string(),
        snapshot_path: snapshot_path.display().to_string(),
        version: BUNDLE_VERSION.to_owned(),
        content_hash: bundle_content_hash().to_owned(),
        concepts,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn paths_at(root: &Path) -> Paths {
        Paths {
            runtime_dir: root.join("runtime"),
            socket: root.join("runtime").join("daemon.sock"),
            lock: root.join("runtime").join("daemon.lock"),
            log_dir: root.join("logs"),
            state_dir: root.join("state"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            config_home: root.join("config"),
            config_dir: root.join("config").join("pohunek"),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pohunek-assistant-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after unix epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn materializes_bundle_and_writes_snapshot() {
        let root = temp_dir("materialize");
        let paths = paths_at(&root);

        let result = materialize_assistant(&paths, r#"{"ok":true}"#).expect("materialize");

        assert!(Path::new(&result.bundle_path).join("index.md").is_file());
        assert_eq!(
            fs::read_to_string(&result.snapshot_path).expect("snapshot"),
            r#"{"ok":true}"#
        );
        assert_eq!(result.version, BUNDLE_VERSION);
        assert_eq!(result.content_hash, bundle_content_hash());
        assert!(!result.concepts.is_empty());
    }

    #[test]
    fn materialize_writes_each_snapshot_to_unique_launch_dir() {
        let root = temp_dir("materialize-unique-snapshots");
        let paths = paths_at(&root);

        let first = materialize_assistant(&paths, r#"{"launch":1}"#).expect("first materialize");
        let second = materialize_assistant(&paths, r#"{"launch":2}"#).expect("second materialize");

        assert_eq!(first.bundle_path, second.bundle_path);
        assert_ne!(first.snapshot_path, second.snapshot_path);
        assert_eq!(
            fs::read_to_string(&first.snapshot_path).expect("first snapshot"),
            r#"{"launch":1}"#
        );
        assert_eq!(
            fs::read_to_string(&second.snapshot_path).expect("second snapshot"),
            r#"{"launch":2}"#
        );
    }
}

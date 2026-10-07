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

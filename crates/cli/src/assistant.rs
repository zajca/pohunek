//! CLI-side assistant materialization helpers.

use std::fs;

use knowledge::{
    assistant_launch_id, bundle_content_hash, bundle_index, materialize as materialize_bundle,
    materialized_version_hash, BUNDLE_VERSION,
};
use protocol::{
    method, AssistantMaterializeParams, AssistantMaterializeResult, ProtocolError, Request,
};

use crate::client::Client;
use crate::error::CliError;
use crate::paths::Paths;

const SNAPSHOT_FILE: &str = "snapshot.json";

/// Result of a degraded (snapshot-only) materialization.
///
/// Like [`AssistantMaterializeResult`] but without a bundle directory or bundle
/// content. There is no `bundle_path` — the bundle was intentionally not
/// materialized.
pub(crate) struct DegradedMaterializeResult {
    /// Absolute path to the per-launch `snapshot.json`.
    pub(crate) snapshot_path: String,
    /// The bundle version embedded in this binary (for prompt annotation only;
    /// the bundle files themselves are absent).
    pub(crate) version: String,
}

/// Materialize only the snapshot for a degraded launch (no bundle extraction).
///
/// Writes the snapshot JSON to a unique per-launch runtime directory and returns
/// the path together with the binary's embedded bundle version (for annotation).
/// The bundle directory is not created or returned — the caller must not rely on
/// it for file I/O.
///
/// # Errors
///
/// Returns [`CliError`] when the runtime directory cannot be created or the
/// snapshot cannot be written.
pub(crate) fn materialize_degraded(
    paths: &Paths,
    snapshot: &str,
) -> Result<DegradedMaterializeResult, CliError> {
    let version_hash = materialized_version_hash();
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

    Ok(DegradedMaterializeResult {
        snapshot_path: snapshot_path.display().to_string(),
        version: BUNDLE_VERSION.to_owned(),
    })
}

pub(crate) fn materialize_local(
    paths: &Paths,
    snapshot: &str,
) -> Result<AssistantMaterializeResult, CliError> {
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

pub(crate) async fn materialize_remote(
    client: &mut Client,
    snapshot: &str,
) -> Result<AssistantMaterializeResult, CliError> {
    let params = AssistantMaterializeParams {
        snapshot: snapshot.to_owned(),
    };
    let request = Request::new(
        crate::commands::request_id(method::ASSISTANT_MATERIALIZE),
        method::ASSISTANT_MATERIALIZE,
        serde_json::to_value(params)?,
    );
    let value = client
        .request(&request)
        .await
        .map_err(|err| map_assistant_method_error(err, method::ASSISTANT_MATERIALIZE))?;
    let result: AssistantMaterializeResult = serde_json::from_value(value)?;
    assert_bundle_matches(&result)?;
    Ok(result)
}

fn assert_bundle_matches(result: &AssistantMaterializeResult) -> Result<(), CliError> {
    let expected_hash = bundle_content_hash();
    if result.version == BUNDLE_VERSION && result.content_hash == expected_hash {
        Ok(())
    } else {
        Err(ProtocolError::assistant_bundle_mismatch(
            BUNDLE_VERSION,
            expected_hash,
            &result.version,
            &result.content_hash,
        )
        .into())
    }
}

fn map_assistant_method_error(err: CliError, method: &str) -> CliError {
    match err {
        CliError::Protocol(source) if source.code == "method_not_found" => {
            CliError::Protocol(ProtocolError::assistant_method_unsupported(method))
        }
        CliError::Client(pohunek_client::ClientError::RemoteProtocol { host, source })
            if source.code == "method_not_found" =>
        {
            CliError::Client(pohunek_client::ClientError::RemoteProtocol {
                host,
                source: ProtocolError::assistant_method_unsupported(method),
            })
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn paths_at(root: &Path) -> Paths {
        Paths {
            runtime_dir: root.join("runtime"),
            socket: root.join("runtime").join("daemon.sock"),
            data_dir: root.join("data"),
            log_dir: root.join("logs"),
            cache_dir: root.join("cache"),
            config_home: root.join("config"),
            config_dir: root.join("config").join("pohunek"),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pohunek-cli-assistant-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after unix epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn local_materialization_writes_unique_snapshot_paths() {
        let root = temp_dir("local-materialize");
        let paths = paths_at(&root);

        let first = materialize_local(&paths, "first").expect("first materialize");
        let second = materialize_local(&paths, "second").expect("second materialize");

        assert_eq!(first.bundle_path, second.bundle_path);
        assert_ne!(first.snapshot_path, second.snapshot_path);
        assert_eq!(
            std::fs::read_to_string(&first.snapshot_path).expect("first snapshot"),
            "first"
        );
        assert_eq!(
            std::fs::read_to_string(&second.snapshot_path).expect("second snapshot"),
            "second"
        );
    }

    #[test]
    fn assistant_method_not_found_maps_to_assistant_method_unsupported() {
        let err = CliError::Protocol(ProtocolError::method_not_found("assistant.materialize"));

        let mapped = map_assistant_method_error(err, method::ASSISTANT_MATERIALIZE);

        let CliError::Protocol(source) = mapped else {
            panic!("expected protocol error");
        };
        assert_eq!(source.code, "assistant_method_unsupported");
    }

    #[test]
    fn remote_assistant_method_not_found_preserves_host_context() {
        let err = CliError::Client(pohunek_client::ClientError::RemoteProtocol {
            host: "build-box".to_owned(),
            source: ProtocolError::method_not_found("assistant.materialize"),
        });

        let mapped = map_assistant_method_error(err, method::ASSISTANT_MATERIALIZE);

        let CliError::Client(pohunek_client::ClientError::RemoteProtocol { host, source }) = mapped
        else {
            panic!("expected remote protocol error");
        };
        assert_eq!(host, "build-box");
        assert_eq!(source.code, "assistant_method_unsupported");
    }
}

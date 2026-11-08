//! Daemon-side assistant launch: resolve the bundle of the assistant context
//! package and persist the launch snapshot.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use knowledge::{
    assistant_keyed_launch_id, assistant_launch_id, bundle_index, read_prompt_contract,
    BUNDLE_VERSION,
};
use protocol::{
    AssistantMaterializeParams, AssistantMaterializeResult, PackageId, ProtocolError,
    ASSISTANT_CONTEXT_PACKAGE_ID,
};

use crate::agent::host::ContextError;
use crate::packages::PackageService;
use crate::Paths;

const SNAPSHOT_FILE: &str = "snapshot.json";
/// Mode of every directory and file the launch creates below the runtime root.
const PRIVATE_DIR_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Resolve the assistant context package, index its bundle and persist the
/// launch snapshot below the runtime directory.
///
/// # Errors
///
/// `assistant_package_missing` when the package is not installed, selected and
/// enabled or its files cannot be used, and `materialization_failed` when the
/// bundle cannot be indexed or the snapshot cannot be written.
pub async fn materialize_assistant(
    paths: &Paths,
    packages: &PackageService,
    params: AssistantMaterializeParams,
) -> Result<AssistantMaterializeResult, ProtocolError> {
    let id = PackageId::parse(ASSISTANT_CONTEXT_PACKAGE_ID).map_err(|error| {
        ProtocolError::materialization_failed("assistant package id", &error.to_string())
    })?;
    let context = packages.context_dir(id).await.map_err(|error| {
        let detail = match &error {
            ContextError::NotInstalled => "it is not installed and selected".to_owned(),
            ContextError::Disabled => "it is disabled".to_owned(),
            ContextError::Rejected(rejection) => rejection.to_string(),
        };
        ProtocolError::assistant_package_missing(ASSISTANT_CONTEXT_PACKAGE_ID, &detail)
    })?;
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || {
        let concepts: Vec<protocol::ConceptMeta> = bundle_index(&context.dir)
            .map_err(|err| {
                ProtocolError::materialization_failed("assistant bundle index", &err.to_string())
            })?
            .into_iter()
            .map(Into::into)
            .collect();
        let prompt_contract = read_prompt_contract(&context.dir).map_err(|err| {
            ProtocolError::assistant_package_missing(
                ASSISTANT_CONTEXT_PACKAGE_ID,
                &format!("its prompt contract is unusable: {err}"),
            )
        })?;
        let launch_id = match params.request_id.as_deref() {
            Some(request_id) => assistant_keyed_launch_id(BUNDLE_VERSION, request_id),
            None => assistant_launch_id(BUNDLE_VERSION),
        };
        let runtime_dir = paths.assistant_runtime_dir(&launch_id).ok_or_else(|| {
            ProtocolError::materialization_failed("assistant runtime", "invalid launch id")
        })?;
        create_private_dir_all(&runtime_dir).map_err(|err| {
            ProtocolError::materialization_failed(
                &runtime_dir.display().to_string(),
                &err.to_string(),
            )
        })?;
        let snapshot_path = runtime_dir.join(SNAPSHOT_FILE);
        write_snapshot(
            &snapshot_path,
            &params.snapshot,
            params.request_id.is_some(),
        )
        .map_err(|err| {
            ProtocolError::materialization_failed(
                &snapshot_path.display().to_string(),
                &err.to_string(),
            )
        })?;
        Ok(AssistantMaterializeResult {
            bundle_path: context.dir.display().to_string(),
            snapshot_path: snapshot_path.display().to_string(),
            version: context.identity.version.to_string(),
            content_hash: context.digest.to_string(),
            concepts,
            prompt_contract,
        })
    })
    .await
    .map_err(|_join_error| {
        ProtocolError::materialization_failed("assistant materialization", "the task panicked")
    })?
}

/// Writes the launch snapshot to `path`.
///
/// A launch under a request id owns its directory: the first launch writes the
/// snapshot and a repeat keeps it, so a repeat that the daemon refuses cannot
/// change the file a running session reads.
fn write_snapshot(path: &Path, snapshot: &str, keep_existing: bool) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if keep_existing {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(PRIVATE_FILE_MODE);
    }
    match options.open(path) {
        Ok(mut file) => file.write_all(snapshot.as_bytes()),
        Err(error) if keep_existing && error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIR_MODE)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

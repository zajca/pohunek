//! The retention sweep selectors over the socket: `spawned_by` and
//! `session_ids` narrow the examined set, reported through a dry run.
//!
//! A dry run never removes anything, so these scenarios pin `examined` and
//! `removed` instead of the survivors: the selector shapes the candidate set
//! before the TTL rules ever apply.

// Rust guideline compliant 2026-11-13

use super::*;
use crate::session_tree::{create_tree, tree_cwd, TREE_IDLE_SHELL};

fn selector_sweep(
    spawned_by: Option<SessionId>,
    session_ids: Option<Vec<SessionId>>,
) -> ProtocolRequest {
    Request::make(
        "retention-selector-sweep",
        method::SESSION_RETENTION_SWEEP,
        serde_json::to_value(protocol::SessionRetentionParams {
            dry_run: true,
            limit: None,
            spawned_by,
            session_ids,
        })
        .expect("serialize sweep params"),
    )
}

async fn dry_sweep(
    control: &mut Framed<UnixStream, LinesCodec>,
    spawned_by: Option<SessionId>,
    session_ids: Option<Vec<SessionId>>,
) -> protocol::SessionRetentionResult {
    let response = exchange(control, &selector_sweep(spawned_by, session_ids)).await;
    serde_json::from_value(ok_payload(response)).expect("sweep result")
}

#[tokio::test]
async fn sweep_with_spawned_by_examines_only_the_parent_children() {
    let socket = temp_socket("retention-selector-children");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", TREE_IDLE_SHELL]),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = tree_cwd(&socket);

    let (parent, first, second, _, _, unrelated) = create_tree(&mut control, cwd).await;

    let result = dry_sweep(&mut control, Some(parent.id.clone()), None).await;
    assert_eq!(result.examined, 2, "only the two children: {result:?}");
    assert_eq!(result.removed, 0, "a dry run removes nothing: {result:?}");
    drop(first);
    drop(second);
    drop(unrelated);

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn sweep_with_session_ids_examines_only_the_named_sessions() {
    let socket = temp_socket("retention-selector-ids");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", TREE_IDLE_SHELL]),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = tree_cwd(&socket);

    let (_, first, _, _, _, unrelated) = create_tree(&mut control, cwd).await;

    let result = dry_sweep(
        &mut control,
        None,
        Some(vec![first.id.clone(), unrelated.id.clone()]),
    )
    .await;
    assert_eq!(
        result.examined, 2,
        "only the two named sessions: {result:?}"
    );
    assert_eq!(result.removed, 0, "a dry run removes nothing: {result:?}");

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
}

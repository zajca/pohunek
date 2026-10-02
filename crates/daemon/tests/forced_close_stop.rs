//! A stop whose PTY output is force-closed still ends the session with its
//! root's exit.
//!
//! A descendant outside the root's process group (a `setsid` child) keeps the
//! PTY slave open after the root dies, so the worker has to force the output
//! closed. The root's exit is known and reaped, so the session must reach its
//! terminal state with that exit, and the forced close must stay visible.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pohunek_daemon::procwatch::HostInspector;
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
use pohunek_test_support::wait::{guard, poll_until};
use pohunek_test_support::worker_binary;
use protocol::{RuntimeState, SessionNewParams, SessionState, SessionWarningKind};

/// Stop grace short enough that both grace windows pass quickly; the stop is
/// bounded by the hang guard, never by this value.
const STOP_GRACE: Duration = Duration::from_millis(50);

/// Seconds the root sleeps if the stop never kills it.
const ROOT_SLEEP_SECONDS: u32 = 30;

/// Releases the escaped descendant when dropped, also while a failure unwinds.
///
/// The descendant loops while the barrier directory exists.
struct ReleaseHolder(PathBuf);

impl Drop for ReleaseHolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn params(agent: &str, cwd: &Path) -> SessionNewParams {
    SessionNewParams {
        name: None,
        agent: agent.to_owned(),
        cwd: Some(cwd.to_path_buf()),
        cols: 80,
        rows: 24,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
        metadata: std::collections::BTreeMap::new(),
    }
}

/// Writes the agent script: it starts the escaped holder, waits until the
/// holder reports ready, and then becomes a process that dies on `SIGTERM`.
fn write_agent(root: &Path, barrier: &Path, ready: &Path) -> PathBuf {
    let script = root.join("escape.sh");
    std::fs::write(
        &script,
        format!(
            "trap '' HUP\n\
             setsid sh -c 'trap \"\" HUP TERM; printf ready > \"$2\"; while [ -d \"$1\" ]; do sleep 0.01; done' escaped \"{barrier}\" \"{ready}\" &\n\
             while [ ! -e \"{ready}\" ]; do sleep 0.01; done\n\
             exec sleep {ROOT_SLEEP_SECONDS}\n",
            barrier = barrier.display(),
            ready = ready.display(),
        ),
    )
    .expect("write the agent script");
    let agents = root.join("agents");
    std::fs::create_dir(&agents).expect("agents directory");
    std::fs::write(
        agents.join("escape.toml"),
        format!(
            "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"{}\"]\n",
            script.display()
        ),
    )
    .expect("write the agent profile");
    agents
}

#[tokio::test]
async fn a_forced_output_close_ends_the_session_with_the_root_exit() {
    let root = pohunek_test_support::tempdir().expect("fixture root");
    let barrier = root.path().join("barrier");
    std::fs::create_dir(&barrier).expect("holder barrier");
    let _release = ReleaseHolder(barrier.clone());
    let ready = root.path().join("ready");
    let agents = write_agent(root.path(), &barrier, &ready);
    // Short home: the worker socket nests several directories below it.
    let worker_home = pohunek_test_support::tempdir_with_prefix("fc-").expect("worker home");
    let home = worker_home.path().to_path_buf();
    let environment = SubprocessWorkerEnvironment {
        runtime_home: home.join("runtime"),
        state_home: home.join("state"),
        data_home: home.join("data"),
        config_home: home.join("config"),
        cache_home: home.join("cache"),
        home: home.clone(),
        daemon_socket: home.join("daemon.sock"),
    };
    let mut config = SessionRegistryConfig {
        agents_dir: Some(agents),
        stop_grace: STOP_GRACE,
        store_path: Some(root.path().join("metadata.jsonl")),
        ..SessionRegistryConfig::default()
    };
    config.socket_path = Some(environment.daemon_socket.clone());
    config.worker_runtime_root = Some(environment.runtime_home.join("pohunek/workers"));
    config.worker_state_root = Some(environment.state_home.join("pohunek/workers"));
    config.supervision = Some(environment.supervision(worker_binary()));
    let registry = SessionRegistry::new_with_launcher_and_inspector(
        config,
        Arc::new(SubprocessWorkerLauncher::new()),
        Arc::new(HostInspector::new()),
    );

    let created = registry
        .create(params("escape", root.path()))
        .await
        .expect("create the session");
    poll_until("the escaped descendant to hold the PTY", || {
        ready.exists().then_some(())
    });

    let stopped = guard(
        "the stop of a session with a forced output close",
        Box::pin(registry.stop(&created.id)),
    )
    .await
    .expect("a forced output close must not fail the stop");
    assert!(stopped.stopped);

    let info = registry
        .inspect(&created.id)
        .await
        .expect("inspect the stopped session");
    assert_eq!(info.state, SessionState::Stopped);
    let runtime = info.runtime.as_ref().expect("runtime");
    assert_eq!(runtime.state, RuntimeState::Terminal);
    assert!(
        info.warnings
            .iter()
            .any(|warning| warning.kind == SessionWarningKind::OutputForceClosed),
        "the forced close stays visible: {:?}",
        info.warnings
    );
    // The root was killed by the stop, so the exit is a signal death, not a
    // success the stop could have invented.
    assert_ne!(info.exit_code, Some(0));

    let journal = worker_journal(&home.join("state/pohunek/workers"));
    assert_eq!(journal["phase"], "terminal");
    assert_eq!(journal["outcome"]["output_forced_closed"], true);
    assert_eq!(journal["outcome"]["reason"], "explicit_stop");

    assert!(
        !registry
            .stop(&created.id)
            .await
            .expect("a repeated stop of a terminal session")
            .stopped
    );
}

/// Reads the single worker journal below `state_root`.
fn worker_journal(state_root: &Path) -> serde_json::Value {
    let session = std::fs::read_dir(state_root)
        .expect("worker state root")
        .next()
        .expect("one session directory")
        .expect("session directory entry")
        .path();
    let journal = std::fs::read_dir(session)
        .expect("session journal directory")
        .map(|entry| entry.expect("journal entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .expect("a worker journal");
    serde_json::from_slice(&std::fs::read(journal).expect("read the journal"))
        .expect("the journal is JSON")
}

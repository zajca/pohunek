//! Recovery of a hook runtime whose conversation is a `<reference>/session.jsonl`
//! below a date-sharded tree under `XDG_DATA_HOME`.

// Rust guideline compliant 2026-10-27

use super::*;

/// A fixture agent that parks on `gate` and exits when released on its first
/// launch, and stays alive on a relaunch that names a conversation.
fn parked_agent(dir: &std::path::Path, marker: &std::path::Path) -> (PathBuf, fs::File) {
    let script = dir.join("data-root-agent");
    let gate_path = dir.join("exit.gate");
    let gate = hook_gate(&gate_path);
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\n{record}case \" $* \" in *\" --session \"*) sleep 30 ;; *) read _ < '{gate}'; exit 0 ;; esac\n",
            record = append_launch_record(marker, "printf 'launch\\n'; printf '%s\\n' \"$@\""),
            gate = gate_path.display(),
        ),
    );
    (script, gate)
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_needs_the_session_directory_below_the_data_root() {
    let dir = temp_dir("data-root-recovery");
    let home = temp_dir("data-root-recovery-home");
    let data = temp_dir("data-root-recovery-data");
    let marker = dir.join("argv.txt");
    let (script, mut gate) = parked_agent(&dir, &marker);
    let registry = SessionRegistry::new_with_runtimes_and_environment(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            ..SessionRegistryConfig::default()
        },
        crate::agent::host::fixture::data_root_hooked_host(&script),
        assigned_environment(&home, &[("XDG_DATA_HOME", &data)]),
        pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
    );
    let created = registry
        .create(SessionNewParams {
            agent: "pi".to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create the session");
    let reference = "9d0a9a0e-52c8-4f0e-9a43-6c1b8a1c2d03".to_owned();
    assert!(
        registry
            .report_as_root(
                native_report_params(
                    &registry,
                    created.id.clone(),
                    "pi".to_owned(),
                    reference.clone(),
                    None,
                )
                .await,
            )
            .await
            .recorded
    );
    gate.write_all(b"go\n").expect("release the exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("the session exits");

    let launches = || {
        fs::read_to_string(&marker)
            .expect("marker")
            .matches("launch\n")
            .count()
    };
    let refused = |what: &str| {
        let registry = &registry;
        let id = created.id.clone();
        let what = what.to_owned();
        async move {
            let error = registry
                .resume(&id)
                .await
                .expect_err(&format!("{what} is no conversation"));
            assert_eq!(error.code, "agent_native_reference_missing", "{what}");
        }
    };

    // Nothing below the data root, then the dated directory without the
    // conversation's.
    refused("an empty data root").await;
    let tree = data.join("fixture/sessions/2026/10/08");
    fs::create_dir_all(&tree).expect("dated directory");
    refused("a dated tree without the session").await;

    // The session directory exists but not its log, or the log is not a regular
    // file, or the directory sits at another depth.
    let session = tree.join(&reference);
    fs::create_dir_all(&session).expect("session directory");
    refused("a session directory without its log").await;
    fs::create_dir(session.join("session.jsonl")).expect("a directory named like the log");
    refused("a directory named session.jsonl").await;
    fs::remove_dir(session.join("session.jsonl")).expect("remove it");
    let elsewhere = dir.join("elsewhere.jsonl");
    fs::write(&elsewhere, "{}\n").expect("the log elsewhere");
    std::os::unix::fs::symlink(&elsewhere, session.join("session.jsonl")).expect("symlink the log");
    refused("a symlinked log").await;
    fs::remove_file(session.join("session.jsonl")).expect("remove it");
    let shallow = data.join("fixture/sessions/2026/10").join(&reference);
    fs::create_dir_all(&shallow).expect("session directory at the wrong depth");
    fs::write(shallow.join("session.jsonl"), "{}\n").expect("log at the wrong depth");
    refused("a session directory at the wrong depth").await;
    assert_eq!(
        launches(),
        1,
        "no agent is launched into a missing conversation"
    );

    // The conversation exists: the same recovery goes through.
    fs::write(session.join("session.jsonl"), "{}\n").expect("the conversation log");
    registry.resume(&created.id).await.expect("native recovery");
    wait_for_file_contains(&marker, &format!("--session\n{reference}\n")).await;
    assert_eq!(launches(), 2);

    registry.stop(&created.id).await.expect("stop the session");
}

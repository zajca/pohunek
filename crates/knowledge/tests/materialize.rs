//! Materializing the embedded knowledge bundle into a fresh cache directory.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;

/// Set in the child process that runs under the permissive umask.
const UMASK_CHILD_VAR: &str = "KNOWLEDGE_MATERIALIZE_UMASK_CHILD";

/// Umask of a default login session, under which a plain `mkdir` is 0755.
const PERMISSIVE_UMASK: &str = "022";

#[test]
fn materializing_into_a_fresh_cache_creates_its_directories_owner_private() {
    // The umask is process-wide, so a child owns it and this binary keeps its own.
    let output = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &format!("umask {PERMISSIVE_UMASK}; exec \"$0\" \"$@\""),
        ])
        .arg(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "materialize_under_a_permissive_umask_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(UMASK_CHILD_VAR, "1")
        .output()
        .expect("run the child under the permissive umask");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "child failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn materialize_under_a_permissive_umask_child() {
    if std::env::var_os(UMASK_CHILD_VAR).is_none() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    // `pohunek` stands for `XDG_CACHE_HOME/pohunek`, which does not exist yet.
    let cache = root.path().join("pohunek");

    let bundle =
        knowledge::materialize(&cache, "0123456789abcdef").expect("materialize the bundle");

    assert!(bundle.starts_with(&cache), "{}", bundle.display());
    for directory in [cache.clone(), cache.join("knowledge")] {
        let mode = std::fs::metadata(&directory)
            .expect("the cache directory exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{}", directory.display());
    }
}

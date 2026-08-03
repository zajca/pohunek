//! End-to-end standalone host discovery without a local daemon socket.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "pohunek-standalone-discovery-{}-{nanos}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create temp directory");
    dir
}

fn fake_netbird(bin: &std::path::Path) {
    let path = bin.join("netbird");
    fs::write(&path, "#!/bin/sh\nprintf '%s\\n' '{\"peers\":[]}'\n").expect("write fake netbird");
    let mut permissions = fs::metadata(&path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod fake netbird");
}

#[test]
fn discover_and_list_json_need_cache_and_netbird_but_not_runtime_socket() {
    let root = temp_dir();
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    fake_netbird(&bin);
    let inherited_path = std::env::var("PATH").expect("PATH");
    let path = format!("{}:{inherited_path}", bin.display());

    for command in ["discover", "list"] {
        let output = Command::new(env!("CARGO_BIN_EXE_pohunek"))
            .args(["host", command, "--json"])
            .env("PATH", &path)
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("HOME")
            .output()
            .expect("run CLI");
        assert!(
            output.status.success(),
            "{command} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).expect("utf8").trim(), "[]");
    }
    let _ = fs::remove_dir_all(root);
}

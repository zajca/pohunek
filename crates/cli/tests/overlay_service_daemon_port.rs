//! A service-started daemon binds the remote listener on the port of
//! `overlays.toml`.
//!
//! A supervised daemon starts with the bootstrap environment allowlist and
//! nothing else, so `POHUNEK_REMOTE_PORT` never reaches it. The configuration
//! file is the one port source such a daemon can read: its directory comes
//! from `XDG_CONFIG_HOME`, which the allowlist carries.

#![cfg(target_os = "linux")]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use pohunek_platform::supervisor::BOOTSTRAP_ENV_ALLOWLIST;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use pohunek_test_support::{bin_exe, bind_redirect, worker_binary};

#[path = "support/connect_redirect.rs"]
mod connect_redirect;

/// The address the fake `netbird` reports for this machine.
const SELF_ADDRESS: &str = "100.64.0.7";

/// The address the fake `netbird` reports for the remote peer.
const PEER_ADDRESS: &str = "100.64.0.8";

/// A daemon process, killed and reaped on drop.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_loopback_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind an ephemeral loopback port")
        .local_addr()
        .expect("read the ephemeral port")
        .port()
}

fn write_fake_netbird(bin: &Path) {
    fs::create_dir_all(bin).expect("create bin");
    let status = format!(
        "{{\"netbirdIp\":\"{SELF_ADDRESS}\",\"peers\":{{\"details\":[{{\
         \"fqdn\":\"fixture-remote.netbird.test\",\"netbirdIp\":\"{PEER_ADDRESS}\",\
         \"status\":\"Connected\"}}]}}}}"
    );
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{status}'\n"),
    )
    .expect("write fake netbird");
}

#[test]
fn a_service_started_daemon_binds_the_port_of_the_file() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let bin = env.root().join("bin");
    write_fake_netbird(&bin);
    let port = free_loopback_port();

    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(&file, format!("providers = [\"netbird\"]\nport = {port}\n"))
        .expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");

    let (bind_library, bind_capture) = bind_redirect::build(env.root());
    let (connect_library, connect_capture) = connect_redirect::build(env.root());

    // The environment of the service job: the bootstrap allowlist and the
    // executable search path, no POHUNEK_REMOTE_PORT. The shim variables and
    // the subprocess worker launcher only let the daemon run outside a
    // service manager on a host without an overlay address.
    let mut daemon = Command::new(bin_exe("pohunek").with_file_name("pohunekd"));
    daemon.env_clear();
    for key in BOOTSTRAP_ENV_ALLOWLIST {
        if let Some(value) = env.environment().get(std::ffi::OsStr::new(key)) {
            daemon.env(key, value);
        }
    }
    let log = fs::File::create(env.root().join("daemon.out")).expect("create the daemon log");
    let child = daemon
        .env("PATH", &bin)
        .env("LD_PRELOAD", &bind_library)
        .env(bind_redirect::CAPTURE_ENV, &bind_capture)
        .env("SHELL", "/bin/sh")
        .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
        .env("POHUNEK_WORKER_BIN", worker_binary())
        .stdin(Stdio::null())
        .stdout(log.try_clone().expect("duplicate the log handle"))
        .stderr(log)
        .spawn()
        .expect("spawn pohunekd");
    let _daemon = Daemon(child);

    let bound = poll_until("the daemon to attempt its overlay bind", || {
        fs::read_to_string(&bind_capture)
            .ok()
            .filter(|text| !text.is_empty())
    });
    assert!(
        bound
            .lines()
            .any(|line| line == format!("{SELF_ADDRESS}:{port}")),
        "the daemon binds the file's port on the overlay address: {bound:?}"
    );

    // The CLI of the same user dials the peer on the same port.
    poll_until("the redirected listener to accept connections", || {
        std::net::TcpStream::connect(("127.0.0.1", port))
            .ok()
            .map(drop)
    });
    let output = env
        .command(bin_exe("pohunek"))
        .env("PATH", &bin)
        .env("LD_PRELOAD", &connect_library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
        .args(["--host", "fixture-remote", "session", "list", "--json"])
        .output()
        .expect("run the remote CLI");
    let dialed = fs::read_to_string(&connect_capture).unwrap_or_default();
    assert!(
        dialed
            .lines()
            .any(|line| line == format!("{PEER_ADDRESS}:{port}")),
        "the CLI dials the file's port: {dialed:?}"
    );
    assert!(
        output.status.success(),
        "the service daemon answers on the file's port: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

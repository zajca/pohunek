//! Binary-level Hermes and legacy integration lifecycle contracts.

#![cfg(unix)]

// Rust guideline compliant 2026-08-06

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, RuntimeRef, PROTOCOL_VERSION};
use serde_json::{json, Value};

struct Fixture {
    env: TestEnv,
    root: PathBuf,
}

impl Fixture {
    fn new(_tag: &str) -> Self {
        // `TestEnv` keeps the root short and canonical, so the fixture daemon
        // socket stays within the macOS 103-byte `sun_path` limit.
        let env = TestEnv::new().expect("create the hermetic test environment");
        let root = env.root().to_path_buf();
        Self { env, root }
    }

    fn private_directory(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(&path).expect("create private fixture directory");
        let mut current = self.root.clone();
        for component in Path::new(relative).components() {
            current.push(component.as_os_str());
            set_mode(&current, 0o700);
        }
        path
    }

    fn command(&self) -> Command {
        let mut command = self.env.command(pohunek_test_support::bin_exe("pohunek"));
        command.env("HERMES_HOME", self.root.join("ambient-must-not-be-read"));
        command
    }
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set fixture mode");
}

fn run(fixture: &Fixture, arguments: &[&str]) -> Output {
    fixture
        .command()
        .args(arguments)
        .output()
        .expect("run pohunek binary")
}

fn parse_error(output: &Output) -> Value {
    assert!(
        !output.status.success(),
        "expected command failure, stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
    let envelope: Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON error envelope");
    envelope["err"].clone()
}

#[test]
fn binary_legacy_install_preserves_daemon_rpc_for_each_selector() {
    let fixture = Fixture::new("legacy-rpc");
    for directory in ["home", "state", "run/pohunek", "data", "config", "cache"] {
        fixture.private_directory(directory);
    }
    let socket = fixture.root.join("run/pohunek/daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind fake daemon");
    let server = thread::spawn(move || {
        let mut captured = Vec::new();
        for _ in 0..3 {
            let (stream, _) = listener.accept().expect("accept CLI request");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read CLI request");
            let request: Request = serde_json::from_str(line.trim_end()).expect("parse request");
            let agent = request.params()["agent"].as_str();
            let report_agent = match agent {
                Some("codex") | None => RuntimeRef::codex(),
                Some("claude") => RuntimeRef::claude(),
                other => panic!("unexpected agent: {other:?}"),
            };
            captured.push(request.params().clone());
            let response = Response::ok(
                PROTOCOL_VERSION,
                request.id(),
                json!({
                    "installed": [{
                        "agent": report_agent,
                        "hook_path": "/redacted/fixture-hook",
                        "config_paths": ["/redacted/fixture-config"]
                    }]
                }),
            )
            .expect("build daemon response");
            writeln!(
                reader.get_mut(),
                "{}",
                serde_json::to_string(&response).expect("serialize response")
            )
            .expect("write daemon response");
        }
        captured
    });

    for selector in [None, Some("codex"), Some("claude")] {
        let mut arguments = vec!["integration", "install"];
        if let Some(agent) = selector {
            arguments.extend(["--agent", agent]);
        }
        let output = run(&fixture, &arguments);
        assert!(
            output.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8(output.stdout)
            .expect("legacy output")
            .contains("installed"));
    }
    assert_eq!(
        server.join().expect("fake daemon"),
        [
            json!({}),
            json!({"agent": "codex"}),
            json!({"agent": "claude"})
        ]
    );
}

#[test]
fn binary_parser_preserves_daemon_status_and_hermes_target_contracts() {
    let fixture = Fixture::new("parser");
    let missing_status_target = run(
        &fixture,
        &["integration", "status", "--agent", "hermes", "--json"],
    );
    assert_eq!(missing_status_target.status.code(), Some(1));
    assert_eq!(
        parse_error(&missing_status_target)["code"],
        "integration_hermes_usage"
    );

    let both = run(
        &fixture,
        &[
            "integration",
            "status",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-home",
            "/private/hermes",
            "--json",
        ],
    );
    assert_eq!(both.status.code(), Some(2));
    let envelope: Value = serde_json::from_slice(&both.stdout).expect("conflict JSON");
    assert_eq!(envelope["err"]["code"], "cli_usage");

    for selector in [None, Some("codex"), Some("claude")] {
        let mut arguments = vec!["integration", "status", "--json"];
        if let Some(agent) = selector {
            arguments.extend(["--agent", agent]);
        }
        let output = run(&fixture, &arguments);
        assert_eq!(output.status.code(), Some(1), "{selector:?}");
        assert_ne!(parse_error(&output)["code"], "cli_usage", "{selector:?}");
    }
}

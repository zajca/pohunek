//! `pohunek host command run` against a real daemon.
//!
//! The `pohunek` binary runs as a subprocess against the daemon of the plugin
//! harness (a real `SessionRegistry` over a real plugin root), so every
//! assertion exercises the daemon's own verdict for the call. A fixture package
//! declares commands that run one shell script; the script blocks on FIFOs the
//! test controls, so no scenario waits on the clock. The overlay route uses a
//! real `RemoteServer` behind the fake `netbird` and connect interposer of the
//! host-pin scenarios, and a daemon without the method is a scripted socket.

// Rust guideline compliant 2026-10-28

#![cfg(unix)]

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use package::{build_archive, read_archive, ArchiveEntry, Limits};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::guard;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::Value;

#[cfg(target_os = "linux")]
#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[cfg(target_os = "linux")]
#[path = "support/pin_overlay.rs"]
mod pin_overlay;
#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, stderr_text, stdout_text, Built, Harness};

/// Id of the fixture package that declares the commands.
const PACKAGE: &str = "acme.fleet-state";

/// Id of the fixture package that is linked, so it stays unselected.
const LINKED_PACKAGE: &str = "acme.linked";

/// The program's stdout of the `ok` command.
const OK_OUTPUT: &str = "h\u{e9}llo \u{4e16}\u{754c}\nsecond line\n";

/// Exit status of a usage error.
const USAGE_STATUS: i32 = 2;

/// The fixture program. `$1` is the work directory, `$2` the mode and `$3` the
/// command's tag; the modes are fixed in the descriptor's `args`.
const SCRIPT: &str = r#"#!/bin/sh
dir="$1"; mode="$2"; id="$3"
case "$mode" in
ok)
  : > "$dir/$id.ran"
  printf 'h\303\251llo \344\270\226\347\225\214\nsecond line\n'
  ;;
exit)
  exit 3
  ;;
badutf8)
  printf '\377\376'
  ;;
hold)
  echo up > "$dir/$id.started"
  read x < "$dir/$id.release"
  printf 'released'
  ;;
stall)
  read x < "$dir/$id.release"
  ;;
esac
"#;

/// Commands of the fixture: name, mode, exposure.
const COMMANDS: [(&str, &str, &str); 6] = [
    ("ok", "ok", "peers"),
    ("ok-local", "ok", "local"),
    ("exit-three", "exit", "peers"),
    ("badutf8", "badutf8", "peers"),
    ("hold", "hold", "peers"),
    ("stall", "stall", "peers"),
];

fn descriptor(package: &str, work: &Path) -> String {
    let mut text = format!(
        "schema = 1\nid = \"{package}\"\nversion = \"1.0.0\"\nruntime_api = 1\nrequires = [\"package-commands-v1\"]\n"
    );
    for (name, mode, expose) in COMMANDS {
        let _ = write!(
            text,
            "\n[commands.{name}]\nprogram = \"bin/run.sh\"\nargs = [\"{}\", \"{mode}\", \"{name}\"]\nexpose = \"{expose}\"\n",
            work.display()
        );
    }
    text
}

fn entries(package: &str, work: &Path) -> Vec<ArchiveEntry> {
    vec![
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: descriptor(package, work).into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "bin/run.sh".to_owned(),
            contents: SCRIPT.as_bytes().to_vec(),
            executable: true,
        },
    ]
}

impl Harness {
    /// The directory the fixture reports into and blocks on.
    fn work(&self) -> PathBuf {
        self.env.root().join("work")
    }

    /// Writes the fixture archive of `package`.
    fn command_archive(&self, package: &str) -> Built {
        let work = self.work();
        fs::create_dir_all(&work).expect("create the work directory");
        let bytes =
            build_archive(&entries(package, &work), &Limits::DEFAULT).expect("the archive builds");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the archive reads")
            .digest()
            .clone();
        let path = self.env.root().join(format!("{package}.tar.zst"));
        fs::write(&path, bytes).expect("write the archive");
        Built { path, digest }
    }

    /// Installs the fixture package enabled and selected through the CLI.
    async fn install_commands(&self) -> Built {
        let built = self.command_archive(PACKAGE);
        let (code, document) = self
            .json(&[
                "plugin",
                "install",
                path_str(&built.path),
                "--sha256",
                built.digest.as_str(),
                "--yes",
            ])
            .await;
        assert_eq!(code, 0, "{document}");
        assert_eq!(document["ok"]["status"], "installed", "{document}");
        built
    }

    /// Creates the FIFOs the `hold` and `stall` modes of command `tag` use.
    fn make_fifos(&self, tag: &str) {
        for suffix in ["started", "release"] {
            let path = self.work().join(format!("{tag}.{suffix}"));
            nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600))
                .unwrap_or_else(|error| panic!("mkfifo {}: {error}", path.display()));
        }
    }

    /// Runs `host command run local <package> <command> <extra>`.
    async fn run_command(
        &self,
        package: &str,
        command: &str,
        extra: &[&str],
    ) -> std::process::Output {
        let mut arguments = vec!["host", "command", "run", "local", package, command];
        arguments.extend_from_slice(extra);
        self.run(&arguments).await
    }

    /// [`Self::run_command`] with `--json`: the exit status and the single
    /// document on stdout.
    async fn json_command(&self, package: &str, command: &str, extra: &[&str]) -> (i32, Value) {
        let mut arguments = vec!["host", "command", "run", "local", package, command];
        arguments.extend_from_slice(extra);
        self.json(&arguments).await
    }
}

/// Asserts a refused run: the exit status, an empty stdout and the typed error
/// under `--json`, and the fixed message plus hint on stderr without it.
async fn assert_refused(
    harness: &Harness,
    package: &str,
    command: &str,
    extra: &[&str],
    status: i32,
    code: &str,
) -> Value {
    let (exit, document) = harness.json_command(package, command, extra).await;
    assert_eq!(exit, status, "{code}: {document}");
    assert!(document.get("ok").is_none(), "{document}");
    assert_eq!(document["err"]["code"], code, "{document}");
    assert!(document["err"]["msg"].is_string(), "{document}");
    assert!(document["err"]["recover"].is_string(), "{document}");

    let output = harness.run_command(package, command, extra).await;
    assert_eq!(output.status.code(), Some(status), "{code}");
    assert!(
        output.stdout.is_empty(),
        "{code}: no stdout on an error: {}",
        stdout_text(&output)
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains(document["err"]["msg"].as_str().expect("message")),
        "{code}: {stderr}"
    );
    assert!(stderr.contains("hint: "), "{code}: {stderr}");
    document
}

#[tokio::test]
async fn success_prints_output_verbatim() {
    let harness = Harness::start().await;
    harness.install_commands().await;

    let output = harness.run_command(PACKAGE, "ok", &[]).await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    assert_eq!(output.stdout, OK_OUTPUT.as_bytes());
    assert!(output.stderr.is_empty(), "{}", stderr_text(&output));
    assert!(harness.work().join("ok.ran").exists());
    harness.stop().await;
}

#[tokio::test]
async fn json_success_shape() {
    let harness = Harness::start().await;
    let built = harness.install_commands().await;

    let (code, document) = harness.json_command(PACKAGE, "ok", &[]).await;

    assert_eq!(code, 0, "{document}");
    assert!(document.get("err").is_none(), "{document}");
    let ok = &document["ok"];
    assert_eq!(ok["package"]["id"], PACKAGE, "{document}");
    assert_eq!(ok["package"]["version"], "1.0.0", "{document}");
    assert_eq!(ok["command"], "ok", "{document}");
    assert_eq!(ok["digest"], built.digest.as_str(), "{document}");
    assert_eq!(ok["output"], OK_OUTPUT, "{document}");
    assert!(ok["duration_ms"].is_u64(), "{document}");
    harness.stop().await;
}

#[tokio::test]
async fn each_daemon_verdict_exits_with_its_status() {
    let harness = Harness::start().await;

    // Nothing is installed yet.
    assert_refused(
        &harness,
        PACKAGE,
        "ok",
        &[],
        41,
        "command_package_not_installed",
    )
    .await;

    harness.install_commands().await;
    harness.make_fifos("stall");

    assert_refused(
        &harness,
        PACKAGE,
        "missing",
        &[],
        44,
        "command_not_declared",
    )
    .await;
    assert_refused(
        &harness,
        PACKAGE,
        "badutf8",
        &[],
        50,
        "command_output_not_utf8",
    )
    .await;
    // The program writes more than the five bytes the cap allows.
    assert_refused(
        &harness,
        PACKAGE,
        "ok",
        &["--max-output-bytes", "5"],
        49,
        "command_output_too_large",
    )
    .await;
    // The program blocks on a FIFO nobody writes, so any deadline stops it.
    assert_refused(
        &harness,
        PACKAGE,
        "stall",
        &["--timeout-ms", "1"],
        48,
        "command_timed_out",
    )
    .await;
    harness.stop().await;
}

#[tokio::test]
async fn failed_reports_process_exit() {
    let harness = Harness::start().await;
    harness.install_commands().await;

    let document = assert_refused(&harness, PACKAGE, "exit-three", &[], 47, "command_failed").await;
    assert_eq!(
        document["err"]["details"]["process_exit"]["exit_code"], 3,
        "{document}"
    );

    let output = harness.run_command(PACKAGE, "exit-three", &[]).await;
    assert!(
        stderr_text(&output).contains("the program exited with status 3"),
        "{}",
        stderr_text(&output)
    );
    harness.stop().await;
}

#[tokio::test]
async fn a_disabled_package_exits_43() {
    let harness = Harness::start().await;
    harness.install_commands().await;
    let (code, document) = harness.json(&["plugin", "disable", PACKAGE]).await;
    assert_eq!(code, 0, "{document}");

    assert_refused(&harness, PACKAGE, "ok", &[], 43, "command_package_disabled").await;
    harness.stop().await;
}

#[tokio::test]
async fn a_package_without_a_selected_version_exits_42() {
    let harness = Harness::start().await;
    let work = harness.work();
    let directory = harness.env.root().join("linked-package");
    fs::create_dir_all(directory.join("bin")).expect("create the package directory");
    fs::write(
        directory.join("runtime.toml"),
        descriptor(LINKED_PACKAGE, &work),
    )
    .expect("write the descriptor");
    let script = directory.join("bin/run.sh");
    fs::write(&script, SCRIPT).expect("write the script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("make it executable");
    let (code, document) = harness
        .json(&["plugin", "link", path_str(&directory), "--yes"])
        .await;
    assert_eq!(code, 0, "{document}");

    assert_refused(
        &harness,
        LINKED_PACKAGE,
        "ok",
        &[],
        42,
        "command_not_selected",
    )
    .await;
    harness.stop().await;
}

#[tokio::test]
async fn a_package_that_fails_verification_exits_51() {
    let harness = Harness::start().await;
    let built = harness.install_commands().await;
    let script = package::registry::files_path(&harness.plugins, &built.digest).join("bin/run.sh");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("make it writable");
    fs::write(&script, format!("{SCRIPT}# tampered\n")).expect("modify the script");

    assert_refused(&harness, PACKAGE, "ok", &[], 51, "command_unavailable").await;
    assert!(!harness.work().join("ok.ran").exists(), "nothing started");
    harness.stop().await;
}

#[tokio::test]
async fn a_running_command_makes_the_next_call_exit_46() {
    let harness = Harness::start().await;
    harness.install_commands().await;
    harness.make_fifos("hold");

    let mut first = harness
        .env
        .tokio_command(pohunek_test_support::bin_exe("pohunek"))
        .args(["host", "command", "run", "local", PACKAGE, "hold"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start the first call");
    // The program announces itself once it runs.
    let started = harness.work().join("hold.started");
    guard(
        "the program to start",
        tokio::task::spawn_blocking(move || fs::read(started).expect("read the start FIFO")),
    )
    .await
    .expect("start handshake");

    assert_refused(&harness, PACKAGE, "hold", &[], 46, "command_busy").await;

    let release = harness.work().join("hold.release");
    tokio::task::spawn_blocking(move || fs::write(release, b"go").expect("write the release"))
        .await
        .expect("release");
    let status = guard("the first call to finish", first.wait())
        .await
        .expect("wait for the first call");
    assert_eq!(status.code(), Some(0));
    harness.stop().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_local_only_command_is_not_exposed_over_a_peer() {
    let harness = Harness::start().await;
    harness.install_commands().await;
    let governance = Arc::new(
        pohunek_daemon::governance::HostGovernanceService::open(
            harness.env.root().join("peer-governance"),
        )
        .await
        .expect("open the host-governance service"),
    );
    let state = pohunek_daemon::api::DaemonState::new(
        pohunek_daemon::api::HealthInfo::new("0.0.0-test"),
        harness.sessions.clone(),
        governance,
        plugin_harness::overlay_registry(),
        pohunek_daemon::api::OverlaySource::Default,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the peer listener");
    let port = listener.local_addr().expect("peer address").port();
    let server = pohunek_daemon::api::RemoteServer::from_listener(listener, state);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(server.serve(async move {
        let _ = stopped.await;
    }));

    let root = harness.env.root().to_path_buf();
    let (library, capture) = connect_redirect::build(&root);
    let table = pin_overlay::status(
        None,
        &[pin_overlay::Peer::new(
            "peer-a.netbird.test",
            "100.64.0.11",
            "key-a",
        )],
    );
    let path = pin_overlay::install(&root.join("bin"), &capture, &table, &table);
    let run = |extra: &'static [&'static str]| {
        let mut command = harness
            .env
            .tokio_command(pohunek_test_support::bin_exe("pohunek"));
        command
            .env("PATH", &path)
            .env("LD_PRELOAD", &library)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
            .env("POHUNEK_REMOTE_PORT", port.to_string())
            .args(["host", "command", "run", "peer-a", PACKAGE])
            .args(extra);
        async move {
            guard("the pohunek binary to finish", command.output())
                .await
                .expect("run the pohunek binary")
        }
    };

    let output = run(&["ok-local", "--json"]).await;
    assert_eq!(output.status.code(), Some(45), "{}", stderr_text(&output));
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["err"]["code"], "command_not_exposed", "{document}");
    assert!(
        !harness.work().join("ok-local.ran").exists(),
        "the program never started"
    );

    // A command exposed to peers runs over the same route.
    let output = run(&["ok"]).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    assert_eq!(output.stdout, OK_OUTPUT.as_bytes());

    let _ = stop.send(());
    serving.await.expect("the peer server stops");
    harness.stop().await;
}

#[test]
fn a_daemon_without_the_method_exits_40() {
    let env = TestEnv::new().expect("private CLI environment");
    let runtime = env.runtime_dir().join("pohunek");
    fs::create_dir(&runtime).expect("create the daemon socket directory");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))
        .expect("make the socket directory private");
    let socket = runtime.join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind the daemon socket");
    // The client opens a second, dedicated connection for the run, so every
    // connection is served on a thread of its own.
    let ending = Arc::new(AtomicBool::new(false));
    let server = {
        let ending = Arc::clone(&ending);
        std::thread::spawn(move || {
            while let Ok((connection, _)) = listener.accept() {
                if ending.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::spawn(move || {
                    let mut writer = connection.try_clone().expect("clone the connection");
                    for line in BufReader::new(connection).lines() {
                        let Ok(line) = line else { break };
                        let request: Request =
                            serde_json::from_str(&line).expect("one request line");
                        let response = Response::err(
                            PROTOCOL_VERSION,
                            request.id(),
                            protocol::ProtocolError::method_not_found(request.method()),
                        )
                        .expect("response");
                        writeln!(
                            writer,
                            "{}",
                            serde_json::to_string(&response).expect("response line")
                        )
                        .expect("write the response");
                    }
                });
            }
        })
    };

    let run = |extra: &[&str]| {
        env.command(pohunek_test_support::bin_exe("pohunek"))
            .args(["host", "command", "run", "local", PACKAGE, "ok"])
            .args(extra)
            .output()
            .expect("run CLI")
    };
    let json = run(&["--json"]);
    assert_eq!(json.status.code(), Some(40), "{json:?}");
    let document: Value = serde_json::from_slice(&json.stdout).expect("one JSON document");
    assert_eq!(document["err"]["code"], "command_unsupported", "{document}");
    assert!(document["err"]["recover"].is_string(), "{document}");

    let human = run(&[]);
    assert_eq!(human.status.code(), Some(40), "{human:?}");
    assert!(human.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&human.stderr).contains("does not support package commands"),
        "{human:?}"
    );

    ending.store(true, Ordering::SeqCst);
    drop(std::os::unix::net::UnixStream::connect(&socket).expect("end the fixture daemon"));
    server.join().expect("the fixture daemon ends");
}

#[test]
fn an_unreachable_daemon_is_the_generic_failure() {
    let env = TestEnv::new().expect("private CLI environment");

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["host", "command", "run", "local", PACKAGE, "ok"])
        .output()
        .expect("run CLI");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[tokio::test]
async fn the_command_takes_no_caller_input() {
    let harness = Harness::start().await;
    harness.install_commands().await;

    let refused: [&[&str]; 8] = [
        &["--args", "x"],
        &["--arg", "x"],
        &["--env", "A=b"],
        &["--stdin", "x"],
        &["--input", "x"],
        &["--cwd", "/work"],
        &["--", "x"],
        &["extra-argument"],
    ];
    for extra in refused {
        let output = harness.run_command(PACKAGE, "ok", extra).await;
        assert_eq!(
            output.status.code(),
            Some(USAGE_STATUS),
            "{extra:?}: {}",
            stderr_text(&output)
        );
        assert!(output.stdout.is_empty(), "{extra:?}");
    }
    assert!(
        !harness.work().join("ok.ran").exists(),
        "no refused call started the program"
    );

    let help = harness.run(&["host", "command", "run", "--help"]).await;
    let help = stdout_text(&help);
    for flag in ["--timeout-ms", "--max-output-bytes", "--json"] {
        assert!(help.contains(flag), "{flag}: {help}");
    }
    for flag in ["--arg", "--env", "--stdin", "--input", "--cwd"] {
        assert!(!help.contains(flag), "{flag}: {help}");
    }
    harness.stop().await;
}

#[tokio::test]
async fn invalid_values_are_usage_errors() {
    let harness = Harness::start().await;
    harness.install_commands().await;

    let invalid: [&[&str]; 4] = [
        &["--timeout-ms", "0"],
        &["--max-output-bytes", "0"],
        &["--timeout-ms", "soon"],
        &["--max-output-bytes", "-1"],
    ];
    for extra in invalid {
        let output = harness.run_command(PACKAGE, "ok", extra).await;
        assert_eq!(
            output.status.code(),
            Some(USAGE_STATUS),
            "{extra:?}: {}",
            stderr_text(&output)
        );
    }
    for (package, command) in [("Not A Package", "ok"), (PACKAGE, "Not_A_Command")] {
        let output = harness.run_command(package, command, &[]).await;
        assert_eq!(
            output.status.code(),
            Some(USAGE_STATUS),
            "{package} {command}: {}",
            stderr_text(&output)
        );
    }
    harness.stop().await;
}

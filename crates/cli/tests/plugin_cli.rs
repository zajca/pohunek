//! Binary-level contracts of `pohunek plugin`: local-only enforcement, the
//! consent flow, selector resolution, and `--json` rendering.
//!
//! A scripted control server stands in for the daemon and records every request
//! the binary sends; the real package lifecycle against a real daemon is
//! covered by `plugin_daemon.rs`.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// Mode of every private fixture directory.
const PRIVATE_MODE: u32 = 0o700;

const DIGEST_OLD: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
/// Shares its first twelve hex characters with `DIGEST_OLD`.
const DIGEST_SIBLING: &str =
    "sha256:1111111111112222222222222222222222222222222222222222222222222222";
const DIGEST_NEW: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

type Recorded = Arc<Mutex<Vec<(String, Value)>>>;

struct Fixture {
    env: TestEnv,
    root: PathBuf,
}

/// A scripted daemon that answers each request from `reply` and records it.
struct FakeDaemon {
    recorded: Recorded,
    stop: Arc<AtomicBool>,
    socket: PathBuf,
    handle: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let root = env.root().to_path_buf();
        let path = root.join("run/pohunek");
        fs::create_dir_all(&path).expect("create fixture directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(PRIVATE_MODE))
            .expect("private fixture directory");
        Self { env, root }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/pohunek/daemon.sock")
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(arguments)
            .output()
            .expect("run pohunek binary")
    }

    /// The host agent profile directory of the hermetic environment.
    fn agents_dir(&self) -> PathBuf {
        let dir = self.env.config_home().join("pohunek/agents");
        fs::create_dir_all(&dir).expect("create the agents directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(PRIVATE_MODE))
            .expect("private agents directory");
        dir
    }

    fn profile(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.agents_dir().join(format!("{name}.toml"));
        fs::write(&path, text).expect("write profile");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("profile mode");
        path
    }

    fn file(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, b"archive").expect("write archive");
        path
    }

    fn serve(&self, reply: fn(&str, &Value) -> Value) -> FakeDaemon {
        FakeDaemon::start(&self.socket(), reply)
    }
}

impl FakeDaemon {
    fn start(socket: &Path, reply: fn(&str, &Value) -> Value) -> Self {
        let listener = UnixListener::bind(socket).expect("bind fake daemon");
        let recorded: Recorded = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let recorded = Arc::clone(&recorded);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while let Ok((stream, _)) = listener.accept() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    serve_connection(stream, reply, &recorded);
                }
            })
        };
        Self {
            recorded,
            stop,
            socket: socket.to_path_buf(),
            handle: Some(handle),
        }
    }

    /// Methods and params received so far, after the server has stopped.
    fn finish(mut self) -> Vec<(String, Value)> {
        self.stop.store(true, Ordering::SeqCst);
        // A final connection wakes the accept loop so it observes `stop`.
        drop(UnixStream::connect(&self.socket));
        if let Some(handle) = self.handle.take() {
            handle.join().expect("fake daemon thread");
        }
        self.recorded.lock().expect("recorded requests").clone()
    }
}

fn serve_connection(stream: UnixStream, reply: fn(&str, &Value) -> Value, recorded: &Recorded) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
        let request: Request = serde_json::from_str(line.trim_end()).expect("parse request");
        // A method new in the current protocol is preceded by the client's
        // `daemon.health` version probe. It is answered like a daemon would and
        // kept out of the recorded requests, which hold the package methods only.
        let result = if request.method() == "daemon.health" {
            json!({
                "status": "ok",
                "daemon_version": "fixture",
                "protocol_version": PROTOCOL_VERSION.get()
            })
        } else {
            let result = reply(request.method(), request.params());
            recorded
                .lock()
                .expect("recorded requests")
                .push((request.method().to_owned(), request.params().clone()));
            result
        };
        let response = Response::ok(PROTOCOL_VERSION, request.id(), result).expect("response");
        writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).expect("serialize response")
        )
        .expect("write response");
        line.clear();
    }
}

fn package(id: &str, version: &str, digest: &str, selected: bool) -> Value {
    json!({
        "digest": digest,
        "package": { "id": id, "version": version },
        "origin": "explicit_digest",
        "enabled": true,
        "selected": selected,
        "installed_at_unix_seconds": 1,
        "runtime_id": "acme-pi",
        "referenced": false
    })
}

fn runtime() -> Value {
    json!({
        "runtime_id": "acme-pi",
        "display_name": "Acme Pi",
        "program": "bin/pi",
        "args": ["--mode", "rpc"],
        "resumable": true,
        "forkable": false,
        "prompt_argument": false
    })
}

/// A daemon with no packages installed; every install validates and succeeds.
fn empty_daemon(method: &str, params: &Value) -> Value {
    match method {
        "package.list" => json!({ "generation": 1, "packages": [] }),
        "package.install" | "package.link" => json!({
            "status": if params["dry_run"] == true { "preview" } else { "installed" },
            "package": package("acme.pi", "1.0.0", DIGEST_NEW, true),
            "runtime": runtime(),
            "reloaded": true
        }),
        other => panic!("unexpected method {other}"),
    }
}

/// A daemon that already holds an older version of `acme.pi`.
fn daemon_with_old_version(method: &str, params: &Value) -> Value {
    match method {
        "package.list" => json!({
            "generation": 2,
            "packages": [package("acme.pi", "0.9.0", DIGEST_OLD, true)]
        }),
        "package.uninstall" => json!({ "digest": params["digest"], "reloaded": true }),
        "package.set_enabled" | "package.select" => json!({
            "package": package("acme.pi", "0.9.0", DIGEST_OLD, true),
            "reloaded": true
        }),
        _ => empty_daemon(method, params),
    }
}

/// A daemon holding two versions of `acme.pi` that share a digest prefix.
fn daemon_with_two_versions(method: &str, _params: &Value) -> Value {
    match method {
        "package.list" => json!({
            "generation": 2,
            "packages": [
                package("acme.pi", "1.0.0", DIGEST_OLD, false),
                package("acme.pi", "2.0.0", DIGEST_SIBLING, true),
            ]
        }),
        other => panic!("unexpected method {other}"),
    }
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("one JSON document")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn methods(recorded: &[(String, Value)]) -> Vec<&str> {
    recorded.iter().map(|(method, _)| method.as_str()).collect()
}

#[test]
fn remote_host_is_rejected_before_any_io_for_every_subcommand() {
    let fixture = Fixture::new();
    // No daemon listens: reaching the connect would fail differently.
    let archive = fixture.root.join("missing.tar");
    let archive = archive.to_str().expect("utf-8 path");
    let cases: Vec<Vec<&str>> = vec![
        vec!["list"],
        vec!["inspect", "acme.pi"],
        vec!["install", archive, "--sha256", DIGEST_NEW],
        vec!["link", archive],
        vec!["update", "acme.pi", archive, "--sha256", DIGEST_NEW],
        vec!["select", "acme.pi"],
        vec!["enable", "acme.pi"],
        vec!["disable", "acme.pi"],
        vec!["uninstall", "acme.pi", "--yes"],
        vec!["doctor"],
        vec!["profile", "list"],
        vec!["profile", "migrate", "work", "--yes"],
    ];
    for case in cases {
        let mut plain = vec!["--host", "build-box", "plugin"];
        plain.extend(&case);
        let output = fixture.run(&plain);
        assert_eq!(output.status.code(), Some(1), "{case:?}");
        let text = stderr_text(&output);
        assert!(text.contains("this machine only"), "{case:?}: {text}");
        assert!(!text.contains("cannot reach"), "{case:?}: {text}");
        assert!(!text.contains("not an existing"), "{case:?}: {text}");

        let mut json = plain.clone();
        json.push("--json");
        let output = fixture.run(&json);
        assert_eq!(output.status.code(), Some(1), "{case:?}");
        let document = stdout_json(&output);
        assert_eq!(document["err"]["code"], "plugin_local_only", "{case:?}");
        assert!(document.get("ok").is_none());
    }
}

#[test]
fn explicit_local_host_is_accepted() {
    let fixture = Fixture::new();
    let daemon = fixture.serve(empty_daemon);
    let output = fixture.run(&["--host", "local", "plugin", "list"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    assert_eq!(methods(&daemon.finish()), ["package.list"]);
}

#[test]
fn missing_sources_fail_before_connecting() {
    let fixture = Fixture::new();
    let missing = fixture.root.join("missing.tar");
    let missing = missing.to_str().expect("utf-8 path");
    let output = fixture.run(&[
        "plugin", "install", missing, "--sha256", DIGEST_NEW, "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let document = stdout_json(&output);
    assert_eq!(document["err"]["code"], "plugin_source_invalid");
    assert!(document["err"]["msg"]
        .as_str()
        .expect("msg")
        .contains("missing.tar"));

    // A file where a directory is required.
    let file = fixture.file("a-file");
    let output = fixture.run(&["plugin", "link", file.to_str().expect("utf-8 path")]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_text(&output).contains("not an existing directory"));
}

#[test]
fn install_without_yes_previews_and_never_sends_the_install() {
    let fixture = Fixture::new();
    let archive = fixture.file("pi.tar");
    let daemon = fixture.serve(empty_daemon);

    let output = fixture.run(&[
        "plugin",
        "install",
        archive.to_str().expect("utf-8 path"),
        "--sha256",
        DIGEST_NEW,
    ]);

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "acme.pi 1.0.0",
        DIGEST_NEW,
        "acme-pi",
        "\"bin/pi\"",
        "Acme Pi",
    ] {
        assert!(stdout.contains(needle), "missing {needle:?}: {stdout}");
    }
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("needs your consent; nothing was changed"),
        "{stderr}"
    );
    assert!(stderr.contains("--yes"), "{stderr}");
    let recorded = daemon.finish();
    assert_eq!(methods(&recorded), ["package.list", "package.install"]);
    assert_eq!(recorded[1].1["dry_run"], true);
}

#[test]
fn json_without_yes_reports_consent_required_as_the_only_document() {
    let fixture = Fixture::new();
    let archive = fixture.file("pi.tar");
    let daemon = fixture.serve(empty_daemon);

    let output = fixture.run(&[
        "plugin",
        "install",
        archive.to_str().expect("utf-8 path"),
        "--sha256",
        DIGEST_NEW,
        "--json",
    ]);

    assert_eq!(output.status.code(), Some(1));
    let document = stdout_json(&output);
    assert_eq!(document["err"]["code"], "consent_required");
    assert!(document["err"]["msg"]
        .as_str()
        .expect("msg")
        .contains("acme.pi 1.0.0"));
    assert_eq!(
        methods(&daemon.finish()),
        ["package.list", "package.install"]
    );
}

#[test]
fn install_with_yes_sends_exactly_a_dry_run_then_the_install() {
    let fixture = Fixture::new();
    let archive = fixture.file("pi.tar");
    let daemon = fixture.serve(empty_daemon);

    let output = fixture.run(&[
        "plugin",
        "install",
        archive.to_str().expect("utf-8 path"),
        "--sha256",
        DIGEST_NEW,
        "--yes",
        "--json",
    ]);

    assert!(output.status.success(), "{}", stderr_text(&output));
    assert_eq!(stdout_json(&output)["ok"]["status"], "installed");
    let recorded = daemon.finish();
    assert_eq!(
        methods(&recorded),
        ["package.list", "package.install", "package.install"]
    );
    let canonical = fs::canonicalize(&archive).expect("canonical archive");
    for (index, dry_run) in [(1, true), (2, false)] {
        let params = &recorded[index].1;
        assert_eq!(params["dry_run"], dry_run);
        assert_eq!(
            params["archive_path"],
            canonical.to_str().expect("utf-8 path")
        );
        assert_eq!(
            params["trust"],
            json!({ "kind": "explicit_digest", "digest": DIGEST_NEW })
        );
        assert_eq!(params["enable"], true);
    }
    // First version of the id: it becomes the selected one.
    assert_eq!(recorded[2].1["select"], true);
}

#[test]
fn install_of_a_new_version_does_not_steal_selection_and_no_enable_is_honored() {
    let fixture = Fixture::new();
    let archive = fixture.file("pi.tar");
    let daemon = fixture.serve(daemon_with_old_version);

    let output = fixture.run(&[
        "plugin",
        "install",
        archive.to_str().expect("utf-8 path"),
        "--sha256",
        DIGEST_NEW,
        "--no-enable",
        "--yes",
    ]);

    assert!(output.status.success(), "{}", stderr_text(&output));
    let recorded = daemon.finish();
    let install = &recorded.last().expect("install request").1;
    assert_eq!(install["dry_run"], false);
    assert_eq!(install["select"], false);
    assert_eq!(install["enable"], false);
}

#[test]
fn catalog_trust_is_sent_as_an_absolute_catalog_path() {
    let fixture = Fixture::new();
    let archive = fixture.file("pi.tar");
    let catalog = fixture.file("catalog.json");
    let daemon = fixture.serve(empty_daemon);

    let output = fixture.run(&[
        "plugin",
        "install",
        archive.to_str().expect("utf-8 path"),
        "--catalog",
        catalog.to_str().expect("utf-8 path"),
        "--yes",
    ]);

    assert!(output.status.success(), "{}", stderr_text(&output));
    let recorded = daemon.finish();
    assert_eq!(
        recorded[1].1["trust"],
        json!({
            "kind": "catalog",
            "catalog_path": fs::canonicalize(&catalog).expect("canonical").to_str().expect("utf-8 path")
        })
    );
}

#[test]
fn link_follows_the_consent_pattern() {
    let fixture = Fixture::new();
    let directory = fixture.root.join("pkg");
    fs::create_dir(&directory).expect("package directory");
    let directory = directory.to_str().expect("utf-8 path");

    let daemon = fixture.serve(empty_daemon);
    let output = fixture.run(&["plugin", "link", directory]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(methods(&daemon.finish()), ["package.list", "package.link"]);
    fs::remove_file(fixture.socket()).expect("remove socket");

    let daemon = fixture.serve(empty_daemon);
    let output = fixture.run(&["plugin", "link", directory, "--yes"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    let recorded = daemon.finish();
    assert_eq!(
        methods(&recorded),
        ["package.list", "package.link", "package.link"]
    );
    assert_eq!(recorded[1].1["dry_run"], true);
    assert_eq!(recorded[2].1["dry_run"], false);
    assert_eq!(
        recorded[2].1["directory"],
        fs::canonicalize(directory)
            .expect("canonical")
            .to_str()
            .expect("utf-8 path")
    );
}

#[test]
fn update_installs_side_by_side_selected_and_enabled() {
    let fixture = Fixture::new();
    let archive = fixture.file("pi2.tar");
    let daemon = fixture.serve(daemon_with_old_version);

    let output = fixture.run(&[
        "plugin",
        "update",
        "acme.pi",
        archive.to_str().expect("utf-8 path"),
        "--sha256",
        DIGEST_NEW,
        "--yes",
    ]);

    assert!(output.status.success(), "{}", stderr_text(&output));
    let recorded = daemon.finish();
    assert_eq!(
        methods(&recorded),
        ["package.list", "package.install", "package.install"]
    );
    let install = &recorded[2].1;
    assert_eq!(install["enable"], true);
    assert_eq!(install["select"], true);
    assert_eq!(install["dry_run"], false);
}

#[test]
fn update_requires_the_id_to_be_installed_and_the_archive_to_match_it() {
    let fixture = Fixture::new();
    let archive = fixture.file("pi2.tar");
    let archive = archive.to_str().expect("utf-8 path");

    let daemon = fixture.serve(empty_daemon);
    let output = fixture.run(&[
        "plugin", "update", "acme.pi", archive, "--sha256", DIGEST_NEW, "--yes",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_text(&output).contains("no installed package has the id acme.pi"));
    assert_eq!(methods(&daemon.finish()), ["package.list"]);
    fs::remove_file(fixture.socket()).expect("remove socket");

    // Installed id differs from what the archive declares: only the dry run is sent.
    let daemon = fixture.serve(|method, params| match method {
        "package.list" => json!({
            "generation": 1,
            "packages": [package("acme.other", "1.0.0", DIGEST_OLD, true)]
        }),
        _ => empty_daemon(method, params),
    });
    let output = fixture.run(&[
        "plugin",
        "update",
        "acme.other",
        archive,
        "--sha256",
        DIGEST_NEW,
        "--yes",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout_json(&output)["err"]["code"],
        "plugin_update_id_mismatch"
    );
    let recorded = daemon.finish();
    assert_eq!(methods(&recorded), ["package.list", "package.install"]);
    assert_eq!(recorded[1].1["dry_run"], true);
}

#[test]
fn uninstall_needs_consent_and_forwards_the_modified_root_option() {
    let fixture = Fixture::new();
    let daemon = fixture.serve(daemon_with_old_version);
    let output = fixture.run(&["plugin", "uninstall", "acme.pi"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("acme.pi 0.9.0"));
    assert_eq!(methods(&daemon.finish()), ["package.list"]);
    fs::remove_file(fixture.socket()).expect("remove socket");

    let daemon = fixture.serve(daemon_with_old_version);
    let output = fixture.run(&[
        "plugin",
        "uninstall",
        "acme.pi",
        "--remove-modified",
        "--yes",
        "--json",
    ]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    let recorded = daemon.finish();
    assert_eq!(methods(&recorded), ["package.list", "package.uninstall"]);
    assert_eq!(
        recorded[1].1,
        json!({ "digest": DIGEST_OLD, "remove_modified": true })
    );
}

#[test]
fn ambiguous_selectors_never_reach_a_mutating_request() {
    let fixture = Fixture::new();
    let daemon = fixture.serve(daemon_with_two_versions);
    for action in ["enable", "disable", "select"] {
        let output = fixture.run(&[
            "plugin",
            action,
            "acme.pi",
            "--digest",
            "sha256:111111111111",
            "--json",
        ]);
        assert_eq!(output.status.code(), Some(1), "{action}");
        assert_eq!(
            stdout_json(&output)["err"]["code"],
            "plugin_selector_ambiguous",
            "{action}"
        );
    }
    let output = fixture.run(&["plugin", "uninstall", "acme.pi", "--yes"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(daemon
        .finish()
        .iter()
        .all(|(method, _)| method == "package.list"));
}

#[test]
fn narrowing_selects_one_version_for_a_mutation_and_inspect_prefers_the_selected() {
    let fixture = Fixture::new();
    let daemon = fixture.serve(|method, params| match method {
        "package.inspect" => json!({
            "package": package("acme.pi", "2.0.0", params["digest"].as_str().expect("digest"), true),
            "runtime": runtime()
        }),
        "package.set_enabled" => json!({
            "package": package("acme.pi", "1.0.0", params["digest"].as_str().expect("digest"), false),
            "reloaded": true
        }),
        _ => daemon_with_two_versions(method, params),
    });

    let output = fixture.run(&["plugin", "disable", "acme.pi", "--version", "1.0.0"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Live sessions keep running"));

    let output = fixture.run(&["plugin", "inspect", "acme.pi"]);
    assert!(output.status.success(), "{}", stderr_text(&output));

    let recorded = daemon.finish();
    let disable = recorded
        .iter()
        .find(|(method, _)| method == "package.set_enabled")
        .expect("set_enabled request");
    assert_eq!(disable.1, json!({ "digest": DIGEST_OLD, "enabled": false }));
    let inspect = recorded
        .iter()
        .find(|(method, _)| method == "package.inspect")
        .expect("inspect request");
    assert_eq!(inspect.1["digest"], DIGEST_SIBLING);
}

#[test]
fn doctor_exits_one_on_findings_with_a_single_json_document() {
    let fixture = Fixture::new();
    let daemon = fixture.serve(|_, _| {
        json!({
            "generation": 4,
            "findings": [{
                "kind": "fault",
                "digest": DIGEST_OLD,
                "package": { "id": "acme.pi", "version": "1.0.0" },
                "fault": "root_modified",
                "referenced": false
            }]
        })
    });

    let output = fixture.run(&["plugin", "doctor", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout_json(&output)["ok"]["findings"][0]["fault"],
        "root_modified"
    );

    let output = fixture.run(&["plugin", "doctor", "acme.pi"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("root modified"));

    let recorded = daemon.finish();
    assert_eq!(recorded[0].1, json!({}));
    assert_eq!(recorded[1].1, json!({ "package": "acme.pi" }));
}

#[test]
fn doctor_exits_zero_when_clean() {
    let fixture = Fixture::new();
    let daemon = fixture.serve(|_, _| json!({ "generation": 4, "findings": [] }));
    let output = fixture.run(&["plugin", "doctor"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("No problems found"));
    drop(daemon.finish());
}

// ----- plugin profile ------------------------------------------------------

/// A value that must never reach stdout, stderr or a JSON document.
const SENTINEL: &str = "sk-sentinel-secret-9f3a";

/// A profile with comments, a multi-line `[env]` secret and no pin.
fn profile_text() -> String {
    format!(
        "# Work profile.\nbase = \"acme-pi\"\n# Flags.\nargs = [\"--mode\", \"rpc\"]\n\n\
         [env]\n# Gateway credentials.\nAPI_TOKEN = \"{SENTINEL}\"\n\
         MULTI = \"\"\"\nfirst {SENTINEL}\nsecond\n\"\"\"\n"
    )
}

fn pinned_text(digest: &str) -> String {
    profile_text().replacen(
        "base = \"acme-pi\"\n",
        &format!("base = \"acme-pi\"\npackage = \"acme.pi\"\ndigest = \"{digest}\"\n"),
        1,
    )
}

/// A daemon serving `acme-pi` through one selected package; `package.bind_profile`
/// previews on a dry run and binds otherwise.
fn daemon_serving_pi(method: &str, params: &Value) -> Value {
    match method {
        "package.list" => json!({
            "generation": 2,
            "packages": [package("acme.pi", "1.0.0", DIGEST_NEW, true)]
        }),
        "package.bind_profile" => bind_result(
            if params["dry_run"] == true {
                "preview"
            } else {
                "bound"
            },
            params,
        ),
        other => panic!("unexpected method {other}"),
    }
}

/// A daemon whose profile already pins the selected package.
fn daemon_with_pinned_profile(method: &str, params: &Value) -> Value {
    match method {
        "package.bind_profile" => {
            let mut result = bind_result("unchanged", params);
            result["previous"] = json!(DIGEST_NEW);
            result["reloaded"] = json!(false);
            result
        }
        other => panic!("unexpected method {other}"),
    }
}

/// The `package.bind_profile` result for the profile named in `params`.
fn bind_result(status: &str, params: &Value) -> Value {
    json!({
        "status": status,
        "profile": params["profile"],
        "base": "acme-pi",
        "package": package("acme.pi", "1.0.0", DIGEST_NEW, true),
        "runtime": runtime(),
        "reloaded": status == "bound"
    })
}

fn assert_no_secret(output: &Output) {
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        assert!(
            !String::from_utf8_lossy(bytes).contains(SENTINEL),
            "{stream} leaked the sentinel"
        );
    }
}

#[test]
fn profile_list_reports_states_in_a_table_and_in_json() {
    let fixture = Fixture::new();
    fixture.profile("builtin", "base = \"shell\"\n", 0o600);
    fixture.profile("fresh", &profile_text(), 0o600);
    fixture.profile("pinned", &pinned_text(DIGEST_NEW), 0o600);
    fixture.profile("gone", &pinned_text(DIGEST_OLD), 0o600);
    fixture.profile(
        "broken",
        &format!("base = \"acme-pi\"\n[env]\nTOKEN = {SENTINEL}\n"),
        0o600,
    );
    let daemon = fixture.serve(daemon_serving_pi);

    let output = fixture.run(&["plugin", "profile", "list"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    assert_no_secret(&output);
    let table = String::from_utf8_lossy(&output.stdout).into_owned();
    for needle in [
        "NAME",
        "builtin",
        "needs_migration",
        "pinned",
        "pin_not_installed",
        "unreadable (line 3:",
        "sha256:222222222222",
    ] {
        assert!(table.contains(needle), "missing {needle:?}: {table}");
    }
    assert!(
        !table.contains(DIGEST_NEW),
        "table must abbreviate: {table}"
    );

    let output = fixture.run(&["plugin", "profile", "list", "--json"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    assert_no_secret(&output);
    let document = stdout_json(&output);
    let profiles = document["ok"]["profiles"].as_array().expect("profiles");
    let state_of = |name: &str| {
        profiles
            .iter()
            .find(|entry| entry["name"] == name)
            .map(|entry| entry["state"].as_str().expect("state").to_owned())
    };
    assert_eq!(state_of("builtin").as_deref(), Some("builtin"));
    assert_eq!(state_of("fresh").as_deref(), Some("needs_migration"));
    assert_eq!(state_of("pinned").as_deref(), Some("pinned"));
    assert_eq!(state_of("gone").as_deref(), Some("pin_not_installed"));
    assert_eq!(state_of("broken").as_deref(), Some("unreadable"));
    let pinned = profiles
        .iter()
        .find(|entry| entry["name"] == "pinned")
        .expect("pinned entry");
    assert_eq!(pinned["digest"], DIGEST_NEW, "JSON carries the full digest");
    assert_eq!(pinned["package"], "acme.pi");
    drop(daemon.finish());
}

#[test]
fn profile_list_without_an_agents_directory_is_empty() {
    let fixture = Fixture::new();
    let daemon = fixture.serve(daemon_serving_pi);
    let output = fixture.run(&["plugin", "profile", "list"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "No host agent profiles.\n"
    );
    drop(daemon.finish());
}

#[test]
fn profile_migrate_without_yes_asks_the_daemon_for_a_preview_only() {
    let fixture = Fixture::new();
    let path = fixture.profile("work", &profile_text(), 0o600);
    let daemon = fixture.serve(daemon_serving_pi);

    let output = fixture.run(&["plugin", "profile", "migrate", "work"]);
    assert_eq!(output.status.code(), Some(1));
    assert_no_secret(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "work",
        "acme-pi",
        "acme.pi 1.0.0",
        DIGEST_NEW,
        "Current pin: none",
    ] {
        assert!(stdout.contains(needle), "missing {needle:?}: {stdout}");
    }
    assert!(stderr_text(&output).contains("needs your consent; nothing was changed"));

    let output = fixture.run(&["plugin", "profile", "migrate", "work", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert_no_secret(&output);
    assert_eq!(stdout_json(&output)["err"]["code"], "consent_required");

    assert_eq!(fs::read_to_string(&path).expect("read"), profile_text());
    let recorded = daemon.finish();
    assert_eq!(
        methods(&recorded),
        ["package.bind_profile", "package.bind_profile"]
    );
    for (_, params) in &recorded {
        assert_eq!(params["profile"], "work");
        assert_eq!(params["dry_run"], true);
        assert!(params.get("digest").is_none(), "{params}");
    }
}

#[test]
fn profile_migrate_with_yes_binds_through_the_daemon_and_never_writes_the_file() {
    let fixture = Fixture::new();
    let path = fixture.profile("work", &profile_text(), 0o600);
    let daemon = fixture.serve(daemon_serving_pi);

    let output = fixture.run(&["plugin", "profile", "migrate", "work", "--yes", "--json"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    assert_no_secret(&output);
    let document = stdout_json(&output);
    assert_eq!(document["ok"]["status"], "bound");
    assert_eq!(document["ok"]["package"]["digest"], DIGEST_NEW);
    assert_eq!(document["ok"]["profile"], "work");
    assert_eq!(document["ok"]["base"], "acme-pi");

    let output = fixture.run(&["plugin", "profile", "migrate", "work", "--yes"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Pinned profile work to acme.pi"));

    // The daemon owns the rewrite: the command leaves the file alone.
    assert_eq!(fs::read_to_string(&path).expect("read"), profile_text());
    let recorded = daemon.finish();
    assert_eq!(
        methods(&recorded),
        [
            "package.bind_profile",
            "package.bind_profile",
            "package.bind_profile",
            "package.bind_profile"
        ]
    );
    // The consented call names the digest the preview announced.
    for pair in recorded.chunks(2) {
        assert_eq!(pair[0].1["dry_run"], true);
        assert_eq!(pair[1].1["dry_run"], false);
        assert_eq!(pair[1].1["digest"], DIGEST_NEW);
        assert_eq!(pair[1].1["profile"], "work");
    }
}

#[test]
fn profile_migrate_reports_an_already_pinned_profile_without_consent() {
    let fixture = Fixture::new();
    fixture.profile("work", &pinned_text(DIGEST_NEW), 0o600);
    let daemon = fixture.serve(daemon_with_pinned_profile);

    let output = fixture.run(&["plugin", "profile", "migrate", "work"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("nothing changed"));
    let output = fixture.run(&["plugin", "profile", "migrate", "work", "--json"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    assert_eq!(stdout_json(&output)["ok"]["status"], "unchanged");

    let recorded = daemon.finish();
    assert_eq!(
        methods(&recorded),
        ["package.bind_profile", "package.bind_profile"]
    );
    assert!(recorded.iter().all(|(_, params)| params["dry_run"] == true));
}

#[test]
fn profile_migrate_passes_a_full_digest_and_resolves_a_prefix_through_the_package_list() {
    let fixture = Fixture::new();
    fixture.profile("work", &profile_text(), 0o600);
    let daemon = fixture.serve(daemon_serving_pi);

    let output = fixture.run(&[
        "plugin", "profile", "migrate", "work", "--digest", DIGEST_NEW, "--yes",
    ]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    let output = fixture.run(&[
        "plugin",
        "profile",
        "migrate",
        "work",
        "--digest",
        "222222222222",
        "--yes",
    ]);
    assert!(output.status.success(), "{}", stderr_text(&output));

    // A prefix of a digest that is not installed never reaches the daemon's
    // bind.
    let output = fixture.run(&[
        "plugin",
        "profile",
        "migrate",
        "work",
        "--digest",
        "111111111111",
        "--yes",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout_json(&output)["err"]["code"],
        "profile_target_invalid"
    );

    let recorded = daemon.finish();
    assert_eq!(
        methods(&recorded),
        [
            "package.bind_profile",
            "package.bind_profile",
            "package.list",
            "package.bind_profile",
            "package.bind_profile",
            "package.list",
        ]
    );
    assert_eq!(recorded[0].1["digest"], DIGEST_NEW);
    assert_eq!(recorded[3].1["digest"], DIGEST_NEW);
}

#[test]
fn profile_migrate_rejects_invalid_names_before_any_io() {
    let fixture = Fixture::new();
    for name in ["../escape", ".hidden", "a/b", "-x", "a..b"] {
        let output = fixture.run(&["plugin", "profile", "migrate", name, "--yes"]);
        assert_eq!(output.status.code(), Some(2), "{name}");
        assert!(!stderr_text(&output).contains("cannot reach"), "{name}");
    }
}

#[test]
fn profile_list_refuses_an_unsafe_agents_directory() {
    let fixture = Fixture::new();
    fixture.profile("work", &profile_text(), 0o600);
    fs::set_permissions(fixture.agents_dir(), fs::Permissions::from_mode(0o770))
        .expect("loosen directory");
    let daemon = fixture.serve(daemon_serving_pi);
    let output = fixture.run(&["plugin", "profile", "list", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert_no_secret(&output);
    assert_eq!(
        stdout_json(&output)["err"]["code"],
        "profile_directory_unusable"
    );
    drop(daemon.finish());
    fs::set_permissions(
        fixture.agents_dir(),
        fs::Permissions::from_mode(PRIVATE_MODE),
    )
    .expect("restore directory");
}

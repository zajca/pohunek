//! Supported-boundary coverage of the daemon worker probe through CLI doctor.

// Rust guideline compliant 2026-10-11

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use pohunek_test_support::fs::write_executable;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::worker_binary;
use serde_json::Value;

#[path = "../../daemon/tests/support/official_runtimes.rs"]
mod official_runtimes;
#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, Harness};

const WORKER_CANARY: &str = "WORKER_COMPAT_SECRET_CANARY";

fn check<'a>(document: &'a Value, name: &str) -> &'a Value {
    document["ok"]["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("doctor contains {name}: {document}"))
}

async fn assert_worker_failure(harness: &Harness, worker: &std::path::Path, detail: &str) {
    let json = harness.run(&["doctor", "--json"]).await;
    assert_eq!(json.status.code(), Some(1), "JSON doctor must fail");
    let document: Value = serde_json::from_slice(&json.stdout).expect("doctor JSON");
    assert_eq!(document["ok"]["overall"], "fail");
    let worker_check = check(&document, "worker_executable");
    assert_eq!(worker_check["status"], "fail");
    assert!(worker_check["detail"]
        .as_str()
        .unwrap_or_default()
        .contains(detail));
    let profile = check(&document, "agent_profile_broken");
    assert_eq!(profile["status"], "fail");
    assert!(!json
        .stdout
        .windows(worker.as_os_str().as_encoded_bytes().len())
        .any(|window| window == worker.as_os_str().as_encoded_bytes()));
    assert!(!json
        .stdout
        .windows(WORKER_CANARY.len())
        .any(|window| window == WORKER_CANARY.as_bytes()));

    let human = harness.run(&["doctor"]).await;
    assert_eq!(human.status.code(), Some(1), "human doctor must fail");
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.contains("worker_executable"));
    assert!(human.contains("agent_profile_broken"));
    assert!(human.contains(detail));
    assert!(human.contains("overall: fail"));
    assert!(!human.contains(&worker.display().to_string()));
    assert!(!human.contains(WORKER_CANARY));
}

#[tokio::test]
async fn doctor_reports_missing_and_incompatible_selected_worker_through_cli() {
    let harness_root = pohunek_test_support::tempdir().expect("create worker source root");
    let selected_worker = harness_root.path().join("selected-sessiond");
    fs::copy(worker_binary(), &selected_worker).expect("copy the real session worker");
    fs::set_permissions(&selected_worker, fs::Permissions::from_mode(0o700))
        .expect("make copied worker executable private");

    let harness = Harness::start_with_worker(selected_worker.clone()).await;
    let (code, session) = harness
        .json(&[
            "session",
            "new",
            "--agent",
            "shell",
            "--cwd",
            path_str(harness.env.cwd()),
        ])
        .await;
    assert_eq!(
        code, 0,
        "start a session with the selected real worker: {session}"
    );
    assert!(
        session["ok"]["runtime"]["worker_id"].is_string(),
        "{session}"
    );
    harness.profile("broken", "not = [valid");

    let backup = harness.env.root().join("selected-sessiond.backup");
    fs::rename(&selected_worker, &backup).expect("remove selected worker executable");
    assert_worker_failure(&harness, &selected_worker, "worker is missing or unsafe").await;
    fs::rename(&backup, &selected_worker).expect("restore selected worker executable");

    let incompatible = harness.env.root().join("incompatible-sessiond");
    write_executable(
        &incompatible,
        format!("#!/bin/sh\nprintf '%s\\n' '{WORKER_CANARY}' >&2\nexit 71\n"),
    )
    .expect("write an incompatible worker probe");
    fs::rename(&selected_worker, &backup).expect("move selected worker out of the path");
    fs::rename(&incompatible, &selected_worker)
        .expect("install incompatible probe at selected path");
    assert_worker_failure(&harness, &selected_worker, "malformed or incompatible").await;
    fs::remove_file(&selected_worker).expect("remove incompatible worker probe");
    fs::rename(&backup, &selected_worker).expect("restore selected real worker");
    harness.stop().await;
}

/// An agent executable in a directory others can write is named, with the
/// reason, instead of being reported as missing.
#[tokio::test]
async fn doctor_names_an_agent_executable_the_trust_policy_refused() {
    let catalog = official_runtimes::OfficialCatalog::new();
    let harness = Harness::start_with_trust(catalog.trust()).await;
    let scratch = harness.env.root().join("official-packages");
    fs::create_dir_all(&scratch).expect("create the package scratch directory");
    catalog.install(&harness.sessions, &scratch).await;
    let loose = harness.env.root().join("loose-bin");
    fs::create_dir(&loose).expect("create the loose directory");
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o775))
        .expect("make the directory group-writable");
    let candidate = loose.join("claude");
    write_executable(&candidate, "#!/bin/sh\nexit 0\n").expect("write the candidate");

    let mut process_env = ProcessEnv::lock();
    process_env.set("PATH", &loose);
    let output = harness.run(&["doctor", "--json"]).await;
    drop(process_env);
    let document: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    let runtime = check(&document, "agent_runtime_claude");
    let detail = runtime["detail"].as_str().expect("detail");
    assert!(
        detail.contains(&candidate.display().to_string()) && detail.contains("not trusted"),
        "the refused candidate is named: {runtime}"
    );
    harness.stop().await;
}

const SERVICE_TOML: &str = r#"schema_version = 7
prefix = "/home/u/.local"
active_version = "0.35.2"

[namespace]
uid = 1000
state_root = "/home/u/.local/state/pohunek"
runtime_root = "/run/user/1000/pohunek"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = EXIT_TIMEOUT_MS
daemon_restart_throttle_ms = 5000

[environment]
search_path = []
allowlist = ["PATH"]

[sweep]
grace_ms = 5000

[input]
initial_startup_grace_ms = 5000
submit_delay_ms = 150
write_stall_timeout_ms = 3000

[waits]
turn_default_ms = 300000
turn_max_ms = 1800000

[hooks]
stderr_tail_bytes = 4096

[events]
max_file_bytes = 33554432
max_files = 8
max_age_days = 14

[limits]
open_files = 8192
"#;

fn install_service_toml(harness: &Harness, exit_timeout_ms: u32) {
    let dir = harness.env.config_home().join("pohunek");
    fs::create_dir_all(&dir).expect("create the config directory");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod config directory");
    let file = dir.join("service.toml");
    fs::write(
        &file,
        SERVICE_TOML.replace("EXIT_TIMEOUT_MS", &exit_timeout_ms.to_string()),
    )
    .expect("write service.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod service.toml");
}

#[tokio::test]
async fn doctor_warns_when_shutdown_bounds_exceed_the_exit_timeout_through_cli() {
    let harness = Harness::start().await;

    install_service_toml(&harness, 30_000);
    let json = harness.run(&["doctor", "--json"]).await;
    let document: Value = serde_json::from_slice(&json.stdout).expect("doctor JSON");
    assert_eq!(
        check(&document, "service_config_shutdown_budget")["status"],
        "ok"
    );

    install_service_toml(&harness, 20_000);
    let json = harness.run(&["doctor", "--json"]).await;
    let document: Value = serde_json::from_slice(&json.stdout).expect("doctor JSON");
    let budget = check(&document, "service_config_shutdown_budget");
    assert_eq!(budget["status"], "warn", "{document}");
    assert!(
        budget["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("daemon_exit_timeout_ms = 20000"),
        "{budget}"
    );
    harness.stop().await;
}

/// Writes a `service.toml` that describes the hermetic installation of `harness`
/// and records `search_path`.
fn install_recording_service_toml(harness: &Harness, search_path: &std::path::Path) {
    use std::os::unix::fs::MetadataExt as _;

    let env = &harness.env;
    let state_root = env.state_home().join("pohunek");
    let runtime_root = env.runtime_dir().join("pohunek");
    for root in [&state_root, &runtime_root] {
        fs::create_dir_all(root).expect("create a namespace root");
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).expect("chmod the root");
    }
    let canonical = |path: &std::path::Path| fs::canonicalize(path).expect("canonical path");
    let uid = fs::metadata(env.root()).expect("root metadata").uid();
    let text = SERVICE_TOML
        .replace("EXIT_TIMEOUT_MS", "30000")
        .replace(
            "/home/u/.local\"\nactive",
            &format!("{}\"\nactive", env.root().join("prefix").display()),
        )
        .replace("uid = 1000", &format!("uid = {uid}"))
        .replace(
            "state_root = \"/home/u/.local/state/pohunek\"",
            &format!("state_root = \"{}\"", canonical(&state_root).display()),
        )
        .replace(
            "runtime_root = \"/run/user/1000/pohunek\"",
            &format!("runtime_root = \"{}\"", canonical(&runtime_root).display()),
        )
        .replace(
            "search_path = []",
            &format!("search_path = [\"{}\"]", canonical(search_path).display()),
        );
    let dir = env.config_home().join("pohunek");
    fs::create_dir_all(&dir).expect("create the config directory");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod config directory");
    let file = dir.join("service.toml");
    fs::write(&file, text).expect("write service.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod service.toml");
}

/// A login shell that reports `path` as the whole `PATH` and then runs the
/// command it is given, or exits with failure when `path` is `None`.
fn login_shell(
    harness: &Harness,
    name: &str,
    path: Option<&std::path::Path>,
) -> std::path::PathBuf {
    let shell = harness.env.root().join(name);
    let body = match path {
        Some(path) => format!(
            "#!/bin/sh\nPATH='{}'; export PATH\nexec /bin/sh -c \"$3\"\n",
            path.display()
        ),
        None => "#!/bin/sh\nexit 1\n".to_owned(),
    };
    write_executable(&shell, body).expect("write the login shell");
    shell
}

async fn doctor_with_shell(harness: &Harness, shell: &std::path::Path) -> Value {
    let mut command = harness
        .env
        .tokio_command(pohunek_test_support::bin_exe("pohunek"));
    command.args(["doctor", "--json"]).env("SHELL", shell);
    let output = pohunek_test_support::wait::guard("the doctor run to finish", command.output())
        .await
        .expect("run the pohunek binary");
    serde_json::from_slice(&output.stdout).expect("doctor JSON")
}

fn login_path_check(document: &Value) -> Option<&Value> {
    document["ok"]["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("doctor checks: {document}"))
        .iter()
        .find(|check| check["name"] == "service_search_path_agents")
}

/// `pohunek doctor` reports an agent program only the login shell's path
/// resolves on macOS, where `service.toml` records a search path, and adds no
/// such check elsewhere or when the login shell gives no answer. The
/// installation, the login shell and the programs are below the hermetic
/// root. The platform gate is redundant with the probe's own macOS-only
/// discovery, so removing it is observable on no lane; the macOS lane fails
/// when the check call is removed or the gate is negated.
#[tokio::test]
async fn doctor_reports_agents_only_the_login_shell_path_resolves_through_cli() {
    let harness = Harness::start().await;
    let root = harness.env.root();
    let recorded = root.join("recorded-bin");
    let login = root.join("login-bin");
    for dir in [&recorded, &login] {
        fs::create_dir_all(dir).expect("create a program directory");
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).expect("chmod directory");
    }
    // The login path gains the host's fallback directories, so the verdict
    // names only programs this fixture controls: `codex` resolves through both
    // paths, `claude` only through the login shell's directory, which comes first.
    for (dir, name) in [(&login, "claude"), (&login, "codex"), (&recorded, "codex")] {
        write_executable(dir.join(name), "#!/bin/sh\nexit 0\n").expect("write a program");
    }
    install_recording_service_toml(&harness, &recorded);
    let macos = cfg!(target_os = "macos");

    let behind = login_shell(&harness, "login-shell", Some(&login));
    let document = doctor_with_shell(&harness, &behind).await;
    if macos {
        let finding = login_path_check(&document).expect("macOS reports the finding");
        assert_eq!(finding["status"], "warn", "{finding}");
        let detail = finding["detail"].as_str().unwrap_or_default();
        let claude = fs::canonicalize(&login)
            .expect("canonical login dir")
            .join("claude");
        assert!(
            detail.contains(&format!("claude ({})", claude.display()))
                && !detail.contains("codex")
                && detail.contains("pohunek service refresh-path --dry-run"),
            "{detail}"
        );
    } else {
        assert!(
            login_path_check(&document).is_none(),
            "only macOS records a path and probes the login shell: {document}"
        );
    }

    for name in ["claude", "codex"] {
        write_executable(recorded.join(name), "#!/bin/sh\nexit 0\n").expect("write a program");
    }
    let same = login_shell(&harness, "same-shell", Some(&recorded));
    let document = doctor_with_shell(&harness, &same).await;
    if macos {
        let finding = login_path_check(&document).expect("macOS reports the clean verdict");
        assert_eq!(finding["status"], "ok", "{finding}");
    } else {
        assert!(login_path_check(&document).is_none(), "{document}");
    }

    let failing = login_shell(&harness, "failing-shell", None);
    let document = doctor_with_shell(&harness, &failing).await;
    assert!(
        login_path_check(&document).is_none(),
        "a failed login shell gives no verdict: {document}"
    );
    harness.stop().await;
}

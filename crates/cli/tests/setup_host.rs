//! Bare `pohunek setup`: the consent gate and the plan gate, through the built
//! binary.
//!
//! Every scenario starts from a private environment and checks that the host
//! is unchanged after the run: the file tree under the test root is the same
//! before and after. The consent and archive scenarios need no service
//! manager, because they stop before the plan. On Linux the plan of a fresh
//! host reads the systemd user manager's `PATH`, so the scenarios that reach
//! the plan are ignored unless `POHUNEK_SYSTEMD_E2E=1` runs them with the
//! user's session bus (the CI job `real systemd supervision`); macOS runs them
//! in every test run. The apply path is covered by `setup_host_native.rs`.

#![cfg(unix)]

// Rust guideline compliant 2026-10-14

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use pohunek_test_support::env::TestEnv;

#[path = "support/setup_release.rs"]
mod setup_release;
use serde_json::Value;

/// Mode of the fixture daemon: owner and group and other may execute it.
const FIXTURE_EXECUTABLE_MODE: u32 = 0o755;

/// Directory, below the test root, that holds the copied CLI and its daemon.
const RELEASE_DIR: &str = "release";

/// File, beside the fixture daemon, that holds the report it prints.
const FIXTURE_REPORT_FILE: &str = "fixture-report.json";

/// Variable naming the D-Bus session bus, over which the CLI reads the systemd
/// user manager's environment.
const SESSION_BUS_VARIABLE: &str = "DBUS_SESSION_BUS_ADDRESS";

/// Report the fixture daemon prints for a plan it refuses.
const REFUSED_REPORT: &str = r#"{
  "report_version": 2,
  "daemon_version": "fixture",
  "dry_run": true,
  "packages": [],
  "catalog": "unavailable",
  "profiles": [],
  "integrations": [],
  "refusals": [
    {"subject": "claude", "code": "official_package_archive_invalid", "detail": "fixture refusal: archive missing"}
  ]
}"#;

/// Report with a version this CLI does not read, so the plan is unusable.
const UNREADABLE_REPORT: &str = r#"{
  "report_version": 3,
  "daemon_version": "fixture",
  "dry_run": true,
  "packages": [],
  "catalog": "unavailable",
  "profiles": [],
  "integrations": [],
  "refusals": []
}"#;

/// Report with a claude package whose selected version the release does not
/// replace.
const KEPT_REPORT: &str = r#"{
  "report_version": 2,
  "daemon_version": "fixture",
  "dry_run": true,
  "packages": [
    {"runtime": "claude", "package": "claude-code", "version": "2.0",
     "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
     "status": "kept", "reasons": ["profile"], "release_version": "1.0.3"}
  ],
  "catalog": "record",
  "profiles": [],
  "integrations": [],
  "refusals": []
}"#;

/// Relative path to every entry under `root`, with its size for files, so a
/// change of any file shows up in the comparison.
fn tree(root: &Path) -> BTreeMap<PathBuf, Option<u64>> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(listing) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in listing {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("below root").to_path_buf();
            let kind = entry.file_type().expect("file type");
            if kind.is_dir() {
                entries.insert(relative, None);
                pending.push(path);
            } else {
                let size = entry.metadata().expect("metadata").len();
                entries.insert(relative, Some(size));
            }
        }
    }
    entries
}

/// Runs `pohunek` with `arguments` from `binary`, with no terminal on stdin.
///
/// On Linux the run reaches this user's systemd user manager over the session
/// bus, as a login session does: a fresh host's plan reads the `PATH` the
/// manager gives the daemon job.
fn run_setup(env: &TestEnv, binary: &Path, arguments: &[&str]) -> Output {
    let mut command = env.command(binary);
    if let Some(address) = session_bus_address() {
        command.env(SESSION_BUS_VARIABLE, address);
    }
    command
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .expect("spawn pohunek")
}

/// The session bus address of this test process: its own variable, or the
/// bus socket of its runtime directory. `None` on macOS, which has no
/// systemd user manager.
fn session_bus_address() -> Option<std::ffi::OsString> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    std::env::var_os(SESSION_BUS_VARIABLE).or_else(|| {
        std::env::var_os("XDG_RUNTIME_DIR").map(|runtime| {
            let mut address = std::ffi::OsString::from("unix:path=");
            address.push(Path::new(&runtime).join("bus"));
            address
        })
    })
}

/// Copies the CLI under test into a release directory with a fixture daemon.
///
/// The CLI runs from that copy, so the staged release it plans against is the
/// fixture's directory and never the build tree. The daemon prints its report
/// with shell builtins only, because the test environment's `PATH` is scrubbed.
fn release_with_fixture(env: &TestEnv, report: &str) -> PathBuf {
    let release = env.root().join(RELEASE_DIR);
    fs::create_dir_all(&release).expect("create release directory");
    let binary = release.join("pohunek");
    fs::copy(pohunek_test_support::bin_exe("pohunek"), &binary).expect("copy the CLI");
    // `read` in the fixture drops a final line without its newline.
    fs::write(release.join(FIXTURE_REPORT_FILE), format!("{report}\n"))
        .expect("write fixture report");
    let daemon = release.join(pohunek_paths::DAEMON_EXECUTABLE_NAME);
    let script = format!(
        "#!/bin/sh\nwhile IFS= read -r line; do printf '%s\\n' \"$line\"; done < \"${{0%/*}}/{FIXTURE_REPORT_FILE}\"\n"
    );
    fs::write(&daemon, script).expect("write fixture daemon");
    fs::set_permissions(&daemon, fs::Permissions::from_mode(FIXTURE_EXECUTABLE_MODE))
        .expect("make fixture daemon executable");
    binary
}

/// The stdout of a run as one JSON document.
fn json_document(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout is one JSON document")
}

/// Without a terminal and without `--yes`, setup is refused before it reads
/// the plan or writes anything: the human error names the consent and the
/// hint, and the host is unchanged.
#[test]
fn bare_setup_without_a_terminal_is_refused_and_changes_nothing() {
    let env = TestEnv::new().expect("private environment");
    let binary = pohunek_test_support::bin_exe("pohunek");
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup"]);

    assert!(!output.status.success(), "a refused setup exits non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("needs your consent") && stderr.contains("nothing was changed"),
        "stderr names the refusal: {stderr}"
    );
    assert!(stderr.contains("--yes"), "stderr names the hint: {stderr}");
    assert_eq!(tree(env.root()), before, "a refused setup writes nothing");
}

/// With `--json` setup cannot ask, so the refusal is one JSON error document
/// with its stable code and no change to the host.
#[test]
fn json_setup_without_yes_is_refused_with_a_stable_code() {
    let env = TestEnv::new().expect("private environment");
    let binary = pohunek_test_support::bin_exe("pohunek");
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--json"]);

    assert!(!output.status.success(), "a refused setup exits non-zero");
    let document = json_document(&output);
    let error = &document["err"];
    assert_eq!(error["code"], "setup_consent_required");
    assert_eq!(error["class"], "configuration");
    assert!(
        error["recover"].as_str().is_some_and(
            |hint| hint.contains("--yes") && hint.contains("`pohunek setup --dry-run --json`")
        ),
        "the recovery hint names the preview command and --yes: {error}"
    );
    assert_eq!(tree(env.root()), before, "a refused setup writes nothing");
}

/// A plan the release daemon refuses stops setup before the service, the
/// packages, the hooks and the config are touched, even with `--yes`.
#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager on the session bus"
)]
fn a_refused_plan_stops_setup_before_any_write() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, REFUSED_REPORT);
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--yes"]);

    assert!(!output.status.success(), "a refused plan exits non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("setup stopped before any change")
            && stderr.contains("fixture refusal: archive missing"),
        "stderr carries the daemon's refusal: {stderr}"
    );
    assert_eq!(tree(env.root()), before, "a refused plan writes nothing");
}

/// The dry-run preview names the selected version a kept package keeps and the
/// version the release offers.
#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager on the session bus"
)]
fn the_preview_names_the_selected_version_a_kept_package_keeps() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, KEPT_REPORT);

    let output = run_setup(&env, &binary, &["setup", "--dry-run"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("keeps the selected claude-code 2.0; the release offers 1.0.3"),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A plan report of a version this CLI does not read is an unusable plan, so
/// setup stops before any write, with the reason.
#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager on the session bus"
)]
fn an_unreadable_plan_stops_setup_before_any_write() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, UNREADABLE_REPORT);
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--yes"]);

    assert!(
        !output.status.success(),
        "an unreadable plan exits non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("its report has version 3"),
        "stderr names the report version: {stderr}"
    );
    assert_eq!(
        tree(env.root()),
        before,
        "an unreadable plan writes nothing"
    );
}

/// `setup config` keeps its own behavior: it writes the config files on a
/// fresh host and keeps an existing one, with no plan or consent involved.
#[test]
fn setup_config_still_writes_only_config_files() {
    let env = TestEnv::new().expect("private environment");
    let binary = pohunek_test_support::bin_exe("pohunek");

    let output = run_setup(&env, &binary, &["setup", "config", "--json"]);
    assert!(output.status.success(), "setup config succeeds");
    let first = json_document(&output);
    let created = first["ok"]["created"].as_array().expect("created list");
    assert_eq!(created.len(), 3, "a fresh host gets the three files");

    let attach = env.config_home().join("pohunek").join("attach.conf");
    fs::write(&attach, "user-edited").expect("user edit");
    let output = run_setup(&env, &binary, &["setup", "config", "--json"]);
    assert!(output.status.success(), "a second setup config succeeds");
    let second = json_document(&output);
    assert!(second["ok"]["created"]
        .as_array()
        .expect("created list")
        .is_empty());
    assert_eq!(
        fs::read_to_string(&attach).expect("read attach.conf"),
        "user-edited",
        "an existing file is kept"
    );
}

/// Writes a pending record of `operation` below `env`'s state directory and
/// runs `setup --yes` over it; returns what setup printed to stderr after
/// checking that it failed and wrote nothing.
fn setup_over_pending(
    env: &TestEnv,
    operation: pohunek_cli::service::record::Operation,
    version: &str,
    refresh: Option<pohunek_cli::service::record::RefreshJournal>,
) -> String {
    let binary = pohunek_test_support::bin_exe("pohunek");
    let state = env.state_home().join("pohunek");
    fs::create_dir_all(&state).expect("state directory");
    // The state directory must be owner-only, as the service records require.
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("owner-only state");
    let record = pohunek_cli::service::record::Record {
        schema_version: pohunek_cli::service::record::SCHEMA_VERSION,
        operation,
        version: version.to_owned(),
        prefix: env.home().join(".local"),
        previous_version: None,
        version_dir_preexisted: false,
        config_backup: None,
        step: pohunek_cli::service::record::Step::Started,
        rolling_back: false,
        runtime_stop: false,
        runtime_backup: None,
        launchd_default_path: None,
        launchd_boot_identity: None,
        accepted_risks: None,
        refresh,
    };
    pohunek_cli::service::record::Store::new(state)
        .save(&record)
        .expect("record the pending transaction");
    let before = tree(env.root());

    let output = run_setup(env, &binary, &["setup", "--yes"]);

    assert!(
        !output.status.success(),
        "a pending transaction stops setup"
    );
    // Reading the transaction store creates its lock files. They coordinate
    // service commands and hold no configuration, so they are not a change.
    let without_locks = |tree: BTreeMap<PathBuf, Option<u64>>| {
        tree.into_iter()
            .filter(|(path, _)| !path.to_string_lossy().contains("service-install.lock"))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(
        without_locks(tree(env.root())),
        without_locks(before),
        "a pending transaction writes no configuration, package or hook"
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// An interrupted install that is still pending stops setup before any change,
/// and the stop names the command that resumes it.
#[test]
fn a_pending_install_stops_setup_before_any_change() {
    let env = TestEnv::new().expect("private environment");
    let stderr = setup_over_pending(
        &env,
        pohunek_cli::service::record::Operation::Install,
        "9.9.9-pending",
        None,
    );
    assert!(
        stderr.contains("interrupted install of version 9.9.9-pending")
            && stderr.contains("pohunek service install")
            && stderr.contains("pohunek service uninstall"),
        "the stop names the pending install and its next step: {stderr}"
    );
}

/// An interrupted search-path refresh is resolved by its own command: setup
/// names `pohunek service refresh-path`, never a subcommand that does not
/// exist or an uninstall that would remove the installation.
#[test]
fn a_pending_refresh_stops_setup_and_names_only_its_own_command() {
    let env = TestEnv::new().expect("private environment");
    let journal = pohunek_cli::service::record::RefreshJournal {
        previous: Vec::new(),
        target: Vec::new(),
        digest_before: "0".repeat(64),
        digest_after: "1".repeat(64),
        daemon_before: None,
    };
    let stderr = setup_over_pending(
        &env,
        pohunek_cli::service::record::Operation::RefreshPath,
        "9.9.9-pending",
        Some(journal),
    );
    assert!(
        stderr.contains("interrupted search-path refresh of version 9.9.9-pending")
            && stderr.contains("rerun `pohunek service refresh-path`"),
        "the stop names the refresh and its own command: {stderr}"
    );
    assert!(
        !stderr.contains("service refresh_path") && !stderr.contains("pohunek service uninstall"),
        "the stop names no nonexistent subcommand and no uninstall: {stderr}"
    );
}

/// Mode of a directory every account can write, without the sticky bit.
const SHARED_WRITABLE_MODE: u32 = 0o777;

/// File the fixture daemon of a tampered archive creates when it runs.
const RAN_MARKER: &str = "fixture-daemon-ran";

/// Turns the fixture release into an extracted daemon archive: a fixture
/// session worker beside the daemon, then the archive's verifier and a sealed
/// `MANIFEST` (see `support/setup_release.rs`).
fn seal_release(release: &Path) {
    let worker = release.join("pohunek-sessiond");
    fs::write(&worker, "#!/bin/sh\nexit 0\n").expect("write the fixture worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(FIXTURE_EXECUTABLE_MODE))
        .expect("make the fixture worker executable");
    setup_release::seal(release);
}

/// Changes the sealed fixture daemon after sealing, so that it leaves
/// [`RAN_MARKER`] beside itself if it ever runs.
fn tamper_with_daemon(release: &Path) {
    let daemon = release.join(pohunek_paths::DAEMON_EXECUTABLE_NAME);
    let mut script = fs::read_to_string(&daemon).expect("read the fixture daemon");
    script.insert_str(
        script.find('\n').expect("a shebang line") + 1,
        &format!(": > \"${{0%/*}}/{RAN_MARKER}\"\n"),
    );
    fs::write(&daemon, script).expect("tamper with the sealed daemon");
}

/// A sealed archive whose member changed after sealing is refused before its
/// daemon runs: the fixture daemon, which the change makes leave a marker,
/// never runs, the reason names the digest mismatch, and nothing is written.
#[test]
fn a_tampered_sealed_archive_stops_setup_before_its_daemon_runs() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, REFUSED_REPORT);
    let release = binary.parent().expect("release directory").to_path_buf();
    seal_release(&release);
    tamper_with_daemon(&release);
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--yes"]);

    assert!(
        !output.status.success(),
        "a tampered archive exits non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("setup stopped before any change")
            && stderr.contains("failed its verification")
            && stderr.contains("digest mismatch"),
        "stderr names the verification failure: {stderr}"
    );
    assert!(
        !release.join(RAN_MARKER).exists(),
        "the tampered daemon never runs"
    );
    assert_eq!(tree(env.root()), before, "a refused archive writes nothing");
}

/// A `pohunek` reached through a symbolic link in another directory, as a
/// link on the `PATH` would be, verifies the archive it resolves to: the
/// tampered archive is refused although the link's own directory carries no
/// manifest.
#[test]
fn a_linked_cli_verifies_the_archive_it_resolves_to() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, REFUSED_REPORT);
    let release = binary.parent().expect("release directory").to_path_buf();
    seal_release(&release);
    tamper_with_daemon(&release);
    let links = env.root().join("links");
    fs::create_dir_all(&links).expect("create the link directory");
    let link = links.join("pohunek");
    std::os::unix::fs::symlink(&binary, &link).expect("link the CLI");
    let before = tree(env.root());

    let output = run_setup(&env, &link, &["setup", "--yes"]);

    assert!(
        !output.status.success(),
        "a tampered archive exits non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed its verification") && stderr.contains("digest mismatch"),
        "the resolved archive is verified: {stderr}"
    );
    assert!(
        !release.join(RAN_MARKER).exists(),
        "the tampered daemon never runs"
    );
    assert_eq!(tree(env.root()), before, "a refused archive writes nothing");
}

/// A sealed archive whose manifest was removed is refused like a modified
/// one: the verifier it still carries marks it as an archive.
#[test]
fn a_sealed_archive_without_its_manifest_stops_setup() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, REFUSED_REPORT);
    let release = binary.parent().expect("release directory").to_path_buf();
    seal_release(&release);
    fs::remove_file(release.join("MANIFEST")).expect("remove the manifest");
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--yes"]);

    assert!(
        !output.status.success(),
        "an unsealed archive exits non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed its verification") && stderr.contains("has no MANIFEST"),
        "stderr names the missing manifest: {stderr}"
    );
    assert_eq!(tree(env.root()), before, "a refused archive writes nothing");
}

/// An archive verifier that is a symbolic link is refused before it runs,
/// whatever it points to.
#[test]
fn a_linked_archive_verifier_is_refused_before_it_runs() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, REFUSED_REPORT);
    let release = binary.parent().expect("release directory").to_path_buf();
    seal_release(&release);
    let substitute = env.root().join("substitute-verifier");
    fs::write(
        &substitute,
        format!(
            "#!/bin/sh\n: > \"{}\"\nexit 0\n",
            release.join(RAN_MARKER).display()
        ),
    )
    .expect("write the substitute verifier");
    let verifier = release.join("packaging/verify-archive");
    fs::remove_file(&verifier).expect("remove the verifier");
    std::os::unix::fs::symlink(&substitute, &verifier).expect("link the verifier");
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--yes"]);

    assert!(!output.status.success(), "a linked verifier exits non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not a regular file of this user or root"),
        "stderr names the verifier: {stderr}"
    );
    assert!(
        !release.join(RAN_MARKER).exists(),
        "the linked verifier never runs"
    );
    assert_eq!(tree(env.root()), before, "a refused archive writes nothing");
}

/// On a fresh Linux host whose systemd user manager cannot be reached, the
/// `PATH` the daemon job would get is unknown, so a dry run stops before its
/// daemon plans and writes nothing.
#[cfg(target_os = "linux")]
#[test]
fn a_fresh_host_without_a_user_manager_stops_the_dry_run() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, REFUSED_REPORT);
    let missing_bus = env.root().join("no-session-bus");
    let before = tree(env.root());

    let output = env
        .command(&binary)
        .env(
            SESSION_BUS_VARIABLE,
            format!("unix:path={}", missing_bus.display()),
        )
        .args(["setup", "--dry-run"])
        .stdin(Stdio::null())
        .output()
        .expect("spawn pohunek");

    assert!(!output.status.success(), "the dry run stops");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("setup stopped before any change")
            && stderr.contains("user service manager")
            && !stderr.contains("fixture refusal"),
        "stderr names the unreadable manager before any plan: {stderr}"
    );
    assert_eq!(
        tree(env.root()),
        before,
        "the stopped dry run writes nothing"
    );
}

/// A release below a directory another account can write is refused before
/// its verifier runs, because that account could swap the checked tree for
/// its own between the check and the run.
#[test]
fn a_release_below_a_shared_writable_directory_is_refused_before_its_verifier_runs() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, REFUSED_REPORT);
    let release = binary.parent().expect("release directory").to_path_buf();
    seal_release(&release);
    let verifier = release.join("packaging/verify-archive");
    fs::write(
        &verifier,
        format!(
            "#!/bin/sh\n: > \"{}\"\nexit 0\n",
            env.root().join(RAN_MARKER).display()
        ),
    )
    .expect("write the marking verifier");
    let shared = env.root().join("shared");
    fs::create_dir(&shared).expect("create the shared directory");
    fs::set_permissions(&shared, fs::Permissions::from_mode(SHARED_WRITABLE_MODE))
        .expect("open the shared directory to other accounts");
    let moved = shared.join(release.file_name().expect("release name"));
    fs::rename(&release, &moved).expect("move the release below the shared directory");
    let before = tree(env.root());

    let output = run_setup(&env, &moved.join("pohunek"), &["setup", "--yes"]);

    assert!(!output.status.success(), "the release is refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("a directory above the release archive")
            && stderr.contains(&shared.display().to_string()),
        "stderr names the shared directory: {stderr}"
    );
    assert!(
        !env.root().join(RAN_MARKER).exists(),
        "the verifier never runs"
    );
    assert_eq!(tree(env.root()), before, "a refused archive writes nothing");
}

/// An archive without its manifest and its verifier still carries its
/// `packaging/` directory, so it is refused instead of planned unverified.
#[test]
fn an_archive_without_manifest_and_verifier_stops_setup() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, REFUSED_REPORT);
    let release = binary.parent().expect("release directory").to_path_buf();
    seal_release(&release);
    fs::remove_file(release.join("MANIFEST")).expect("remove the manifest");
    fs::remove_file(release.join("packaging/verify-archive")).expect("remove the verifier");
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--yes"]);

    assert!(
        !output.status.success(),
        "a stripped archive exits non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("has no usable")
            && stderr.contains("packaging/verify-archive")
            && !stderr.contains("fixture refusal"),
        "the archive is refused before its daemon plans: {stderr}"
    );
    assert_eq!(tree(env.root()), before, "a refused archive writes nothing");
}

/// An intact sealed archive passes the verification, so setup goes on to run
/// its daemon's plan, which this fixture refuses.
#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager on the session bus"
)]
fn an_intact_sealed_archive_passes_verification_and_is_planned() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, REFUSED_REPORT);
    seal_release(binary.parent().expect("release directory"));
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--yes"]);

    assert!(!output.status.success(), "the fixture plan is refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("failed its verification")
            && stderr.contains("fixture refusal: archive missing"),
        "the archive verifies and its daemon plans: {stderr}"
    );
    assert_eq!(tree(env.root()), before, "a refused plan writes nothing");
}

/// On a host without a service, `--dry-run --json` prints the plan the run
/// would preview, as one JSON document, and changes nothing. The release is
/// this build's binaries, signed and sealed like an extracted archive, so the
/// dry run verifies it and its daemon plans with `--detect`.
#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager on the session bus"
)]
fn a_dry_run_on_a_fresh_host_prints_the_preview_and_changes_nothing() {
    let env = TestEnv::new().expect("private environment");
    let release = env.root().join(RELEASE_DIR);
    setup_release::stage_binaries(&release);
    setup_release::sign(&release, &pohunek_test_support::catalog::test_key());
    setup_release::seal(&release);
    let binary = release.join("pohunek");
    // A state directory without a service, as a removed installation or
    // another tool leaves it: the dry run creates no lock file in it.
    let state = env.state_home().join("pohunek");
    fs::create_dir_all(&state).expect("create the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("owner-only state");
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--dry-run", "--json"]);

    assert!(
        output.status.success(),
        "a dry run succeeds: {}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let document = json_document(&output);
    let preview = &document["ok"];
    assert_eq!(preview["service"]["action"], "install", "{preview}");
    assert!(preview["packages"].is_array(), "{preview}");
    assert!(
        preview.get("applied").is_none(),
        "a dry run applies nothing: {preview}"
    );
    assert_eq!(tree(env.root()), before, "a dry run writes nothing");
}

/// `--dry-run` never applies: with `--yes`, or with a setup subcommand, the
/// command line is refused before anything runs.
#[test]
fn a_dry_run_with_yes_or_a_subcommand_is_a_usage_error() {
    let env = TestEnv::new().expect("private environment");
    let binary = pohunek_test_support::bin_exe("pohunek");
    let before = tree(env.root());

    for arguments in [
        &["setup", "--dry-run", "--yes"][..],
        &["setup", "--dry-run", "config"][..],
    ] {
        let output = run_setup(&env, &binary, arguments);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{arguments:?} is a usage error"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--dry-run"), "{arguments:?}: {stderr}");
    }
    assert_eq!(
        tree(env.root()),
        before,
        "a refused command line writes nothing"
    );
}

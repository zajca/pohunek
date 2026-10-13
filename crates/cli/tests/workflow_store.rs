//! End-to-end contracts of `pohunek workflow`: the local workflow plugin
//! store, the strict `workflow.toml` manifest and the consent install.
//!
//! Every scenario runs the real `pohunek` binary in a hermetic environment and
//! observes the store on disk. Archives are built in the test with
//! `package::build_archive`. The last scenario adds a real daemon from the
//! plugin harness to show a workflow archive never becomes a runtime package.

// Rust guideline compliant 2026-10-13

#![cfg(unix)]

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
use pohunek_cli::service::definition::identity_config;
use pohunek_cli::service::VERSION;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

#[path = "support/plugin_harness.rs"]
mod plugin_harness;
#[path = "support/workflow_units.rs"]
mod workflow_units;

use plugin_harness::{path_str, Harness};

/// Plugin id every fixture manifest declares.
const PLUGIN_ID: &str = "acme.work";

/// Error code of every refused manifest.
const DESCRIPTOR_INVALID: &str = "workflow_descriptor_invalid";

/// A manifest value that must never appear in an error.
const SENTINEL: &str = "sentinel-argv-value-7f3a";

/// Mode the store gives every plugin state and config directory.
const PRIVATE_MODE: u32 = 0o700;

/// The cap the manifest is checked against before parsing (plan section 2.1).
const MANIFEST_CAP: usize = 64 * 1024;

fn entry(path: &str, contents: &str, executable: bool) -> ArchiveEntry {
    ArchiveEntry {
        path: path.to_owned(),
        contents: contents.as_bytes().to_vec(),
        executable,
    }
}

/// A valid manifest of `version` whose one action runs `argv0`.
fn manifest(version: &str, argv0: &str) -> String {
    manifest_of(PLUGIN_ID, version, argv0)
}

/// A valid manifest of plugin `id`.
fn manifest_of(id: &str, version: &str, argv0: &str) -> String {
    format!(
        r#"schema = 1

[plugin]
id = "{id}"
version = "{version}"
description = "Work overview"

[actions.overview]
argv = ["{argv0}", "overview", "{SENTINEL}"]
args = "passthrough"
timeout_ms = 30000
max_output_bytes = 1048576
"#
    )
}

/// The files of a valid plugin archive whose manifest is `document`.
fn plugin_files(document: &str) -> Vec<ArchiveEntry> {
    vec![
        entry("workflow.toml", document, false),
        entry("bin/work", "#!/bin/sh\nexit 0\n", true),
    ]
}

/// One hermetic environment and the `pohunek` binary run in it.
struct Fixture {
    env: TestEnv,
}

impl Fixture {
    fn new() -> Self {
        Self {
            env: TestEnv::new().expect("create the hermetic test environment"),
        }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.run_with(&[], arguments)
    }

    /// Runs `pohunek` with `overrides` on top of the hermetic environment.
    fn run_with(&self, overrides: &[(&str, &Path)], arguments: &[&str]) -> Output {
        let mut command = self.env.command(pohunek_test_support::bin_exe("pohunek"));
        for (name, value) in overrides {
            command.env(name, value);
        }
        command
            .args(arguments)
            .output()
            .expect("run the pohunek binary")
    }

    /// Runs `pohunek <arguments> --json`; returns the exit code and document.
    fn json(&self, arguments: &[&str]) -> (i32, Value) {
        let mut all = arguments.to_vec();
        all.push("--json");
        json_of(&all, &self.run(&all))
    }

    /// Writes `files` as an archive named `name` and returns it with its digest.
    fn archive(&self, name: &str, files: &[ArchiveEntry]) -> (PathBuf, PackageDigest) {
        write_archive(self.env.root(), name, files)
    }

    /// `pohunek workflow install <archive> --sha256 <digest> --yes --json`.
    fn install(&self, archive: &Path, digest: &PackageDigest, extra: &[&str]) -> (i32, Value) {
        let mut arguments = vec![
            "workflow",
            "install",
            path_str(archive),
            "--sha256",
            digest.as_str(),
        ];
        arguments.extend_from_slice(extra);
        self.json(&arguments)
    }

    fn store_root(&self) -> PathBuf {
        self.env.state_home().join("pohunek/workflow-plugins")
    }

    fn state_dir(&self) -> PathBuf {
        self.env
            .state_home()
            .join("pohunek/workflow-plugin-state")
            .join(PLUGIN_ID)
    }

    fn state_parent(&self) -> PathBuf {
        self.env.state_home().join("pohunek/workflow-plugin-state")
    }

    fn config_parent(&self) -> PathBuf {
        self.env
            .config_home()
            .join("pohunek/workflow-plugin-config")
    }

    fn config_dir(&self) -> PathBuf {
        self.config_parent().join(PLUGIN_ID)
    }

    /// Entries a purge, a rollback or the registry leaves while it removes a
    /// tree: `.purge-*` in the plugin directory parents, `.collect-*` and
    /// `.staging-*` among the package roots.
    fn quarantine_leftovers(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for (dir, prefixes) in [
            (self.state_parent(), &[".purge-"][..]),
            (self.config_parent(), &[".purge-"][..]),
            (
                self.store_root().join("packages"),
                &[".collect-", ".staging-"][..],
            ),
        ] {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries {
                let entry = entry.expect("read a directory entry");
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if prefixes.iter().any(|prefix| name.starts_with(prefix)) {
                    found.push(entry.path());
                }
            }
        }
        found
    }

    /// The package root of `digest` in the store.
    fn package_root(&self, digest: &PackageDigest) -> PathBuf {
        self.store_root().join("packages").join(hex(digest))
    }

    /// Ids listed by `pohunek workflow list --json`.
    fn listed(&self) -> Vec<Value> {
        let (code, document) = self.json(&["workflow", "list"]);
        assert_eq!(code, 0, "{document}");
        document["ok"]["plugins"]
            .as_array()
            .expect("plugins array")
            .clone()
    }

    /// Installs a manifest that breaks one rule and returns the refusal text,
    /// after checking the code and that nothing was recorded.
    fn refused_manifest(&self, case: &str, files: &[ArchiveEntry]) -> String {
        let (archive, digest) = self.archive(case, files);
        let (code, document) = self.install(&archive, &digest, &["--yes"]);
        assert_eq!(code, 1, "{case}: {document}");
        assert_eq!(
            document["err"]["code"], DESCRIPTOR_INVALID,
            "{case}: {document}"
        );
        let message = document["err"]["msg"].as_str().expect("message").to_owned();
        assert!(
            !document.to_string().contains(SENTINEL),
            "{case}: the error echoes a manifest value: {document}"
        );
        assert!(
            !self.store_root().exists(),
            "{case}: a refused manifest created the store"
        );
        assert!(
            !self.state_dir().exists(),
            "{case}: a refused manifest created the state directory"
        );
        message
    }
}

fn write_archive(dir: &Path, name: &str, files: &[ArchiveEntry]) -> (PathBuf, PackageDigest) {
    let bytes = build_archive(files, &Limits::DEFAULT).expect("the archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    let path = dir.join(format!("{name}.tar.zst"));
    fs::write(&path, bytes).expect("write the archive");
    (path, digest)
}

fn json_of(arguments: &[&str], output: &Output) -> (i32, Value) {
    let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout of {arguments:?} is one JSON document ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    (output.status.code().expect("exit code"), document)
}

fn hex(digest: &PackageDigest) -> &str {
    digest.as_str().trim_start_matches("sha256:")
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

#[test]
fn install_without_consent_shows_the_plugin_and_records_nothing() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));

    let output = fixture.run(&[
        "workflow",
        "install",
        path_str(&archive),
        "--sha256",
        digest.as_str(),
    ]);
    assert_eq!(output.status.code(), Some(1));
    let review = String::from_utf8_lossy(&output.stdout);
    let program = fixture
        .store_root()
        .join("packages")
        .join(hex(&digest))
        .join("files/bin/work");
    for needle in [
        PLUGIN_ID,
        "1.0.0",
        digest.as_str(),
        "overview",
        path_str(&program),
        SENTINEL,
    ] {
        assert!(
            review.contains(needle),
            "the review lacks {needle:?}: {review}"
        );
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("needs your consent; nothing was changed"),
        "{stderr}"
    );

    let (code, document) = fixture.install(&archive, &digest, &[]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(document["err"]["code"], "consent_required", "{document}");

    assert!(!fixture.store_root().exists(), "consent created the store");
    assert!(!fixture.state_dir().exists());
    assert!(!fixture.config_dir().exists());
    assert!(fixture.listed().is_empty());
    assert!(!fixture.store_root().exists(), "list created the store");
}

#[test]
fn a_wrong_digest_is_refused_and_records_nothing() {
    let fixture = Fixture::new();
    let (archive, _digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let wrong: PackageDigest = format!("sha256:{}", "0".repeat(64))
        .parse()
        .expect("digest");

    let (code, document) = fixture.install(&archive, &wrong, &["--yes"]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_digest_mismatch",
        "{document}"
    );
    assert!(!fixture.store_root().exists());
    assert!(!fixture.state_dir().exists());
    assert!(fixture.listed().is_empty());
}

#[test]
fn a_manifest_with_an_unknown_key_is_refused() {
    let fixture = Fixture::new();
    let document = manifest("1.0.0", "bin/work").replace(
        "description = \"Work overview\"\n",
        &format!("description = \"Work overview\"\nhomepage = \"{SENTINEL}\"\n"),
    );
    let message = fixture.refused_manifest("unknown-key", &plugin_files(&document));
    assert!(
        message.contains("does not match the manifest schema") && message.contains("line 7"),
        "unknown key: {message}"
    );
}

#[test]
fn a_bare_argv0_is_refused_even_when_the_archive_has_that_file() {
    let fixture = Fixture::new();
    let mut files = plugin_files(&manifest("1.0.0", "work"));
    files.push(entry("work", "#!/bin/sh\nexit 0\n", true));
    let message = fixture.refused_manifest("bare", &files);
    assert!(
        message.contains("action overview") && message.contains("bare command name"),
        "bare argv[0]: {message}"
    );
}

#[test]
fn a_parent_segment_in_argv0_is_refused() {
    let fixture = Fixture::new();
    let message = fixture.refused_manifest(
        "parent",
        &plugin_files(&manifest("1.0.0", "bin/../bin/work")),
    );
    assert!(
        message.contains("not a normalized path"),
        "`..` in argv[0]: {message}"
    );
}

#[test]
fn a_non_executable_argv0_is_refused() {
    let fixture = Fixture::new();
    let files = vec![
        entry("workflow.toml", &manifest("1.0.0", "bin/work"), false),
        entry("bin/work", "#!/bin/sh\nexit 0\n", false),
    ];
    let message = fixture.refused_manifest("not-executable", &files);
    assert!(
        message.contains("not executable"),
        "non-executable argv[0]: {message}"
    );
}

#[test]
fn an_argv0_missing_from_the_archive_is_refused() {
    let fixture = Fixture::new();
    let message =
        fixture.refused_manifest("missing", &plugin_files(&manifest("1.0.0", "bin/absent")));
    assert!(
        message.contains("not a file of the archive"),
        "argv[0] missing from the archive: {message}"
    );
}

#[test]
fn an_oversize_manifest_is_refused_before_parsing() {
    let fixture = Fixture::new();
    let mut document = manifest("1.0.0", "bin/work");
    // Incompressible padding: a repeated byte would trip the archive's
    // expansion-ratio limit before the manifest cap is reached.
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    while document.len() <= MANIFEST_CAP {
        document.push_str("# ");
        for _ in 0..64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            write!(document, "{:02x}", state & 0xff).expect("write to a String");
        }
        document.push('\n');
    }
    let message = fixture.refused_manifest("oversize", &plugin_files(&document));
    assert!(
        message.contains(&format!("exceeds {MANIFEST_CAP} bytes")),
        "oversize manifest: {message}"
    );
}

#[test]
fn an_archive_that_also_carries_runtime_toml_is_refused() {
    let fixture = Fixture::new();
    let mut files = plugin_files(&manifest("1.0.0", "bin/work"));
    files.push(entry("runtime.toml", "schema = 1\n", false));
    let message = fixture.refused_manifest("runtime-descriptor", &files);
    assert!(
        message.contains("runtime.toml"),
        "archive with runtime.toml: {message}"
    );
}

#[test]
fn a_reactions_table_is_refused_until_reactions_ship() {
    let fixture = Fixture::new();
    let document = format!(
        "{}\n[reactions.on_stop]\nevents = [\"session_stopped\"]\nargv = [\"bin/work\", \"{SENTINEL}\"]\n",
        manifest("1.0.0", "bin/work")
    );
    let message = fixture.refused_manifest("reactions", &plugin_files(&document));
    assert!(
        message.contains("does not match the manifest schema"),
        "[reactions] table: {message}"
    );
}

/// A manifest with one job and one service besides the action.
fn unit_manifest() -> String {
    format!(
        "{}\n[jobs.poll]\nevery_s = 300\ntimeout_ms = 120000\nargv = [\"bin/work\", \"poll\"]\n\n\
         [services.notify]\nargv = [\"bin/work\", \"notify\"]\n",
        manifest("1.0.0", "bin/work")
    )
}

#[test]
fn hostile_jobs_and_services_are_refused() {
    let fixture = Fixture::new();
    let base = manifest("1.0.0", "bin/work");
    for (case, table, needle) in [
        (
            "job without every_s",
            format!("[jobs.poll]\nargv = [\"bin/work\", \"{SENTINEL}\"]\n"),
            "does not match the manifest schema",
        ),
        (
            "job with an unknown key",
            format!(
                "[jobs.poll]\nevery_s = 60\nshell = true\nargv = [\"bin/work\", \"{SENTINEL}\"]\n"
            ),
            "does not match the manifest schema",
        ),
        (
            "job interval above a week",
            format!("[jobs.poll]\nevery_s = 604801\nargv = [\"bin/work\", \"{SENTINEL}\"]\n"),
            "job poll: every_s must be at most 604800 seconds",
        ),
        (
            "job with a zero interval",
            format!("[jobs.poll]\nevery_s = 0\nargv = [\"bin/work\", \"{SENTINEL}\"]\n"),
            "does not match the manifest schema",
        ),
        (
            "service with a bare argv[0]",
            format!("[services.notify]\nargv = [\"work\", \"{SENTINEL}\"]\n"),
            "service notify: argv[0] is a bare command name",
        ),
        (
            "service with a job key",
            format!("[services.notify]\nevery_s = 60\nargv = [\"bin/work\", \"{SENTINEL}\"]\n"),
            "does not match the manifest schema",
        ),
        (
            "service name outside the grammar",
            format!("[services.Notify]\nargv = [\"bin/work\", \"{SENTINEL}\"]\n"),
            "an action, job or service name is not",
        ),
    ] {
        let document = format!("{base}\n{table}");
        let message = fixture.refused_manifest(case, &plugin_files(&document));
        assert!(message.contains(needle), "{case}: {message}");
    }
}

#[test]
fn jobs_and_services_are_reviewed_installed_and_inspected() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("units", &plugin_files(&unit_manifest()));
    let review = fixture.run(&[
        "workflow",
        "install",
        path_str(&archive),
        "--sha256",
        digest.as_str(),
    ]);
    assert_eq!(review.status.code(), Some(1));
    let text = String::from_utf8_lossy(&review.stdout);
    let files = fixture.package_root(&digest).join("files");
    let program = path_str(&files.join("bin/work")).to_owned();
    for needle in [
        "Jobs (each runs periodically as you".to_owned(),
        "  poll  (every 300 s)".to_owned(),
        format!("argv: [\"{program}\", \"poll\"]"),
        "requested timeout: 120000 ms".to_owned(),
        "Services (each runs continuously as you".to_owned(),
        format!("argv: [\"{program}\", \"notify\"]"),
    ] {
        assert!(text.contains(&needle), "{needle:?} in the review:\n{text}");
    }
    assert!(
        !fixture.store_root().exists(),
        "the review installed nothing"
    );

    let (code, document) = fixture.install(&archive, &digest, &["--yes", "--no-enable"]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["plugin"]["enabled"], false, "{document}");
    let (code, document) = fixture.json(&["workflow", "inspect", PLUGIN_ID]);
    assert_eq!(code, 0, "{document}");
    let inspected = &document["ok"];
    assert_eq!(inspected["jobs"][0]["name"], "poll", "{document}");
    assert_eq!(inspected["jobs"][0]["every_s"], 300, "{document}");
    assert_eq!(inspected["jobs"][0]["timeout_ms"], 120_000, "{document}");
    assert_eq!(
        inspected["jobs"][0]["argv"][0],
        program.as_str(),
        "{document}"
    );
    assert_eq!(inspected["services"][0]["name"], "notify", "{document}");
    assert_eq!(inspected["services"][0]["argv"][1], "notify", "{document}");
}

#[test]
fn jobs_and_services_need_the_service_installation() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("units", &plugin_files(&unit_manifest()));

    // An enabling install is refused before any effect.
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_service_not_installed",
        "{document}"
    );
    assert!(
        !fixture.store_root().exists(),
        "the refusal created the store"
    );

    let (code, document) = fixture.install(&archive, &digest, &["--yes", "--no-enable"]);
    assert_eq!(code, 0, "{document}");
    let (code, document) = fixture.json(&["workflow", "enable", PLUGIN_ID]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_service_not_installed",
        "{document}"
    );
    assert_eq!(
        fixture.listed()[0]["enabled"],
        false,
        "enable changed nothing"
    );

    // Without a service installation, leftover units are looked up in the
    // derived namespace; a service manager that cannot be reached is a
    // warning, never a silent skip. On Linux the hermetic environment has no
    // session bus, so the warning is certain. On macOS a logged-in runner's
    // `gui/<uid>` domain answers, and the lookup boots out labels of the
    // namespace of this test's private roots, which no job ever had, so
    // nothing of the host changes; a host without that domain (SSH,
    // headless) gets the warning instead. Both outcomes are accepted, and
    // any warning must be the unreachable one.
    for verb in ["disable", "uninstall"] {
        let (code, document) = fixture.json(&["workflow", verb, PLUGIN_ID]);
        assert_eq!(code, 0, "{verb}: {document}");
        if let Some(warnings) = document["ok"]["warnings"].as_array() {
            for warning in warnings {
                let warning = warning.as_str().expect("a warning");
                assert!(
                    warning.contains("could not be reached"),
                    "{verb}: {warning}"
                );
            }
        }
    }
    assert!(fixture.listed().is_empty());
}

/// An enabling install refuses with `workflow_store_busy` before it records
/// anything while another unit lifecycle command holds `units.lock`.
#[test]
fn an_enabling_install_waits_for_no_unit_lifecycle_lock() {
    use std::os::unix::fs::OpenOptionsExt as _;

    let units = workflow_units::UnitEnv::new(|env| env.root().join("units"));
    // Any installed plugin creates the store and its lock files' directory.
    units.install(&workflow_units::Plugin::new("acme.other"));
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(
            units
                .env
                .state_home()
                .join("pohunek/workflow-plugins/units.lock"),
        )
        .expect("open the unit lifecycle lock");
    lock.lock().expect("hold the unit lifecycle lock");

    let plugin = workflow_units::Plugin::new(PLUGIN_ID).service("serve", "hang");
    let digest = units.archive(&plugin);
    let output = units.install_with(&plugin, &digest, &[]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        workflow_units::describe(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("a JSON envelope");
    assert_eq!(document["err"]["code"], "workflow_store_busy", "{document}");
    let listed: Vec<Value> = units.listed();
    assert_eq!(
        listed.len(),
        1,
        "only the other plugin is recorded: {listed:?}"
    );
    assert_eq!(listed[0]["id"], "acme.other", "{listed:?}");

    // An enabling install of a recorded plugin with units decides under the
    // same lock whether the plugin is still recorded, so an uninstall can
    // never slip in between; while another command holds the lock it is
    // refused too.
    let recorded = workflow_units::Plugin::new("acme.recorded").service("serve", "hang");
    drop(lock);
    let recorded_digest = units.install(&recorded);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            units
                .env
                .state_home()
                .join("pohunek/workflow-plugins/units.lock"),
        )
        .expect("open the unit lifecycle lock");
    lock.lock().expect("hold the unit lifecycle lock");
    let output = units.install_with(&recorded, &recorded_digest, &[]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        workflow_units::describe(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("a JSON envelope");
    assert_eq!(document["err"]["code"], "workflow_store_busy", "{document}");
}

/// An enabling install whose units cannot be installed keeps the plugin
/// installed disabled and says how to enable it; installing again changes
/// nothing. The hermetic environment has no session bus, so the systemd user
/// manager cannot be reached.
#[cfg(target_os = "linux")]
#[test]
fn an_install_whose_enable_fails_stays_installed_disabled() {
    let units = workflow_units::UnitEnv::new(|env| env.root().join("units"));
    let plugin = workflow_units::Plugin::new(PLUGIN_ID).service("serve", "hang");
    let digest = units.archive(&plugin);

    let output = units.install_with(&plugin, &digest, &[]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        workflow_units::describe(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("a JSON envelope");
    assert_eq!(
        document["err"]["code"], "workflow_installed_disabled",
        "{document}"
    );
    assert_eq!(
        document["err"]["cause"], "workflow_units_failed",
        "{document}"
    );
    let message = document["err"]["msg"].as_str().expect("a message");
    assert!(message.contains("stays installed disabled"), "{message}");
    let recover = document["err"]["recover"]
        .as_str()
        .expect("a recovery hint");
    // What to fix for the cause, then the one step to repeat; never a hint
    // to repeat the install, which would not enable the plugin.
    assert!(
        recover.starts_with("check the user service manager"),
        "{recover}"
    );
    assert!(
        recover.contains("then run `pohunek workflow enable"),
        "{recover}"
    );
    assert!(
        !recover.contains("repeat") && !recover.contains("retry"),
        "{recover}"
    );
    assert_eq!(units.listed()[0]["enabled"], false);

    let output = units.install_with(&plugin, &digest, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        workflow_units::describe(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("a JSON envelope");
    assert_eq!(document["ok"]["status"], "already_installed", "{document}");
    assert_eq!(document["ok"]["plugin"]["enabled"], false, "{document}");
}

/// Services in the record-size scenario. Three-letter names give a record
/// entry of 32 bytes (name, fixed fields and comma) for a manifest entry of
/// 24 bytes, so 2100 services put the record past 64 KiB while the manifest
/// stays well under its own 64 KiB limit.
const RECORD_SIZE_SERVICES: usize = 2100;

/// The `index`-th service name of [`RECORD_SIZE_SERVICES`]: three lowercase
/// letters, unique for every index below 26^3.
fn record_size_service(index: usize) -> String {
    let letter = |digit: usize| char::from(b'a' + u8::try_from(digit % 26).expect("below 26"));
    format!(
        "{}{}{}",
        letter(index / (26 * 26)),
        letter(index / 26),
        letter(index)
    )
}

/// A unit record that lists as many units as a large manifest declares is
/// read, and recorded units whose removal cannot be confirmed fail the
/// disable: no service manager is reachable, so the units may still run, and
/// the record is kept. The hermetic environment has no session bus, and the
/// installation's `service.toml` is removed so the namespace this process
/// derives is the one looked up.
#[test]
fn a_unit_record_of_a_large_manifest_is_read() {
    let names: Vec<String> = (0..RECORD_SIZE_SERVICES).map(record_size_service).collect();
    let units = workflow_units::UnitEnv::new(|env| env.root().join("units"));
    let plugin = workflow_units::Plugin::new(PLUGIN_ID).compact_services(&names);
    units.install(&plugin);
    fs::remove_file(units.context.config_path()).expect("remove the installation record");

    let units_dir = units
        .env
        .state_home()
        .join("pohunek/workflow-plugins/units");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&units_dir)
        .expect("create the unit record directory");
    let entries: Vec<Value> = names
        .iter()
        .map(|name| serde_json::json!({ "kind": "service", "name": name }))
        .collect();
    let record = serde_json::to_vec(&serde_json::json!({ "units": entries }))
        .expect("serialize the unit record");
    assert!(
        record.len() > 64 * 1024,
        "the record is {} bytes",
        record.len()
    );
    let record_path = units_dir.join(format!("{PLUGIN_ID}.json"));
    fs::write(&record_path, &record).expect("write the unit record");
    fs::set_permissions(&record_path, fs::Permissions::from_mode(0o600))
        .expect("chmod the unit record");

    let output = units
        .command(&["workflow", "disable", PLUGIN_ID, "--json"])
        .output()
        .expect("run pohunek workflow disable");
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        workflow_units::describe(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("a JSON envelope");
    assert_eq!(
        document["err"]["code"], "workflow_units_failed",
        "{document}"
    );
    let message = document["err"]["msg"].as_str().expect("a message");
    assert!(
        message.contains("the recorded units were not removed and stay recorded"),
        "{document}"
    );
    assert_eq!(
        units.listed()[0]["enabled"],
        false,
        "the plugin is disabled"
    );
    assert!(
        record_path.exists(),
        "the unit record is kept while its units cannot be reached"
    );
}

/// Another version than this CLI, recorded for the installation.
const OTHER_VERSION: &str = "0.0.1";

/// Restart delay a refused service is paced with: longer than the scenario
/// runs, so the stop request always arrives during the pacing. The owner
/// knob only sets the wait, so a larger value changes nothing else.
const REFUSAL_PACING_MS: u64 = 60_000;

/// Enabling a plugin with units refuses an installation recorded for another
/// version of pohunek, before any unit is written: that version's trampoline
/// may not exist, so the units would never run.
#[test]
fn enabling_refuses_an_installation_of_another_version() {
    assert_ne!(VERSION, OTHER_VERSION);
    let units = workflow_units::UnitEnv::new(|env| env.root().join("units"));
    let plugin = workflow_units::Plugin::new(PLUGIN_ID).service("serve", "hang");
    units.install(&plugin);
    let prefix = units.env.root().join("prefix");
    identity_config(&units.context, &prefix, OTHER_VERSION)
        .expect("the installation configuration")
        .write(&units.context.config_path())
        .expect("record the other version");

    let output = units
        .command(&["workflow", "enable", PLUGIN_ID, "--json"])
        .output()
        .expect("run pohunek workflow enable");
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        workflow_units::describe(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("a JSON envelope");
    assert_eq!(
        document["err"]["code"], "workflow_service_not_installed",
        "{document}"
    );
    let message = document["err"]["msg"].as_str().expect("a message");
    assert!(
        message.contains(OTHER_VERSION) && message.contains(VERSION),
        "{message}"
    );
    assert_eq!(units.listed()[0]["enabled"], false);
    assert!(
        units
            .context
            .supervisor_dir()
            .read_dir()
            .map_or(true, |mut entries| entries.next().is_none()),
        "no unit was written"
    );
}

/// A stop request that arrives while a refused service is paced before its
/// next start ends the trampoline as stopped: it exits 0, as for a run the
/// service manager stops, so the unit is not recorded as failed. The refusal
/// is recorded first, so the request reaches the pacing.
#[test]
fn a_stop_during_a_refused_services_pacing_exits_cleanly() {
    let units = workflow_units::UnitEnv::new(|env| env.root().join("units"));
    let paced = format!("restart_max_ms = {REFUSAL_PACING_MS}");
    let knobs = workflow_units::KNOBS.replace("restart_max_ms = 2000", &paced);
    assert!(knobs.contains(&paced), "the knob was set");
    units.knobs(&knobs);
    // Installed disabled, so the trampoline refuses the service and paces it.
    let plugin = workflow_units::Plugin::new(PLUGIN_ID).service("serve", "hang");
    let digest = units.install(&plugin);

    let trampoline = units
        .exec(PLUGIN_ID, digest.as_str(), "service", "serve")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the trampoline");
    poll_until("the refusal to be recorded", || {
        units
            .records(PLUGIN_ID)
            .iter()
            .any(|record| record["outcome"] == "refused")
            .then_some(())
    });
    let pid = Pid::from_raw(i32::try_from(trampoline.id()).expect("a PID"));
    kill(pid, Signal::SIGTERM).expect("signal the trampoline");
    let output = trampoline
        .wait_with_output()
        .expect("wait for the trampoline");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        workflow_units::describe(&output)
    );
}

#[test]
fn a_job_outside_the_owner_bounds_is_refused_before_any_effect() {
    // A recorded service installation, so only the job bounds refuse; the
    // refusal comes before any service manager is contacted.
    let units = workflow_units::UnitEnv::new(|env| env.root().join("units"));
    units.knobs("[jobs]\nmin_interval_s = 60\n");
    // Where `pohunek workflow enable` itself puts units: the systemd user
    // unit directory of its config home, or its home's `LaunchAgents`.
    let unit_dirs = [
        units.env.config_home().join("systemd/user"),
        units.env.home().join("Library/LaunchAgents"),
    ];
    let plugin = workflow_units::Plugin::new(PLUGIN_ID).job("poll", "tick", 30);
    let digest = units.install(&plugin);
    let refused = |output: &Output| {
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            workflow_units::describe(output)
        );
        let document: Value = serde_json::from_slice(&output.stdout).expect("a JSON envelope");
        assert_eq!(
            document["err"]["code"], "workflow_unit_refused",
            "{document}"
        );
        let message = document["err"]["msg"].as_str().expect("a message");
        assert!(
            message.contains("job poll") && message.contains("below jobs.min_interval_s 60"),
            "{message}"
        );
    };
    refused(
        &units
            .command(&["workflow", "enable", PLUGIN_ID, "--json"])
            .output()
            .expect("run pohunek workflow enable"),
    );
    assert_eq!(
        units.listed()[0]["enabled"],
        false,
        "the refusal changed nothing"
    );
    for dir in &unit_dirs {
        assert!(!dir.exists(), "{} was created", dir.display());
    }

    // An enabling install of the same archive in a fresh store is refused
    // before the store exists.
    let fresh = workflow_units::UnitEnv::new(|env| env.root().join("units"));
    fresh.knobs("[jobs]\nmin_interval_s = 60\n");
    let archive = units.env.root().join(format!("{PLUGIN_ID}.tar.zst"));
    refused(
        &fresh
            .command(&[
                "workflow",
                "install",
                path_str(&archive),
                "--sha256",
                digest.as_str(),
                "--yes",
                "--json",
            ])
            .output()
            .expect("run pohunek workflow install"),
    );
    assert!(fresh.listed().is_empty(), "the refusal installed nothing");
}

#[test]
fn a_remote_host_is_refused_before_any_filesystem_effect() {
    let fixture = Fixture::new();
    // A store root no workflow command would accept: reading it would fail
    // with another code than the local-only refusal.
    fs::create_dir_all(fixture.store_root()).expect("plant the store root");
    fs::set_permissions(fixture.store_root(), fs::Permissions::from_mode(0o755))
        .expect("open the store root mode");
    let missing = fixture.env.root().join("absent.tar.zst");
    let zero = format!("sha256:{}", "0".repeat(64));
    for arguments in [
        vec![
            "--host",
            "remote-peer",
            "workflow",
            "install",
            path_str(&missing),
            "--sha256",
            &zero,
            "--yes",
        ],
        vec!["--host", "remote-peer", "workflow", "list"],
        vec![
            "--host",
            "remote-peer",
            "workflow",
            "uninstall",
            PLUGIN_ID,
            "--purge",
        ],
    ] {
        let (code, document) = fixture.json(&arguments);
        assert_eq!(code, 1, "{arguments:?}: {document}");
        assert_eq!(
            document["err"]["code"], "workflow_local_only",
            "{arguments:?}: {document}"
        );
    }
    assert_eq!(mode(&fixture.store_root()), 0o755, "the store root changed");
    assert_eq!(
        fs::read_dir(fixture.store_root())
            .expect("list the store root")
            .count(),
        0,
        "a refused command wrote into the store root"
    );
    assert!(!fixture.state_parent().exists());
    assert!(!fixture.env.config_home().join("pohunek").exists());
}

#[test]
fn install_list_inspect_disable_enable_and_reinstall() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));

    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["status"], "installed", "{document}");
    assert_eq!(document["ok"]["plugin"]["enabled"], true, "{document}");
    for dir in [
        fixture.state_dir(),
        fixture.config_dir(),
        fixture.store_root(),
    ] {
        assert_eq!(mode(&dir), PRIVATE_MODE, "{}", dir.display());
    }
    let listed = fixture.listed();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["id"], PLUGIN_ID);
    assert_eq!(listed[0]["digest"], digest.as_str());

    let (code, inspected) = fixture.json(&["workflow", "inspect", PLUGIN_ID]);
    assert_eq!(code, 0, "{inspected}");
    let files = fixture
        .store_root()
        .join("packages")
        .join(hex(&digest))
        .join("files");
    let action = &inspected["ok"]["actions"][0];
    assert_eq!(action["name"], "overview", "{inspected}");
    assert_eq!(
        action["argv"],
        serde_json::json!([path_str(&files.join("bin/work")), "overview", SENTINEL]),
        "{inspected}"
    );
    assert_eq!(action["args"], "passthrough", "{inspected}");
    assert_eq!(action["timeout_ms"], 30_000, "{inspected}");
    assert_eq!(action["max_output_bytes"], 1_048_576, "{inspected}");
    assert_eq!(inspected["ok"]["state_dir"], path_str(&fixture.state_dir()));
    assert_eq!(
        inspected["ok"]["config_dir"],
        path_str(&fixture.config_dir())
    );
    assert!(files.join("bin/work").is_file());

    let (code, document) = fixture.json(&["workflow", "disable", PLUGIN_ID]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["id"], PLUGIN_ID, "{document}");
    assert_eq!(document["ok"]["digest"], digest.as_str(), "{document}");
    assert_eq!(document["ok"]["enabled"], false, "{document}");
    assert_eq!(fixture.listed()[0]["enabled"], false);
    let (code, document) = fixture.json(&["workflow", "enable", PLUGIN_ID]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["id"], PLUGIN_ID, "{document}");
    assert_eq!(document["ok"]["enabled"], true, "{document}");
    assert_eq!(fixture.listed()[0]["enabled"], true);

    // The same archive again changes nothing; another archive of the id is refused.
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["status"], "already_installed", "{document}");
    let (other, other_digest) =
        fixture.archive("work-1.1", &plugin_files(&manifest("1.1.0", "bin/work")));
    let (code, document) = fixture.install(&other, &other_digest, &["--yes"]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_plugin_installed",
        "{document}"
    );
}

#[test]
fn uninstall_keeps_the_plugin_dirs_and_purge_removes_them() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{document}");
    let marker = fixture.state_dir().join("cache.json");
    fs::write(&marker, "{}").expect("write plugin state");
    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["plugin"]["id"], PLUGIN_ID, "{document}");
    assert_eq!(document["ok"]["purged"], false, "{document}");
    assert_eq!(
        document["ok"]["state_dir"],
        path_str(&fixture.state_dir()),
        "{document}"
    );
    assert_eq!(
        document["ok"]["config_dir"],
        path_str(&fixture.config_dir()),
        "{document}"
    );
    assert!(fixture.listed().is_empty());
    assert!(
        !fixture.package_root(&digest).exists(),
        "uninstall left the package root"
    );
    assert!(
        fixture.quarantine_leftovers().is_empty(),
        "{:?}",
        fixture.quarantine_leftovers()
    );
    assert!(marker.is_file(), "uninstall removed the plugin state");
    assert!(
        fixture.config_dir().is_dir(),
        "uninstall removed the plugin config"
    );
    let (code, document) = fixture.json(&["workflow", "inspect", PLUGIN_ID]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_plugin_unknown",
        "{document}"
    );

    let (code, document) = fixture.install(&archive, &digest, &["--yes", "--no-enable"]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["plugin"]["enabled"], false, "{document}");
    assert!(marker.is_file(), "a reinstall keeps the plugin state");
    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--purge"]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["purged"], true, "{document}");
    assert_eq!(
        document["ok"]["purge"]["state_dir"], "removed",
        "{document}"
    );
    assert_eq!(
        document["ok"]["purge"]["config_dir"], "removed",
        "{document}"
    );
    assert!(!fixture.package_root(&digest).exists());
    assert!(
        fixture.quarantine_leftovers().is_empty(),
        "{:?}",
        fixture.quarantine_leftovers()
    );
    assert!(
        !fixture.state_dir().exists(),
        "--purge kept the state directory"
    );
    assert!(
        !fixture.config_dir().exists(),
        "--purge kept the config directory"
    );
    assert!(fixture.listed().is_empty());
}

#[test]
fn changed_plugin_files_are_refused_and_left_as_found() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{document}");
    let program = fixture
        .store_root()
        .join("packages")
        .join(hex(&digest))
        .join("files/bin/work");
    let tampered = "#!/bin/sh\necho tampered\n";
    fs::write(&program, tampered).expect("tamper with the installed program");

    for command in ["inspect", "enable", "uninstall"] {
        let (code, document) = fixture.json(&["workflow", command, PLUGIN_ID]);
        assert_eq!(code, 1, "{command}: {document}");
        assert_eq!(
            document["err"]["code"], "workflow_root_invalid",
            "{command}: {document}"
        );
    }
    assert_eq!(
        fs::read_to_string(&program).expect("read the installed program"),
        tampered,
        "a refused command changed the tampered file"
    );
    assert_eq!(fixture.listed().len(), 1, "the plugin stays recorded");
    // Disabling never reads the installed files, so it still succeeds.
    let (code, document) = fixture.json(&["workflow", "disable", PLUGIN_ID]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(fixture.listed()[0]["enabled"], false);
}

#[test]
fn a_file_that_is_not_a_package_archive_is_refused() {
    let fixture = Fixture::new();
    let bytes = b"not a package archive";
    let path = fixture.env.root().join("plain.tar.zst");
    fs::write(&path, bytes).expect("write the file");
    let digest = format!("sha256:{:x}", Sha256::digest(bytes));
    let (code, document) = fixture.json(&[
        "workflow",
        "install",
        path_str(&path),
        "--sha256",
        &digest,
        "--yes",
    ]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_archive_invalid",
        "{document}"
    );
    assert!(!fixture.store_root().exists());
}

#[test]
fn a_plugin_directory_that_is_not_owner_private_refuses_the_install() {
    // The config directory of the plugin is open: the state directory this
    // install created first is removed again.
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.config_dir()).expect("create the config directory");
    fs::set_permissions(
        fixture.config_parent(),
        fs::Permissions::from_mode(PRIVATE_MODE),
    )
    .expect("private config parent");
    fs::set_permissions(fixture.config_dir(), fs::Permissions::from_mode(0o755))
        .expect("open the config directory");
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 1, "open plugin dir: {document}");
    assert_eq!(
        document["err"]["code"], "workflow_store_failed",
        "open plugin dir: {document}"
    );
    assert!(
        document["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains("owner-only permissions")),
        "open plugin dir: {document}"
    );
    assert!(
        !fixture.state_dir().exists(),
        "open plugin dir: the created state directory was left behind"
    );
    assert!(
        !fixture.store_root().join("registry.json").exists(),
        "open plugin dir: a refused install recorded the plugin"
    );
    assert!(fixture.listed().is_empty(), "open plugin dir");
    assert_eq!(
        mode(&fixture.config_dir()),
        0o755,
        "open plugin dir: mode changed"
    );

    // The config parent is open: the install fails before either plugin
    // directory exists, so a later purge can always open both parents.
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.config_parent()).expect("create the config parent");
    fs::set_permissions(fixture.config_parent(), fs::Permissions::from_mode(0o755))
        .expect("open the config parent");
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 1, "open parent: {document}");
    assert_eq!(
        document["err"]["code"], "workflow_store_failed",
        "open parent: {document}"
    );
    assert!(
        !fixture.state_dir().exists(),
        "open parent: state directory created"
    );
    assert!(
        !fixture.config_dir().exists(),
        "open parent: config directory created"
    );
    assert!(fixture.listed().is_empty(), "open parent");
}

#[test]
fn an_absolute_argv0_without_a_file_name_is_refused() {
    let fixture = Fixture::new();
    let message = fixture.refused_manifest("root-path", &plugin_files(&manifest("1.0.0", "/")));
    assert!(
        message.contains("without a file name"),
        "argv[0] `/`: {message}"
    );
}

#[test]
fn purge_after_a_plain_uninstall_removes_the_kept_directories() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{document}");
    fs::write(fixture.state_dir().join("cache.json"), "{}").expect("write plugin state");
    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID]);
    assert_eq!(code, 0, "{document}");
    assert!(fixture.state_dir().is_dir() && fixture.config_dir().is_dir());

    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--purge"]);
    assert_eq!(code, 0, "{document}");
    assert!(document["ok"]["plugin"].is_null(), "{document}");
    assert_eq!(document["ok"]["purged"], true, "{document}");
    assert_eq!(
        document["ok"]["purge"]["state_dir"], "removed",
        "{document}"
    );
    assert_eq!(
        document["ok"]["purge"]["config_dir"], "removed",
        "{document}"
    );
    assert!(!fixture.state_dir().exists());
    assert!(!fixture.config_dir().exists());

    // Purging again finds nothing and still succeeds.
    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--purge"]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["purge"]["state_dir"], "absent", "{document}");
    // A plain uninstall of an id without a record is still refused.
    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_plugin_unknown",
        "{document}"
    );
}

#[test]
fn a_purge_that_cannot_finish_names_where_the_data_is_and_tries_both_dirs() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{document}");
    let locked = fixture.state_dir().join("sub");
    fs::create_dir(&locked).expect("create a subdirectory");
    fs::write(locked.join("data.bin"), "kept").expect("write plugin data");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("read-only subdir");

    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--purge"]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_purge_incomplete",
        "{document}"
    );
    let message = document["err"]["msg"].as_str().expect("message");
    assert!(message.contains("the plugin was uninstalled"), "{message}");
    assert!(
        message.contains("no config directory") || message.contains("config directory removed"),
        "{message}"
    );
    assert!(fixture.listed().is_empty(), "the record must be gone");
    assert!(
        !fixture.config_dir().exists(),
        "the config directory was not attempted"
    );
    let leftovers = fixture.quarantine_leftovers();
    let [staged] = leftovers.as_slice() else {
        panic!("one purge leftover: {leftovers:?}");
    };
    assert!(staged.starts_with(fixture.state_parent()), "{staged:?}");
    assert!(
        message.contains(path_str(staged)),
        "the error hides the data: {message}"
    );
    assert_eq!(
        fs::read_to_string(staged.join("sub/data.bin")).expect("data stays readable"),
        "kept"
    );

    // Once the cause is fixed, purging again removes the leftover.
    fs::set_permissions(staged.join("sub"), fs::Permissions::from_mode(PRIVATE_MODE))
        .expect("restore the subdirectory mode");
    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--purge"]);
    assert_eq!(code, 0, "{document}");
    assert!(
        fixture.quarantine_leftovers().is_empty(),
        "{:?}",
        fixture.quarantine_leftovers()
    );
}

#[test]
fn repurging_without_fixing_the_cause_still_fails_and_names_the_data() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{document}");
    let locked = fixture.state_dir().join("sub");
    fs::create_dir(&locked).expect("create a subdirectory");
    fs::write(locked.join("data.bin"), "kept").expect("write plugin data");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("read-only subdir");
    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--purge"]);
    assert_eq!(code, 1, "first purge: {document}");

    // The plugin's own data is still on disk, so the purge is not done.
    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--purge"]);
    assert_eq!(code, 1, "a repeated purge reported success: {document}");
    assert_eq!(
        document["err"]["code"], "workflow_purge_incomplete",
        "{document}"
    );
    let leftovers = fixture.quarantine_leftovers();
    let [staged] = leftovers.as_slice() else {
        panic!("one purge leftover: {leftovers:?}");
    };
    let message = document["err"]["msg"].as_str().expect("message");
    assert!(
        message.contains(path_str(staged)),
        "the error hides the data: {message}"
    );
    assert_eq!(
        fs::read_to_string(staged.join("sub/data.bin")).expect("data stays readable"),
        "kept"
    );

    fs::set_permissions(staged.join("sub"), fs::Permissions::from_mode(PRIVATE_MODE))
        .expect("restore the subdirectory mode");
    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--purge"]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(
        document["ok"]["purge"]["state_dir"], "removed",
        "{document}"
    );
    assert!(
        fixture.quarantine_leftovers().is_empty(),
        "{:?}",
        fixture.quarantine_leftovers()
    );
}

#[test]
fn remove_modified_removes_only_changed_plugin_files() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{document}");

    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--remove-modified"]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_root_intact",
        "{document}"
    );
    assert_eq!(fixture.listed().len(), 1, "intact files were removed");

    fs::write(
        fixture.package_root(&digest).join("files/bin/work"),
        "#!/bin/sh\necho tampered\n",
    )
    .expect("tamper with the installed program");
    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--remove-modified"]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["plugin"]["id"], PLUGIN_ID, "{document}");
    assert!(fixture.listed().is_empty());
    assert!(!fixture.package_root(&digest).exists());
    assert!(fixture.state_dir().is_dir(), "the state directory is kept");
}

#[test]
fn equal_state_and_config_roots_keep_plugin_files_apart() {
    let fixture = Fixture::new();
    let shared = fixture.env.state_home().to_path_buf();
    let overrides = [("XDG_CONFIG_HOME", shared.as_path())];
    let json = |arguments: &[&str]| {
        let mut all = arguments.to_vec();
        all.push("--json");
        json_of(&all, &fixture.run_with(&overrides, &all))
    };
    let install = |name: &str, document: &str| {
        let (archive, digest) = fixture.archive(name, &plugin_files(document));
        let (code, result) = json(&[
            "workflow",
            "install",
            path_str(&archive),
            "--sha256",
            digest.as_str(),
            "--yes",
        ]);
        assert_eq!(code, 0, "{name}: {result}");
    };
    install("work", &manifest("1.0.0", "bin/work"));
    // `packages` is a valid plugin id and the name of the store's root tree.
    install("packages", &manifest_of("packages", "1.0.0", "bin/work"));

    let (code, document) = json(&["workflow", "uninstall", "packages", "--purge"]);
    assert_eq!(code, 0, "{document}");
    let (code, document) = json(&["workflow", "inspect", PLUGIN_ID]);
    assert_eq!(
        code, 0,
        "purging `packages` reached another plugin: {document}"
    );
    assert_eq!(
        document["ok"]["config_dir"],
        path_str(
            &shared
                .join("pohunek/workflow-plugin-config")
                .join(PLUGIN_ID)
        ),
        "{document}"
    );
}

#[test]
fn a_held_store_lock_refuses_every_change() {
    use std::os::unix::fs::OpenOptionsExt as _;

    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{document}");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(fixture.store_root().join("store.lock"))
        .expect("open the store lock");
    lock.lock().expect("hold the store lock");

    let (other, other_digest) = fixture.archive(
        "other",
        &plugin_files(&manifest_of("acme.other", "1.0.0", "bin/work")),
    );
    let other_args = [
        "workflow",
        "install",
        path_str(&other),
        "--sha256",
        other_digest.as_str(),
        "--yes",
    ];
    for arguments in [
        &["workflow", "disable", PLUGIN_ID][..],
        &["workflow", "uninstall", PLUGIN_ID, "--purge"][..],
        &other_args[..],
    ] {
        let (code, document) = fixture.json(arguments);
        assert_eq!(code, 1, "{arguments:?}: {document}");
        assert_eq!(
            document["err"]["code"], "workflow_store_busy",
            "{arguments:?}: {document}"
        );
    }
    let listed = fixture.listed();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(
        listed[0]["enabled"], true,
        "a busy disable changed the plugin"
    );
    assert!(
        fixture.state_dir().is_dir(),
        "a busy purge removed the state"
    );
    assert!(
        !fixture.state_parent().join("acme.other").exists(),
        "a busy install created a plugin directory"
    );

    drop(lock);
    let (code, document) = fixture.json(&["workflow", "disable", PLUGIN_ID]);
    assert_eq!(code, 0, "{document}");
}

#[test]
fn the_review_escapes_format_characters_and_names_missing_files() {
    let fixture = Fixture::new();
    let document = manifest("1.0.0", "bin/work").replace(
        "description = \"Work overview\"",
        "description = \"Work \\u202Eweivrevo\\u200B\"",
    );
    let (archive, digest) = fixture.archive("work", &plugin_files(&document));
    let review = |fixture: &Fixture| {
        let output = fixture.run(&[
            "workflow",
            "install",
            path_str(&archive),
            "--sha256",
            digest.as_str(),
        ]);
        assert_eq!(output.status.code(), Some(1));
        String::from_utf8(output.stdout).expect("utf-8 review")
    };
    let text = review(&fixture);
    assert!(
        text.contains("\\u{202e}") && text.contains("\\u{200b}"),
        "{text}"
    );
    assert!(
        !text.contains('\u{202e}') && !text.contains('\u{200b}'),
        "{text}"
    );

    let (code, result) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{result}");
    fs::remove_dir_all(fixture.package_root(&digest)).expect("remove the installed files");
    let text = review(&fixture);
    assert!(text.contains("its files are missing"), "{text}");
    let (code, result) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["ok"]["status"], "root_restored", "{result}");
}

#[test]
fn a_stuck_purge_leftover_never_decides_another_plugins_purge() {
    let fixture = Fixture::new();
    let (first, first_digest) =
        fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (second, second_digest) = fixture.archive(
        "other",
        &plugin_files(&manifest_of("acme.other", "1.0.0", "bin/work")),
    );
    for (archive, digest) in [(&first, &first_digest), (&second, &second_digest)] {
        let (code, document) = fixture.install(archive, digest, &["--yes"]);
        assert_eq!(code, 0, "{document}");
    }
    let locked = fixture.state_dir().join("sub");
    fs::create_dir(&locked).expect("create a subdirectory");
    fs::write(locked.join("data.bin"), "kept").expect("write plugin data");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("read-only subdir");
    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--purge"]);
    assert_eq!(code, 1, "first plugin: {document}");
    assert_eq!(
        fixture.quarantine_leftovers().len(),
        1,
        "one leftover of the first purge"
    );

    let (code, document) = fixture.json(&["workflow", "uninstall", "acme.other", "--purge"]);
    assert_eq!(code, 0, "the leftover decided the second purge: {document}");
    assert_eq!(
        document["ok"]["purge"]["state_dir"], "removed",
        "{document}"
    );
    assert_eq!(
        document["ok"]["purge"]["config_dir"], "removed",
        "{document}"
    );
    // The retried leftover moves to a fresh purge name; the warning names where
    // it is now.
    let leftovers = fixture.quarantine_leftovers();
    let [stuck] = leftovers.as_slice() else {
        panic!("one leftover after the second purge: {leftovers:?}");
    };
    assert!(stuck.starts_with(fixture.state_parent()), "{stuck:?}");
    let warnings = document["ok"]["warnings"].to_string();
    assert!(
        warnings.contains(path_str(stuck)),
        "the leftover is not named: {document}"
    );
    assert!(!fixture.state_parent().join("acme.other").exists());
    assert!(!fixture.config_parent().join("acme.other").exists());

    // With the cause fixed, the next purge removes the leftover silently.
    fs::set_permissions(stuck.join("sub"), fs::Permissions::from_mode(PRIVATE_MODE))
        .expect("restore the subdirectory mode");
    let (code, document) = fixture.json(&["workflow", "uninstall", "acme.other", "--purge"]);
    assert_eq!(code, 0, "{document}");
    assert!(document["ok"].get("warnings").is_none(), "{document}");
    assert!(
        fixture.quarantine_leftovers().is_empty(),
        "{:?}",
        fixture.quarantine_leftovers()
    );
}

#[test]
fn missing_plugin_files_point_to_reinstall_and_a_plain_uninstall() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{document}");
    fs::remove_dir_all(fixture.package_root(&digest)).expect("remove the installed files");

    let (code, document) = fixture.json(&["workflow", "inspect", PLUGIN_ID]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_root_invalid",
        "{document}"
    );
    let hint = document["err"]["recover"].as_str().expect("recover hint");
    assert!(
        hint.contains("install the same archive again") && !hint.contains("--remove-modified"),
        "{document}"
    );

    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--remove-modified"]);
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_root_intact",
        "{document}"
    );
    assert_eq!(document["err"]["class"], "runtime", "{document}");
    assert!(
        document["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains("verify or are missing")),
        "{document}"
    );

    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID]);
    assert_eq!(code, 0, "{document}");
    assert!(fixture.listed().is_empty());
}

#[test]
fn files_that_cannot_be_deleted_leave_a_warning_and_no_record() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let (code, document) = fixture.install(&archive, &digest, &["--yes"]);
    assert_eq!(code, 0, "{document}");
    // A read-only directory both fails verification (wrong mode) and keeps
    // the registry from deleting the file inside it.
    let bin = fixture.package_root(&digest).join("files/bin");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o500)).expect("read-only bin");

    let (code, document) = fixture.json(&["workflow", "uninstall", PLUGIN_ID, "--remove-modified"]);
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["plugin"]["id"], PLUGIN_ID, "{document}");
    let warnings: Vec<&str> = document["ok"]["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .map(|warning| warning.as_str().expect("a warning"))
        .collect();
    let packages = fixture.store_root().join("packages");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains(".collect-") && warning.contains(path_str(&packages))),
        "{document}"
    );
    // Files that fail verification hide which units the plugin had.
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("could not be listed")),
        "{document}"
    );
    assert!(fixture.listed().is_empty(), "the record must be gone");

    // Restore the mode so the residue can be inspected and the root removed.
    let leftovers = fixture.quarantine_leftovers();
    let [residue] = leftovers.as_slice() else {
        panic!("one residue: {leftovers:?}");
    };
    assert!(residue.starts_with(&packages), "{residue:?}");
    fs::set_permissions(
        residue.join("files/bin"),
        fs::Permissions::from_mode(PRIVATE_MODE),
    )
    .expect("restore the residue mode");
}

#[test]
fn a_config_root_inside_the_store_is_refused_before_any_effect() {
    let fixture = Fixture::new();
    let (archive, digest) = fixture.archive("work", &plugin_files(&manifest("1.0.0", "bin/work")));
    let inside = fixture.store_root();
    let arguments = [
        "workflow",
        "install",
        path_str(&archive),
        "--sha256",
        digest.as_str(),
        "--yes",
        "--json",
    ];
    let (code, document) = json_of(
        &arguments,
        &fixture.run_with(&[("XDG_CONFIG_HOME", inside.as_path())], &arguments),
    );
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "workflow_dirs_overlap",
        "{document}"
    );
    assert!(!fixture.store_root().exists(), "the store was created");
    assert!(
        !fixture.state_parent().exists(),
        "a state directory was created"
    );
}

#[tokio::test]
async fn the_daemon_refuses_a_workflow_archive_as_a_runtime_package() {
    let harness = Harness::start().await;
    let (archive, digest) = write_archive(
        harness.env.root(),
        "work",
        &plugin_files(&manifest("1.0.0", "bin/work")),
    );

    let (code, document) = harness
        .json(&[
            "workflow",
            "install",
            path_str(&archive),
            "--sha256",
            digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["status"], "installed", "{document}");

    let (code, document) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&archive),
            "--sha256",
            digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "package_descriptor_invalid",
        "{document}"
    );
    assert!(harness.registry().packages().is_empty());

    let (code, document) = harness.json(&["workflow", "list"]).await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["plugins"][0]["digest"], digest.as_str());

    harness.stop().await;
}

//! End-to-end contracts of `pohunek plugin profile create` against a real daemon.
//!
//! The `pohunek` binary runs as a subprocess against a `ControlServer` served
//! from this test process over a real `SessionRegistry` and a real plugin root,
//! so the file the daemon composes, validates and publishes is the file the
//! assertions read back. The fixture runtime declares its own config home
//! variable, so the `[env]` key can only come from the package descriptor.

// Rust guideline compliant 2026-11-11

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use package::{build_archive, read_archive, ArchiveEntry, Limits};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, stderr_text, stdout_text, Built, Harness};

/// Package id of the fixture archive.
const PACKAGE_ID: &str = "acme.runtime.homed";

/// Runtime id the fixture package serves.
const RUNTIME: &str = "homed";

/// The config home variable the fixture descriptor declares.
const HOME_ENV: &str = "HOMED_CONFIG_DIR";

/// Mode of a published profile file.
const FILE_MODE: u32 = 0o600;

/// Mode of an agents directory the daemon creates.
const DIR_MODE: u32 = 0o700;

const DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

fn runtime_document() -> String {
    format!(
        r#"schema = 1
id = "{PACKAGE_ID}"
version = "1.0.0"
runtime_api = 1

[runtime]
id = "{RUNTIME}"
name = "Homed"
program = "/bin/sh"
args = []
detect_manifest = "detect.toml"
prompt_arg = true

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["--session", "{{reference}}"]

[native_reference]
strategy = "assigned"
launch_args = ["--session-id", "{{reference}}"]
assign_fork = true

[native_reference.existence]
check = "none"

[config_home]
env = "{HOME_ENV}"
default = ".homed"
"#
    )
}

impl Harness {
    /// Writes the archive of the fixture package that declares a config home.
    fn homed_archive(&self) -> Built {
        let entries = [
            ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: runtime_document().into_bytes(),
                executable: false,
            },
            ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: DETECT_MANIFEST.as_bytes().to_vec(),
                executable: false,
            },
        ];
        let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the archive reads")
            .digest()
            .clone();
        let path = self.env.root().join("homed-1.0.0.tar.zst");
        fs::write(&path, bytes).expect("write the archive");
        Built { path, digest }
    }

    /// An absolute directory path below the hermetic root.
    fn home(&self, name: &str) -> String {
        path_str(&self.env.root().join(name)).to_owned()
    }
}

async fn install(harness: &Harness) -> Built {
    let built = harness.homed_archive();
    let (code, document) = harness
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
    built
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

fn sha256_of(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("list the directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn listed(document: &Value, name: &str) -> Value {
    document["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("{name} is listed in {document}"))
        .clone()
}

#[tokio::test]
async fn create_writes_a_pinned_private_profile_with_the_descriptor_home_variable() {
    let harness = Harness::start().await;
    let built = install(&harness).await;
    let home = harness.home("work-home");

    let (code, created) = harness
        .json(&[
            "plugin",
            "profile",
            "create",
            "work",
            "--base",
            RUNTIME,
            "--home",
            &home,
            "--arg",
            "--model=fast",
            "--secret-env",
            "WORK_API_KEY",
        ])
        .await;
    assert_eq!(code, 0, "{created}");
    assert_eq!(created["ok"]["status"], "created");
    assert_eq!(created["ok"]["home_env"], HOME_ENV);

    let path = harness.agents.join("work.toml");
    let text = fs::read_to_string(&path).expect("read the profile");
    assert_eq!(text, created["ok"]["text"].as_str().expect("text"));
    assert!(text.contains(&format!("{HOME_ENV} = \"{home}\"")), "{text}");
    assert!(text.contains("secret_env = [\"WORK_API_KEY\"]"), "{text}");
    assert_eq!(mode_of(&path), FILE_MODE);

    let (code, list) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{list}");
    let entry = listed(&list, "work");
    assert_eq!(entry["state"], "pinned", "{entry}");
    assert_eq!(entry["package"], PACKAGE_ID);
    assert_eq!(entry["digest"], built.digest.as_str());
    assert_eq!(entry["file_digest"], sha256_of(text.as_bytes()));
    assert_eq!(created["ok"]["file_digest"], entry["file_digest"]);

    harness.stop().await;
}

#[tokio::test]
async fn create_makes_a_missing_agents_directory_private() {
    let harness = Harness::start().await;
    install(&harness).await;
    fs::remove_dir(&harness.agents).expect("remove the empty agents directory");

    let (code, created) = harness
        .json(&["plugin", "profile", "create", "fresh", "--base", RUNTIME])
        .await;
    assert_eq!(code, 0, "{created}");
    assert_eq!(mode_of(&harness.agents), DIR_MODE);
    assert_eq!(mode_of(&harness.agents.join("fresh.toml")), FILE_MODE);

    harness.stop().await;
}

#[tokio::test]
async fn a_credential_literal_is_refused_and_nothing_is_written() {
    let harness = Harness::start().await;
    install(&harness).await;
    let token_home = format!("/{}/sk-ant-api03-{}", "srv", "x".repeat(40));
    let cases: [Vec<&str>; 8] = [
        vec!["--home", &token_home],
        vec!["--arg", "--api-key=sk-ant-0123456789abcdef0123456789"],
        vec!["--arg", "--token", "--arg", "abc"],
        vec!["--arg", "Bearer abcdefghijklmnop"],
        vec!["--arg", "--header=Bearer abcdefghijklmnop"],
        vec!["--arg", "Authorization: Bearer abcdefghijklmnop"],
        vec!["--secret-env", "API_KEY=sk-ant-0123456789abcdef0123456789"],
        vec!["--secret-env", "AKIAABCDEFGHIJKLMNOP"],
    ];
    for extra in cases {
        let mut arguments = vec!["plugin", "profile", "create", "leaky", "--base", RUNTIME];
        arguments.extend(extra.iter().copied());
        let home = harness.home("leaky-home");
        if !arguments.contains(&"--home") {
            arguments.extend(["--home", &home]);
        }
        let output = harness.run(&arguments).await;
        let shown = format!("{}{}", stdout_text(&output), stderr_text(&output));
        assert!(!output.status.success(), "{arguments:?}: {shown}");
        let key_without_value = arguments.contains(&"API_KEY=sk-ant-0123456789abcdef0123456789");
        assert!(
            shown.contains("package_profile_credential_literal")
                || (key_without_value && shown.contains("package_profile_unusable")),
            "{arguments:?}: {shown}"
        );
        assert!(!shown.contains("sk-ant"), "{shown}");
        assert!(!shown.contains("abcdefghijklmnop"), "{shown}");
        assert!(!harness.agents.join("leaky.toml").exists());
    }
    assert_eq!(entries(&harness.agents), Vec::<String>::new());

    harness.stop().await;
}

#[tokio::test]
async fn a_home_that_is_not_an_absolute_plain_path_is_refused() {
    let harness = Harness::start().await;
    install(&harness).await;
    for home in ["~/x", "rel/x", "/a/../b", "/a/./b", "/a//b", "/a/~b"] {
        let (code, refused) = harness
            .json(&[
                "plugin", "profile", "create", "homey", "--base", RUNTIME, "--home", home,
            ])
            .await;
        assert_eq!(code, 1, "{home}: {refused}");
        assert_eq!(
            refused["err"]["code"], "package_profile_home_invalid",
            "{home}"
        );
    }
    assert_eq!(entries(&harness.agents), Vec::<String>::new());

    harness.stop().await;
}

#[tokio::test]
async fn a_base_without_a_config_home_takes_no_home() {
    let harness = Harness::start().await;
    let (code, refused) = harness
        .json(&[
            "plugin", "profile", "create", "sh", "--base", "shell", "--home", "/srv/h",
        ])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_profile_home_unsupported");

    let (code, created) = harness
        .json(&["plugin", "profile", "create", "sh", "--base", "shell"])
        .await;
    assert_eq!(code, 0, "{created}");
    assert!(created["ok"]["package"].is_null(), "{created}");

    harness.stop().await;
}

#[tokio::test]
async fn a_symlinked_agents_directory_is_refused() {
    let harness = Harness::start().await;
    install(&harness).await;
    let real = harness.env.root().join("real-agents");
    fs::create_dir(&real).expect("create the target");
    fs::set_permissions(&real, fs::Permissions::from_mode(DIR_MODE)).expect("target mode");
    fs::remove_dir(&harness.agents).expect("remove the agents directory");
    std::os::unix::fs::symlink(&real, &harness.agents).expect("link the agents directory");

    let (code, refused) = harness
        .json(&["plugin", "profile", "create", "linked", "--base", RUNTIME])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_profile_dir_unsafe");
    assert_eq!(entries(&real), Vec::<String>::new());

    harness.stop().await;
}

#[tokio::test]
async fn an_existing_name_is_replaced_only_with_its_current_digest() {
    let harness = Harness::start().await;
    install(&harness).await;
    let first = harness.home("one");
    let second = harness.home("two");
    let (code, created) = harness
        .json(&[
            "plugin", "profile", "create", "work", "--base", RUNTIME, "--home", &first,
        ])
        .await;
    assert_eq!(code, 0, "{created}");
    let path = harness.agents.join("work.toml");
    let original = fs::read(&path).expect("read");
    let old_digest = created["ok"]["file_digest"]
        .as_str()
        .expect("digest")
        .to_owned();

    let (code, exists) = harness
        .json(&[
            "plugin", "profile", "create", "work", "--base", RUNTIME, "--home", &second,
        ])
        .await;
    assert_eq!(code, 1, "{exists}");
    assert_eq!(exists["err"]["code"], "package_profile_exists");
    assert_eq!(fs::read(&path).expect("read"), original);

    let wrong = format!("sha256:{}", "0".repeat(64));
    let (code, mismatch) = harness
        .json(&[
            "plugin",
            "profile",
            "create",
            "work",
            "--base",
            RUNTIME,
            "--home",
            &second,
            "--replace",
            &wrong,
        ])
        .await;
    assert_eq!(code, 1, "{mismatch}");
    assert_eq!(mismatch["err"]["code"], "package_profile_digest_mismatch");
    assert_eq!(fs::read(&path).expect("read"), original);

    let inode = fs::metadata(&path).expect("metadata").ino();
    let (code, list) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{list}");
    let current = listed(&list, "work")["file_digest"]
        .as_str()
        .expect("file digest")
        .to_owned();
    assert_eq!(current, old_digest);
    let (code, replaced) = harness
        .json(&[
            "plugin",
            "profile",
            "create",
            "work",
            "--base",
            RUNTIME,
            "--home",
            &second,
            "--replace",
            &current,
        ])
        .await;
    assert_eq!(code, 0, "{replaced}");
    assert_eq!(replaced["ok"]["status"], "replaced");
    assert_eq!(replaced["ok"]["previous"], old_digest.as_str());
    let text = fs::read_to_string(&path).expect("read");
    assert!(text.contains(&second) && !text.contains(&first), "{text}");
    assert_ne!(fs::metadata(&path).expect("metadata").ino(), inode);
    assert_eq!(mode_of(&path), FILE_MODE);
    assert_eq!(entries(&harness.agents), vec!["work.toml".to_owned()]);

    harness.stop().await;
}

#[tokio::test]
async fn replace_of_a_missing_profile_is_refused() {
    let harness = Harness::start().await;
    install(&harness).await;
    let digest = format!("sha256:{}", "1".repeat(64));
    let (code, refused) = harness
        .json(&[
            "plugin",
            "profile",
            "create",
            "ghost",
            "--base",
            RUNTIME,
            "--replace",
            &digest,
        ])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_profile_not_found");

    harness.stop().await;
}

#[tokio::test]
async fn a_dry_run_reports_the_profile_and_writes_nothing() {
    let harness = Harness::start().await;
    install(&harness).await;
    let home = harness.home("dry-home");
    let before = entries(&harness.agents);

    let (code, preview) = harness
        .json(&[
            "plugin",
            "profile",
            "create",
            "dry",
            "--base",
            RUNTIME,
            "--home",
            &home,
            "--dry-run",
        ])
        .await;
    assert_eq!(code, 0, "{preview}");
    assert_eq!(preview["ok"]["status"], "preview");
    let text = preview["ok"]["text"].as_str().expect("text");
    assert!(text.contains(&home), "{text}");
    assert_eq!(preview["ok"]["file_digest"], sha256_of(text.as_bytes()));
    assert_eq!(entries(&harness.agents), before);

    // The same request without --dry-run writes exactly the previewed bytes.
    let (code, created) = harness
        .json(&[
            "plugin", "profile", "create", "dry", "--base", RUNTIME, "--home", &home,
        ])
        .await;
    assert_eq!(code, 0, "{created}");
    assert_eq!(
        fs::read_to_string(harness.agents.join("dry.toml")).expect("read"),
        text
    );

    harness.stop().await;
}

#[tokio::test]
async fn a_dry_run_does_not_create_a_missing_agents_directory() {
    let harness = Harness::start().await;
    install(&harness).await;
    fs::remove_dir(&harness.agents).expect("remove the empty agents directory");

    let (code, preview) = harness
        .json(&[
            "plugin",
            "profile",
            "create",
            "dry",
            "--base",
            RUNTIME,
            "--dry-run",
        ])
        .await;
    assert_eq!(code, 0, "{preview}");
    assert!(!harness.agents.exists());

    harness.stop().await;
}

#[tokio::test]
async fn a_created_profile_pins_its_package_against_uninstall() {
    let harness = Harness::start().await;
    install(&harness).await;
    let (code, created) = harness
        .json(&["plugin", "profile", "create", "held", "--base", RUNTIME])
        .await;
    assert_eq!(code, 0, "{created}");

    let (code, refused) = harness
        .json(&["plugin", "uninstall", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_referenced");

    harness.stop().await;
}

#[tokio::test]
async fn a_remote_host_is_refused_before_any_connection() {
    let harness = Harness::start().await;
    let output = harness
        .run(&[
            "--host", "other", "plugin", "profile", "create", "x", "--base", RUNTIME,
        ])
        .await;
    assert!(!output.status.success());
    assert!(
        stderr_text(&output).contains("--host")
            || stdout_text(&output).contains("plugin_local_only"),
        "{}{}",
        stdout_text(&output),
        stderr_text(&output)
    );
    assert_eq!(entries(&harness.agents), Vec::<String>::new());

    harness.stop().await;
}

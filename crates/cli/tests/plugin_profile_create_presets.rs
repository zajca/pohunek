//! `pohunek plugin profile create` with `--options-file` and `--preset-of`
//! (`#2149` rows 16-20) against a real daemon.
//!
//! The `pohunek` binary runs against the harness's in-process daemon, so the
//! file the daemon composes, validates and publishes is the file the
//! assertions read back, and a session started from it is a real launch. The
//! profiles extend the built-in `shell`; every choice runs `-c 'exec sleep
//! ...'`, which any POSIX login shell accepts.

// Rust guideline compliant 2026-11-14

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use serde_json::Value;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, stderr_text, stdout_text, Harness};

/// Mode of a published profile file.
const FILE_MODE: u32 = 0o600;

/// A marker carried by a hostile value; no output may contain it.
const MARKER: &str = "MarkerCreateQ4z";

/// Options with one option `mode` (default `calm`) whose choices keep the
/// session alive under any POSIX shell.
const MODE_OPTIONS: &str = r#"[options.mode]
label = "Mode"
default = "calm"
choices.calm = { label = "Calm", args = ["-c", "exec sleep 30"] }
choices.bold = { label = "Bold", args = ["-c", "exec sleep 31"] }
"#;

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

/// Writes `text` as an options file in the harness root and returns its path.
fn options_file(harness: &Harness, name: &str, text: &str) -> String {
    let path = harness.env.root().join(name);
    fs::write(&path, text).expect("write the options file");
    path_str(&path).to_owned()
}

/// The `plugin profile list --all --json` entry of `name`.
async fn listed(harness: &Harness, name: &str) -> Value {
    let (code, document) = harness.json(&["plugin", "profile", "list", "--all"]).await;
    assert_eq!(code, 0, "{document}");
    document["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("{name} is listed: {document}"))
        .clone()
}

/// `session new --agent <agent> [--opt ...]` in the harness working
/// directory; returns the exit code and the JSON document.
async fn session_new(harness: &Harness, agent: &str, opts: &[&str]) -> (i32, Value) {
    let cwd = harness.env.cwd().to_path_buf();
    let mut arguments = vec!["session", "new", "--agent", agent, "--cwd", path_str(&cwd)];
    for opt in opts {
        arguments.extend(["--opt", opt]);
    }
    harness.json(&arguments).await
}

/// Row 16: `--options-file` adds the file's `[options]` tables to a new
/// profile: the daemon writes it at 0600, the listing shows it, and a
/// session selecting a choice launches with it.
#[tokio::test]
async fn create_writes_the_options_of_an_options_file() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let file = options_file(&harness, "options.toml", MODE_OPTIONS);
    let (code, created) = harness
        .json(&[
            "plugin",
            "profile",
            "create",
            "moody",
            "--base",
            "shell",
            "--options-file",
            &file,
        ])
        .await;
    assert_eq!(code, 0, "{created}");
    assert_eq!(created["ok"]["status"], "created");
    let path = harness.agents.join("moody.toml");
    assert_eq!(mode_of(&path), FILE_MODE);
    let written: toml::Table =
        toml::from_str(&fs::read_to_string(&path).expect("read the profile")).expect("TOML");
    assert_eq!(written["base"].as_str(), Some("shell"));
    assert_eq!(written["options"]["mode"]["default"].as_str(), Some("calm"));
    assert_eq!(listed(&harness, "moody").await["state"], "builtin");

    let (code, session) = session_new(&harness, "moody", &["mode=bold"]).await;
    assert_eq!(code, 0, "{session}");
    assert_eq!(session["ok"]["agent"], "moody", "{session}");
    assert_eq!(session["ok"]["agent_options"]["mode"], "bold", "{session}");
    harness.stop().await;
}

/// Row 17: an options file whose choice env names a secret slot, whose
/// choice env value is a credential literal, or that holds a key other than
/// `options`, is refused with `package_profile_unusable`; the marker is
/// never printed and nothing is written.
#[tokio::test]
async fn create_refuses_an_options_file_the_load_rules_refuse() {
    let harness = Harness::start().await;
    let token = format!("sk-ant-{MARKER}0123456789abcdef");
    let cases = [
        (
            "slot",
            "[options.mode]\nlabel = \"Mode\"\ndefault = \"a\"\n\
             choices.a = { label = \"A\", env = { WORK_SLOT = \"plain\" } }\n"
                .to_owned(),
            vec!["--secret-env", "WORK_SLOT"],
        ),
        (
            "token",
            format!(
                "[options.mode]\nlabel = \"Mode\"\ndefault = \"a\"\n\
                 choices.a = {{ label = \"A\", env = {{ PH_MODE = \"{token}\" }} }}\n"
            ),
            Vec::new(),
        ),
        (
            "extra",
            format!("args = [\"{MARKER}\"]\n{MODE_OPTIONS}"),
            Vec::new(),
        ),
    ];
    for (case, text, extra) in cases {
        let file = options_file(&harness, &format!("{case}.toml"), &text);
        let mut arguments = vec![
            "plugin",
            "profile",
            "create",
            "refused",
            "--base",
            "shell",
            "--options-file",
            &file,
            "--json",
        ];
        arguments.extend(extra);
        let output = harness.run(&arguments).await;
        let shown = format!("{}{}", stdout_text(&output), stderr_text(&output));
        assert_eq!(output.status.code(), Some(1), "{case}: {shown}");
        let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
        assert_eq!(
            document["err"]["code"], "package_profile_unusable",
            "{case}: {shown}"
        );
        assert!(!shown.contains(MARKER), "{case}: {shown}");
        assert!(
            !harness.agents.join("refused.toml").exists(),
            "{case}: nothing is written"
        );
    }
    harness.stop().await;
}

/// An options path that is a FIFO or a symbolic link is refused with
/// `profile_options_file_invalid` before it is opened: the FIFO has no
/// writer, so opening it for reading would block. The command runs under
/// the shared hang guard and is killed if the guard fires.
#[tokio::test]
async fn create_refuses_an_options_path_that_is_not_a_regular_file() {
    let harness = Harness::start().await;
    let fifo = harness.env.root().join("options.fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600))
        .expect("create the options FIFO");
    let target = options_file(&harness, "target.toml", MODE_OPTIONS);
    let link = harness.env.root().join("options-link.toml");
    std::os::unix::fs::symlink(&target, &link).expect("link the options file");
    for (case, path) in [("fifo", &fifo), ("symlink", &link)] {
        let mut command = harness
            .env
            .tokio_command(pohunek_test_support::bin_exe("pohunek"));
        command
            .args([
                "plugin",
                "profile",
                "create",
                "refused",
                "--base",
                "shell",
                "--options-file",
                path_str(path),
                "--json",
            ])
            .kill_on_drop(true);
        let output = pohunek_test_support::wait::guard(
            "plugin profile create with a non-regular options path",
            command.output(),
        )
        .await
        .expect("run the pohunek binary");
        let shown = format!("{}{}", stdout_text(&output), stderr_text(&output));
        assert_eq!(output.status.code(), Some(1), "{case}: {shown}");
        let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
        assert_eq!(
            document["err"]["code"], "profile_options_file_invalid",
            "{case}: {shown}"
        );
        assert!(
            document["err"]["msg"]
                .as_str()
                .is_some_and(|msg| msg.contains("the path is not a regular file")),
            "{case}: {shown}"
        );
        assert!(
            !harness.agents.join("refused.toml").exists(),
            "{case}: nothing is written"
        );
    }
    harness.stop().await;
}

/// The `claude` parent of the preset rows: a shell profile with a `model`
/// option.
fn write_parent(harness: &Harness) {
    harness.profile(
        "claude",
        "base = \"shell\"\n\
         [options.model]\nlabel = \"Model\"\ndefault = \"opus\"\n\
         choices.opus = { label = \"Opus\", args = [\"-c\", \"exec sleep 30\"] }\n\
         choices.sonnet = { label = \"Sonnet\", args = [\"-c\", \"exec sleep 31\"] }\n",
    );
}

/// Row 18: `--preset-of` writes a locked preset at 0600 that selects its
/// choice; the listing shows it as a preset of its parent, and a session of
/// it launches the parent with that choice.
#[tokio::test]
async fn create_writes_a_preset_that_lists_and_launches() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    write_parent(&harness);
    let (code, created) = harness
        .json(&[
            "plugin",
            "profile",
            "create",
            "sonnet",
            "--preset-of",
            "claude",
            "--opt",
            "model=sonnet",
            "--locked",
        ])
        .await;
    assert_eq!(code, 0, "{created}");
    assert_eq!(created["ok"]["status"], "created");
    assert_eq!(created["ok"]["base"], "shell", "the parent's base");
    let path = harness.agents.join("sonnet.toml");
    assert_eq!(mode_of(&path), FILE_MODE);
    let written: toml::Table =
        toml::from_str(&fs::read_to_string(&path).expect("read the preset")).expect("TOML");
    assert_eq!(written["preset"]["parent"].as_str(), Some("claude"));
    assert_eq!(written["preset"]["locked"].as_bool(), Some(true));
    assert_eq!(
        written["preset"]["options"]["model"].as_str(),
        Some("sonnet")
    );

    let entry = listed(&harness, "sonnet").await;
    assert_eq!(entry["state"], "preset", "{entry}");
    assert_eq!(entry["preset_of"], "claude", "{entry}");
    assert!(entry.get("base").is_none(), "{entry}");
    let table = stdout_text(&harness.run(&["plugin", "profile", "list"]).await);
    let row = table
        .lines()
        .find(|line| line.starts_with("sonnet "))
        .unwrap_or_else(|| panic!("the preset row: {table}"));
    assert!(row.contains("preset (preset of claude)"), "{row}");

    let (code, session) = session_new(&harness, "sonnet", &[]).await;
    assert_eq!(code, 0, "{session}");
    assert_eq!(session["ok"]["agent"], "sonnet", "{session}");
    assert_eq!(
        session["ok"]["agent_options"]["model"], "sonnet",
        "{session}"
    );
    harness.stop().await;
}

/// A preset the daemon would not launch is refused before anything is
/// written: a missing parent, a parent that is itself a preset, a choice
/// the parent does not declare, and a preset named after its parent.
#[tokio::test]
async fn create_refuses_a_preset_the_preset_rules_refuse() {
    let harness = Harness::start().await;
    write_parent(&harness);
    harness.profile("nested", "[preset]\nparent = \"claude\"\n");
    let original = fs::read(harness.agents.join("claude.toml")).expect("read the parent");
    for (name, extra) in [
        ("orphan", &["--preset-of", "absent"][..]),
        ("deeper", &["--preset-of", "nested"][..]),
        (
            "wild",
            &["--preset-of", "claude", "--opt", "model=wild"][..],
        ),
        (
            "claude",
            &[
                "--preset-of",
                "claude",
                "--replace",
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            ][..],
        ),
    ] {
        let mut arguments = vec!["plugin", "profile", "create", name];
        arguments.extend(extra);
        let (code, refused) = harness.json(&arguments).await;
        assert_eq!(code, 1, "{name}: {refused}");
        assert_eq!(
            refused["err"]["code"], "package_profile_unusable",
            "{name}: {refused}"
        );
    }
    for name in ["orphan", "deeper", "wild"] {
        assert!(
            !harness.agents.join(format!("{name}.toml")).exists(),
            "{name}"
        );
    }
    assert_eq!(
        fs::read(harness.agents.join("claude.toml")).expect("read the parent"),
        original,
        "the parent is unchanged"
    );
    harness.stop().await;
}

/// Row 19: `--preset-of` takes no launch input of its own; combining it
/// with one is a usage error before anything is sent.
#[tokio::test]
async fn preset_of_conflicts_with_launch_inputs() {
    let harness = Harness::start().await;
    for extra in [
        &["--arg", "-v"][..],
        &["--base", "shell"][..],
        &["--secret-env", "WORK_KEY"][..],
    ] {
        let mut arguments = vec!["plugin", "profile", "create", "p", "--preset-of", "claude"];
        arguments.extend(extra);
        let output = harness.run(&arguments).await;
        let stderr = stderr_text(&output);
        assert_eq!(output.status.code(), Some(2), "{extra:?}: {stderr}");
        assert!(
            stderr.contains("cannot be used with"),
            "{extra:?}: {stderr}"
        );
        assert!(!harness.agents.join("p.toml").exists());
    }
    harness.stop().await;
}

/// Row 20: a preset follows the existence rules of a profile create: a
/// dry run writes nothing, an existing name needs `--replace` with its
/// current file digest, and a wrong digest leaves the file as it was.
#[tokio::test]
async fn a_preset_is_replaced_only_with_its_current_digest() {
    let harness = Harness::start().await;
    write_parent(&harness);
    let create = |extra: &'static [&'static str]| {
        let mut arguments = vec![
            "plugin",
            "profile",
            "create",
            "pick",
            "--preset-of",
            "claude",
        ];
        arguments.extend(extra);
        arguments
    };
    let (code, preview) = harness.json(&create(&["--dry-run"])).await;
    assert_eq!(code, 0, "{preview}");
    assert_eq!(preview["ok"]["status"], "preview");
    let path = harness.agents.join("pick.toml");
    assert!(!path.exists(), "a dry run writes nothing");

    let (code, created) = harness.json(&create(&["--opt", "model=opus"])).await;
    assert_eq!(code, 0, "{created}");
    let original = fs::read(&path).expect("read");
    let first = created["ok"]["file_digest"]
        .as_str()
        .expect("digest")
        .to_owned();

    let (code, exists) = harness.json(&create(&["--opt", "model=sonnet"])).await;
    assert_eq!(code, 1, "{exists}");
    assert_eq!(exists["err"]["code"], "package_profile_exists");
    assert_eq!(fs::read(&path).expect("read"), original);

    let wrong = format!("sha256:{}", "0".repeat(64));
    let mut arguments = create(&["--opt", "model=sonnet", "--replace"]);
    arguments.push(&wrong);
    let (code, mismatch) = harness.json(&arguments).await;
    assert_eq!(code, 1, "{mismatch}");
    assert_eq!(mismatch["err"]["code"], "package_profile_digest_mismatch");
    assert_eq!(fs::read(&path).expect("read"), original);

    let current = listed(&harness, "pick").await["file_digest"]
        .as_str()
        .expect("file digest")
        .to_owned();
    assert_eq!(current, first);
    let mut arguments = create(&["--opt", "model=sonnet", "--dry-run", "--replace"]);
    arguments.push(&current);
    let (code, previewed) = harness.json(&arguments).await;
    assert_eq!(code, 0, "{previewed}");
    assert_eq!(previewed["ok"]["status"], "preview");
    assert_eq!(
        fs::read(&path).expect("read"),
        original,
        "a dry run writes nothing"
    );

    let mut arguments = create(&["--opt", "model=sonnet", "--replace"]);
    arguments.push(&current);
    let (code, replaced) = harness.json(&arguments).await;
    assert_eq!(code, 0, "{replaced}");
    assert_eq!(replaced["ok"]["status"], "replaced");
    assert_eq!(replaced["ok"]["previous"], first.as_str());
    let written: toml::Table =
        toml::from_str(&fs::read_to_string(&path).expect("read")).expect("TOML");
    assert_eq!(
        written["preset"]["options"]["model"].as_str(),
        Some("sonnet")
    );
    harness.stop().await;
}

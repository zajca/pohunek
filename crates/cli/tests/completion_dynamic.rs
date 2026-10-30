//! End-to-end: dynamic shell completion without a daemon. Lookups are silent,
//! and values the command tree defines still complete.
//!
//! These drive the built `pohunek` binary through clap's environment
//! completion protocol, the way the installed `source <(POHUNEK_COMPLETE=bash
//! pohunek)` bootstrap does on Tab. The child gets a scrubbed environment with
//! private HOME and XDG directories, so no daemon socket exists for it.
//!
//! The lookup deadline is covered on virtual time by the in-process daemon
//! scenarios; this file covers what only a process shows: the candidates the
//! installed command tree offers, the exit status, and that nothing is printed
//! besides candidates. Workflow plugin scenarios install real archives with
//! `pohunek workflow install` and complete them from the on-disk store.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
use pohunek_test_support::env::TestEnv;

#[cfg(unix)]
const PUBLIC_COMPLETION_MODE: u32 = 0o644;

/// Runs one bash completion request for `words` (the command line, cursor on
/// the last word) and returns its output.
fn complete_bash(env: &TestEnv, words: &[&str]) -> std::process::Output {
    complete_bash_command(env, words)
        .output()
        .expect("spawn pohunek")
}

/// The bash completion request for `words`, cursor on the last word.
fn complete_bash_command(env: &TestEnv, words: &[&str]) -> std::process::Command {
    let index = words.len() - 1;
    let mut command = env.command(pohunek_test_support::bin_exe("pohunek"));
    command
        .env("POHUNEK_COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", index.to_string())
        .env("_CLAP_IFS", "\n")
        .arg("--")
        .args(words);
    command
}

/// Entry names directly under `dir`, sorted.
fn entries(dir: &std::path::Path) -> Vec<OsString> {
    let mut names = fs::read_dir(dir)
        .expect("list directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// An empty word after a command group lists exactly its subcommands, in
/// declaration order, ahead of clap's own `help` and the flags.
#[test]
fn subcommand_completion_lists_each_group_in_declaration_order() {
    let env = TestEnv::new().expect("private completion environment");
    let scenarios: &[(&[&str], &[&str])] = &[
        (
            &["pohunek", "service", ""],
            &[
                "install",
                "upgrade",
                "uninstall",
                "refresh-path",
                "status",
                "check",
                "lock",
            ],
        ),
        (
            &["pohunek", "plugin", ""],
            &[
                "list",
                "inspect",
                "install",
                "link",
                "update",
                "select",
                "enable",
                "disable",
                "uninstall",
                "doctor",
                "profile",
            ],
        ),
        (&["pohunek", "plugin", "profile", ""], &["list", "migrate"]),
    ];
    for (words, expected) in scenarios {
        let output = complete_bash(&env, words);
        assert!(output.status.success(), "{words:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{words:?}: {output:?}");
        let candidates = String::from_utf8(output.stdout).expect("UTF-8 candidates");
        let subcommands = candidates
            .lines()
            .filter(|line| !line.starts_with('-') && *line != "help")
            .collect::<Vec<_>>();
        assert_eq!(subcommands, *expected, "{words:?}: {candidates:?}");
    }
}

#[test]
fn flag_completion_follows_the_active_cli_tree() {
    let env = TestEnv::new().expect("private completion environment");
    let scenarios: &[(&[&str], &[&str])] = &[
        (
            &["pohunek", "session", "rm", "--accept-"],
            &["--accept-unconfirmed-cleanup"],
        ),
        (&["pohunek", "service", "install", "--fr"], &["--from"]),
        (&["pohunek", "service", "install", "--pr"], &["--prefix"]),
        (
            &["pohunek", "service", "uninstall", "--st"],
            &["--stop-sessions"],
        ),
        (&["pohunek", "service", "uninstall", "--pu"], &["--purge"]),
        (
            &["pohunek", "service", "refresh-path", "--accept-"],
            &["--accept-runtime-loss"],
        ),
        (&["pohunek", "plugin", "install", "--cat"], &["--catalog"]),
        (&["pohunek", "plugin", "install", "--no"], &["--no-enable"]),
        (&["pohunek", "plugin", "install", "--sha"], &["--sha256"]),
        (&["pohunek", "plugin", "select", "--dig"], &["--digest"]),
        (
            &["pohunek", "plugin", "uninstall", "--remove"],
            &["--remove-modified"],
        ),
        (
            &["pohunek", "integration", "install", "--prof"],
            &["--profile"],
        ),
        (
            &["pohunek", "daemon", "start", "--dev"],
            &["--dev-subprocess"],
        ),
    ];
    for (words, expected) in scenarios {
        let output = complete_bash(&env, words);
        assert!(output.status.success(), "{words:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{words:?}: {output:?}");
        let candidates = String::from_utf8(output.stdout).expect("UTF-8 candidates");
        let lines = candidates.lines().collect::<Vec<_>>();
        for candidate in *expected {
            assert!(
                lines.contains(candidate),
                "{words:?} omitted {candidate:?}: {lines:?}"
            );
        }
    }
}

#[test]
fn path_arguments_complete_files_or_directories_as_the_cli_exposes_them() {
    let env = TestEnv::new().expect("private completion environment");
    let source_dir = env.root().join("source-dir");
    let source_file = env.root().join("source-file");
    fs::create_dir(&source_dir).expect("create source directory");
    fs::write(&source_file, b"archive").expect("create source file");
    let prefix = env.root().join("s").to_string_lossy().into_owned();
    let directory_candidate = format!("{}/", source_dir.display());
    let file_candidate = source_file.display().to_string();

    for words in [
        ["pohunek", "service", "install", "--from", prefix.as_str()].as_slice(),
        ["pohunek", "service", "install", "--prefix", prefix.as_str()].as_slice(),
        ["pohunek", "service", "upgrade", "--from", prefix.as_str()].as_slice(),
        ["pohunek", "service", "check", "--prefix", prefix.as_str()].as_slice(),
        ["pohunek", "plugin", "link", prefix.as_str()].as_slice(),
    ] {
        let output = complete_bash(&env, words);
        assert!(output.status.success(), "{words:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{words:?}: {output:?}");
        let candidates = String::from_utf8(output.stdout).expect("UTF-8 candidates");
        assert!(
            candidates.lines().any(|line| line == directory_candidate),
            "{words:?}: {candidates:?}"
        );
        assert!(
            !candidates.lines().any(|line| line == file_candidate),
            "{words:?}: {candidates:?}"
        );
    }

    for words in [
        ["pohunek", "plugin", "install", "--catalog", prefix.as_str()].as_slice(),
        ["pohunek", "plugin", "update", "--catalog", prefix.as_str()].as_slice(),
        ["pohunek", "plugin", "install", prefix.as_str()].as_slice(),
        ["pohunek", "plugin", "update", "pkg", prefix.as_str()].as_slice(),
    ] {
        let output = complete_bash(&env, words);
        assert!(output.status.success(), "{words:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{words:?}: {output:?}");
        let candidates = String::from_utf8(output.stdout).expect("UTF-8 candidates");
        assert!(
            candidates.lines().any(|line| line == directory_candidate),
            "{words:?}: {candidates:?}"
        );
        assert!(
            candidates.lines().any(|line| line == file_candidate),
            "{words:?}: {candidates:?}"
        );
    }
}

#[test]
fn profile_completion_reads_private_agent_profiles_through_the_cli() {
    let env = TestEnv::new().expect("private completion environment");
    let agents = env.config_home().join("pohunek/agents");
    fs::create_dir_all(&agents).expect("create agents directory");
    #[cfg(unix)]
    fs::set_permissions(&agents, fs::Permissions::from_mode(0o700))
        .expect("make agents directory private");
    fs::write(agents.join("work.toml"), b"[agent]\n").expect("write profile name");
    fs::write(agents.join("weekend.toml"), b"[agent]\n").expect("write another profile name");
    fs::write(agents.join("notes.txt"), b"not a profile").expect("write unrelated file");

    for words in [
        ["pohunek", "integration", "install", "--profile", "wo"].as_slice(),
        ["pohunek", "integration", "status", "--profile", "wo"].as_slice(),
        ["pohunek", "integration", "doctor", "--profile", "wo"].as_slice(),
        ["pohunek", "integration", "uninstall", "--profile", "wo"].as_slice(),
        ["pohunek", "plugin", "profile", "migrate", "wo"].as_slice(),
    ] {
        let output = complete_bash(&env, words);
        assert!(output.status.success(), "{words:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{words:?}: {output:?}");
        assert_eq!(output.stdout, b"work", "{words:?}");
    }

    // `integration update` selects no config home, so it offers no
    // `--profile`; `integration install --prof` in
    // `flag_completion_follows_the_active_cli_tree` shows the same prefix
    // reaches the flag where it exists.
    let output = complete_bash(&env, &["pohunek", "integration", "update", "--prof"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let candidates = String::from_utf8(output.stdout).expect("UTF-8 candidates");
    assert!(
        !candidates.lines().any(|line| line == "--profile"),
        "{candidates:?}"
    );
}

#[test]
fn completion_scripts_cover_active_commands_for_each_shell() {
    for (shell, marker, host_marker) in [
        ("bash", "complete", "--host"),
        ("zsh", "#compdef pohunek", "--host"),
        ("fish", "complete -c pohunek", "-l host"),
    ] {
        let env = TestEnv::new().expect("private completion environment");
        let output = env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(["completions", shell])
            .output()
            .expect("generate completion");
        assert!(output.status.success(), "{shell}: {output:?}");
        assert!(output.stderr.is_empty(), "{shell}: {output:?}");
        let script = String::from_utf8(output.stdout).expect("UTF-8 completion");
        for word in [
            marker,
            host_marker,
            "agent-skill",
            "session",
            "service",
            "plugin",
            "accept-unconfirmed-cleanup",
            "stop-sessions",
            "purge",
            "prefix",
            "from",
            "profile",
            "migrate",
            "inspect",
            "uninstall",
            "no-enable",
            "sha256",
            "catalog",
            "remove-modified",
            "digest",
        ] {
            assert!(script.contains(word), "{shell} lacks {word:?}");
        }
    }
}

#[test]
fn dynamic_completion_bootstrap_uses_the_public_environment_protocol() {
    for shell in ["bash", "zsh", "fish"] {
        let env = TestEnv::new().expect("private completion environment");
        let output = env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(["completions", shell, "--dynamic"])
            .output()
            .expect("generate dynamic completion");
        assert!(output.status.success(), "{shell}: {output:?}");
        assert!(output.stderr.is_empty(), "{shell}: {output:?}");
        let script = String::from_utf8(output.stdout).expect("UTF-8 bootstrap");
        assert!(
            script.contains(&format!("POHUNEK_COMPLETE={shell} pohunek")),
            "{script}"
        );
    }
}

#[test]
fn setup_installs_and_updates_completion_at_the_shell_paths() {
    for (shell, relative) in [
        ("bash", "bash-completion/completions/pohunek"),
        ("zsh", "zsh/site-functions/_pohunek"),
        ("fish", "fish/completions/pohunek.fish"),
    ] {
        let env = TestEnv::new().expect("private completion environment");
        let path: PathBuf = if shell == "fish" {
            env.config_home().join(relative)
        } else {
            env.data_home().join(relative)
        };
        let command = || env.command(pohunek_test_support::bin_exe("pohunek"));
        let installed = command()
            .args(["setup", "completions", shell])
            .output()
            .expect("install static completion");
        assert!(installed.status.success(), "{shell}: {installed:?}");
        let static_script = fs::read_to_string(&path).expect("read installed completion");
        assert!(!static_script.contains("POHUNEK_COMPLETE="), "{shell}");

        let updated = command()
            .args(["setup", "completions", shell, "--dynamic"])
            .output()
            .expect("replace with dynamic completion");
        assert!(updated.status.success(), "{shell}: {updated:?}");
        let script = fs::read_to_string(&path).expect("read updated completion");
        assert!(
            script.contains(&format!("POHUNEK_COMPLETE={shell} pohunek")),
            "{shell}"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path)
                .expect("completion metadata")
                .permissions()
                .mode()
                & 0o777,
            PUBLIC_COMPLETION_MODE,
            "{shell}: installed completion is owner-writable and public data"
        );
    }
}

/// Session-target and package completion with no daemon running exit
/// successfully with no candidates and no error output, and create nothing in
/// the runtime directory where a started daemon would bind its socket.
#[test]
fn dynamic_completion_without_a_daemon_prints_nothing_and_starts_nothing() {
    // Positive control: the request protocol reaches clap's completer, so the
    // empty output below is the lookups' answer, not an ignored request.
    let env = TestEnv::new().expect("create the hermetic test environment");
    let output = complete_bash(&env, &["pohunek", "atta"]);
    assert!(output.status.success(), "{:?}", output.status);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "attach");

    for words in [
        ["pohunek", "attach", "local/"].as_slice(),
        // A later `session wait` target uses the same session completer.
        ["pohunek", "session", "wait", "s-1", "local/"].as_slice(),
        ["pohunek", "plugin", "inspect", "acme"].as_slice(),
    ] {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let before = entries(env.runtime_dir());

        let output = complete_bash(&env, words);

        assert!(output.status.success(), "{words:?}: {:?}", output.status);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "",
            "{words:?} offered candidates"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "",
            "{words:?} printed an error"
        );
        assert_eq!(entries(env.runtime_dir()), before, "{words:?}");
    }
}

/// `session wait --until` completes the turn event kinds offline, after a
/// comma as well, so a comma list is completable one kind at a time.
#[test]
fn session_wait_until_completes_turn_kinds_in_a_comma_list() {
    for (words, expected) in [
        (
            ["pohunek", "session", "wait", "s-1", "--until", "a"].as_slice(),
            "agent_blocked\napproval_required",
        ),
        (
            [
                "pohunek",
                "session",
                "wait",
                "s-1",
                "--until",
                "turn_completed,ex",
            ]
            .as_slice(),
            "turn_completed,exited",
        ),
    ] {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let output = complete_bash(&env, words);
        assert!(output.status.success(), "{words:?}: {:?}", output.status);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "{words:?}"
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{words:?}");
    }
}

/// Damage a scenario does to the installed store of `env`; the digest names
/// the installed plugin.
type StoreDamage = fn(&TestEnv, &PackageDigest);

/// Store root of the workflow plugins of `env`.
fn workflow_store(env: &TestEnv) -> PathBuf {
    env.state_home().join("pohunek/workflow-plugins")
}

/// A valid workflow plugin manifest of `id` declaring `actions`, each running
/// the archive's `bin/tool`.
fn workflow_manifest(id: &str, actions: &[&str]) -> String {
    let mut document = format!(
        "schema = 1\n\n[plugin]\nid = \"{id}\"\nversion = \"1.0.0\"\ndescription = \"Completion fixture\"\n"
    );
    for action in actions {
        write!(
            document,
            "\n[actions.{action}]\nargv = [\"bin/tool\", \"{action}\"]\n"
        )
        .expect("format into a String");
    }
    document
}

/// Builds a workflow plugin archive of `id` with `actions`, installs it with
/// `pohunek workflow install --yes` (plus `extra`), and returns its digest.
fn install_workflow(env: &TestEnv, id: &str, actions: &[&str], extra: &[&str]) -> PackageDigest {
    let files = [
        ArchiveEntry {
            path: "bin/tool".to_owned(),
            contents: b"#!/bin/sh\nexit 0\n".to_vec(),
            executable: true,
        },
        ArchiveEntry {
            path: "workflow.toml".to_owned(),
            contents: workflow_manifest(id, actions).into_bytes(),
            executable: false,
        },
    ];
    let bytes = build_archive(&files, &Limits::DEFAULT).expect("the archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    let archive = env.root().join(format!("{id}.tar.zst"));
    fs::write(&archive, bytes).expect("write the archive");
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["workflow", "install"])
        .arg(&archive)
        .args(["--sha256", digest.as_str(), "--yes", "--json"])
        .args(extra)
        .output()
        .expect("run workflow install");
    assert!(output.status.success(), "install {id}: {output:?}");
    digest
}

/// Runs a bash completion request that must succeed silently on stderr and
/// returns its value candidates, one per line.
///
/// Flags clap offers beside the values for an empty word are dropped: no
/// plugin id or action name starts with `-`.
fn candidates(env: &TestEnv, words: &[&str]) -> Vec<String> {
    value_candidates(words, &complete_bash(env, words))
}

/// The value candidates of a completion request for `words` that must
/// succeed silently on stderr.
fn value_candidates(words: &[&str], output: &std::process::Output) -> Vec<String> {
    assert!(output.status.success(), "{words:?}: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "",
        "{words:?} printed an error"
    );
    String::from_utf8(output.stdout.clone())
        .expect("UTF-8 candidates")
        .lines()
        .filter(|line| !line.starts_with('-'))
        .map(str::to_owned)
        .collect()
}

/// Every entry below `dir` with its mode and size, so a scenario can show a
/// completion request changed nothing in the workflow store.
#[cfg(unix)]
fn tree(dir: &std::path::Path) -> Vec<(PathBuf, u32, u64)> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let metadata = fs::symlink_metadata(&next).expect("stat a store entry");
        found.push((next.clone(), metadata.permissions().mode(), metadata.len()));
        if metadata.is_dir() {
            pending.extend(
                fs::read_dir(&next)
                    .expect("list a store directory")
                    .map(|entry| entry.expect("store entry").path()),
            );
        }
    }
    found.sort();
    found
}

/// Workflow plugin commands that name an installed plugin complete its id from
/// the store; `run` offers only enabled plugins, since it refuses disabled ones.
#[cfg(unix)]
#[test]
fn workflow_plugin_ids_complete_from_the_installed_store() {
    let env = TestEnv::new().expect("private completion environment");
    install_workflow(&env, "acme.work", &["overview", "sync"], &[]);
    install_workflow(&env, "acme.lint", &["check"], &[]);
    install_workflow(&env, "acme.paused", &["report"], &["--no-enable"]);
    install_workflow(&env, "other.tool", &["build"], &[]);
    let store = tree(&workflow_store(&env));

    for subcommand in ["inspect", "enable", "disable", "status", "uninstall"] {
        assert_eq!(
            candidates(&env, &["pohunek", "workflow", subcommand, "acme."]),
            ["acme.lint", "acme.paused", "acme.work"],
            "{subcommand}"
        );
    }
    assert_eq!(
        candidates(&env, &["pohunek", "workflow", "run", "acme."]),
        ["acme.lint", "acme.work"],
        "run offers only enabled plugins"
    );
    assert_eq!(
        candidates(&env, &["pohunek", "workflow", "inspect", "acme.w"]),
        ["acme.work"]
    );
    assert_eq!(
        candidates(
            &env,
            &["pohunek", "--host", "local", "workflow", "run", "o"]
        ),
        ["other.tool"]
    );
    assert!(candidates(&env, &["pohunek", "workflow", "inspect", "zzz"]).is_empty());
    // Workflow commands refuse a remote `--host` with `workflow_local_only`,
    // so a line naming one offers no local plugin.
    for words in [
        ["pohunek", "--host", "build-box", "workflow", "run", "o"].as_slice(),
        [
            "pohunek",
            "--host=build-box",
            "workflow",
            "inspect",
            "acme.",
        ]
        .as_slice(),
        [
            "pohunek",
            "workflow",
            "enable",
            "--host",
            "build-box",
            "acme.",
        ]
        .as_slice(),
    ] {
        assert!(candidates(&env, words).is_empty(), "{words:?}");
    }
    assert_eq!(
        tree(&workflow_store(&env)),
        store,
        "completion changed the store"
    );
}

/// This machine's own overlay name in `--host` is the local host, so workflow
/// lines naming it complete the local store, as the command runs locally; a
/// peer name still offers nothing.
#[cfg(target_os = "linux")]
#[test]
fn workflow_completion_treats_the_machines_own_overlay_name_as_local() {
    let env = TestEnv::new().expect("private completion environment");
    install_workflow(&env, "other.tool", &["build"], &[]);
    let bin = env.root().join("self-netbird-bin");
    fs::create_dir_all(&bin).expect("create fixture bin");
    let netbird = bin.join("netbird");
    pohunek_test_support::fs::write_file(
        &netbird,
        "#!/bin/sh\nprintf '%s\\n' '{\"fqdn\":\"fixture-self.netbird.test\",\"netbirdIp\":\"100.64.0.7\",\"peers\":{\"details\":[{\"fqdn\":\"fixture-remote.netbird.test\",\"netbirdIp\":\"100.64.0.42\",\"publicKey\":\"remote-key\",\"status\":\"Connected\"}]}}'\n",
    )
    .expect("write fake netbird");
    fs::set_permissions(&netbird, fs::Permissions::from_mode(0o700))
        .expect("make fake netbird executable");
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&inherited_path)))
            .expect("join PATH");
    let complete = |words: &[&str]| {
        let output = complete_bash_command(&env, words)
            .env("PATH", &path)
            .output()
            .expect("spawn pohunek");
        value_candidates(words, &output)
    };

    for host in ["fixture-self.netbird.test", "100.64.0.7"] {
        assert_eq!(
            complete(&["pohunek", "--host", host, "workflow", "run", "o"]),
            ["other.tool"],
            "{host}"
        );
        assert_eq!(
            complete(&[
                "pohunek",
                "--host",
                host,
                "workflow",
                "run",
                "other.tool",
                "b"
            ]),
            ["build"],
            "{host}"
        );
    }
    assert!(complete(&[
        "pohunek",
        "--host",
        "fixture-remote",
        "workflow",
        "run",
        "o"
    ])
    .is_empty());
}

/// `workflow run <plugin> <action>` completes the actions the typed plugin's
/// installed manifest declares, and none for a plugin `run` would refuse.
/// Only the action name completes: every later word is the action's own
/// verbatim argument.
#[cfg(unix)]
#[test]
fn workflow_run_completes_the_actions_of_the_typed_plugin() {
    let env = TestEnv::new().expect("private completion environment");
    install_workflow(&env, "acme.work", &["overview", "sync", "status"], &[]);
    install_workflow(&env, "acme.lint", &["check"], &[]);
    install_workflow(&env, "acme.paused", &["report"], &["--no-enable"]);
    let store = tree(&workflow_store(&env));

    assert_eq!(
        candidates(&env, &["pohunek", "workflow", "run", "acme.work", ""]),
        ["overview", "status", "sync"]
    );
    assert_eq!(
        candidates(&env, &["pohunek", "workflow", "run", "acme.work", "s"]),
        ["status", "sync"]
    );
    assert_eq!(
        candidates(&env, &["pohunek", "workflow", "run", "acme.lint", ""]),
        ["check"]
    );
    // `--json` belongs to pohunek only before the plugin id.
    assert_eq!(
        candidates(
            &env,
            &["pohunek", "workflow", "run", "--json", "acme.work", "s"]
        ),
        ["status", "sync"]
    );
    assert_eq!(
        candidates(
            &env,
            &[
                "pohunek",
                "--host",
                "local",
                "workflow",
                "run",
                "acme.lint",
                "c"
            ]
        ),
        ["check"]
    );
    for words in [
        // `workflow_local_only` refuses a remote `--host`.
        [
            "pohunek",
            "--host",
            "build-box",
            "workflow",
            "run",
            "acme.lint",
            "",
        ]
        .as_slice(),
        // Words after the action name are its arguments, never actions.
        ["pohunek", "workflow", "run", "acme.work", "overview", "s"].as_slice(),
        ["pohunek", "workflow", "run", "acme.work", "overview", ""].as_slice(),
        ["pohunek", "workflow", "run", "acme.work", "--json", "s"].as_slice(),
        ["pohunek", "workflow", "run", "acme.paused", ""].as_slice(),
        ["pohunek", "workflow", "run", "acme.unknown", ""].as_slice(),
        ["pohunek", "workflow", "run", "Not/AnId", ""].as_slice(),
    ] {
        assert!(candidates(&env, words).is_empty(), "{words:?}");
    }
    assert_eq!(
        tree(&workflow_store(&env)),
        store,
        "completion changed the store"
    );
}

/// A missing, corrupt, unsafe or tampered store completes nothing it cannot
/// trust, prints nothing, exits successfully, and is left as found.
#[cfg(unix)]
#[test]
fn workflow_completion_over_a_missing_or_unusable_store_is_silent() {
    let ids = ["pohunek", "workflow", "inspect", ""];
    let actions = ["pohunek", "workflow", "run", "acme.work", ""];

    let env = TestEnv::new().expect("private completion environment");
    assert!(candidates(&env, &ids).is_empty());
    assert!(candidates(&env, &actions).is_empty());
    assert!(
        !env.state_home().join("pohunek").exists(),
        "completion created the store"
    );

    // Each case starts from a store whose plugin completes, so its empty
    // output comes from the damage, not from the setup.
    let damaged: [(&str, StoreDamage); 3] = [
        ("corrupt registry", |env, _digest| {
            fs::write(workflow_store(env).join("registry.json"), b"{ not json")
                .expect("corrupt the registry");
        }),
        ("unsafe store root", |env, _digest| {
            fs::set_permissions(workflow_store(env), fs::Permissions::from_mode(0o755))
                .expect("widen the store root");
        }),
        ("tampered manifest", |env, digest| {
            let hex = digest.as_str().trim_start_matches("sha256:");
            fs::write(
                workflow_store(env)
                    .join("packages")
                    .join(hex)
                    .join("files/workflow.toml"),
                workflow_manifest("acme.work", &["overview", "injected"]),
            )
            .expect("tamper with the installed manifest");
        }),
    ];
    for (case, damage) in damaged {
        let env = TestEnv::new().expect("private completion environment");
        let digest = install_workflow(&env, "acme.work", &["overview"], &[]);
        assert_eq!(candidates(&env, &ids), ["acme.work"], "{case}");
        assert_eq!(candidates(&env, &actions), ["overview"], "{case}");

        damage(&env, &digest);
        let store = tree(&workflow_store(&env));
        let found = candidates(&env, &actions);
        assert!(found.is_empty(), "{case} offered actions {found:?}");
        if case == "tampered manifest" {
            // The registry still lists the plugin; only its files are refused.
            assert_eq!(candidates(&env, &ids), ["acme.work"], "{case}");
        } else {
            assert!(candidates(&env, &ids).is_empty(), "{case}");
        }
        assert_eq!(
            tree(&workflow_store(&env)),
            store,
            "{case} changed the store"
        );
    }
}

/// Generating or installing a completion script reads no workflow store: the
/// script carries no installed plugin id or action, and an absent store stays
/// absent.
#[cfg(unix)]
#[test]
fn completion_scripts_do_no_workflow_store_io() {
    let command = |env: &TestEnv, arguments: &[&str]| {
        let output = env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(arguments)
            .output()
            .expect("run pohunek");
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}: {output:?}");
        String::from_utf8(output.stdout).expect("UTF-8 output")
    };

    let env = TestEnv::new().expect("private completion environment");
    install_workflow(&env, "acme.probeplugin", &["probeaction"], &[]);
    // Positive control: the dynamic completer does see the installed plugin.
    assert_eq!(
        candidates(&env, &["pohunek", "workflow", "run", "acme.probe"]),
        ["acme.probeplugin"]
    );
    let store = tree(&workflow_store(&env));
    for shell in ["bash", "zsh", "fish"] {
        for arguments in [
            ["completions", shell].as_slice(),
            ["completions", shell, "--dynamic"].as_slice(),
        ] {
            let script = command(&env, arguments);
            for installed in ["probeplugin", "probeaction"] {
                assert!(!script.contains(installed), "{arguments:?} has {installed}");
            }
        }
    }
    assert_eq!(
        tree(&workflow_store(&env)),
        store,
        "a script read changed the store"
    );

    let env = TestEnv::new().expect("private completion environment");
    for shell in ["bash", "zsh", "fish"] {
        command(&env, &["completions", shell]);
        command(&env, &["setup", "completions", shell]);
        command(&env, &["setup", "completions", shell, "--dynamic"]);
    }
    assert!(
        !workflow_store(&env).exists(),
        "script generation created the workflow store"
    );
}

#[cfg(target_os = "linux")]
#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[cfg(target_os = "linux")]
#[path = "support/pin_overlay.rs"]
mod pin_overlay;

/// `--host` completion on a machine that lists itself: this machine's own
/// record is not a second dialable target, `local` stays the only entry for it.
#[cfg(target_os = "linux")]
#[test]
fn host_completion_offers_this_machine_only_as_local() {
    let env = TestEnv::new().expect("private completion environment");
    let port = pin_overlay::serve_health();
    let (library, capture) = connect_redirect::build(env.root());
    let status = pin_overlay::status(
        Some(("fixture-self.netbird.test", "100.64.0.7")),
        &[pin_overlay::Peer::new(
            "fixture-remote.netbird.test",
            "100.64.0.42",
            "remote-key",
        )],
    );
    let path = pin_overlay::install(&env.root().join("bin"), &capture, &status, &status);
    let words = ["pohunek", "--host", ""];
    let output = complete_bash_command(&env, &words)
        .env("PATH", &path)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .env("POHUNEK_REMOTE_PORT", port.to_string())
        .output()
        .expect("spawn pohunek");
    let identity = pohunek_client::ExternalIdentity::peer_id("remote-key")
        .expect("peer identity")
        .selector();
    assert_eq!(
        value_candidates(&words, &output),
        ["local".to_owned(), format!("netbird:{identity}@{port}")]
    );
}

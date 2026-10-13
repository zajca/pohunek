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
//! besides candidates.

use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use pohunek_test_support::env::TestEnv;

#[cfg(unix)]
const PUBLIC_COMPLETION_MODE: u32 = 0o644;

/// Runs one bash completion request for `words` (the command line, cursor on
/// the last word) and returns its output.
fn complete_bash(env: &TestEnv, words: &[&str]) -> std::process::Output {
    let index = words.len() - 1;
    env.command(pohunek_test_support::bin_exe("pohunek"))
        .env("POHUNEK_COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", index.to_string())
        .env("_CLAP_IFS", "\n")
        .arg("--")
        .args(words)
        .output()
        .expect("spawn pohunek")
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
            &["install", "upgrade", "uninstall", "status", "check", "lock"],
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

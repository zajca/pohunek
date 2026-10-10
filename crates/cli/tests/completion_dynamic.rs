//! End-to-end: dynamic shell completion against an absent daemon is silent.
//!
//! These drive the built `pohunek` binary through clap's environment
//! completion protocol, the way the installed `source <(POHUNEK_COMPLETE=bash
//! pohunek)` bootstrap does on Tab. The child gets a scrubbed environment with
//! private HOME and XDG directories, so no daemon socket exists for it.
//!
//! The lookup deadline is covered on virtual time by the completion unit
//! scenarios; this file covers what only a process shows: the exit status and
//! that nothing is printed besides candidates.

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

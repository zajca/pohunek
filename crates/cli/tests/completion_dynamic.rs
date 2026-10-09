//! End-to-end: dynamic shell completion against an absent daemon is silent.
//!
//! These drive the built `pohunek` binary through clap's environment
//! completion protocol, the way the installed `source <(POHUNEK_COMPLETE=bash
//! pohunek)` bootstrap does on Tab. The child gets a scrubbed environment with
//! private HOME and XDG directories, so no daemon socket exists for it.
//!
//! The lookup deadline is covered on virtual time by the in-process daemon
//! scenario; this file covers what only a process shows: the exit status and
//! that nothing is printed besides candidates.

use std::ffi::OsString;
use std::fs;

use pohunek_test_support::env::TestEnv;

#[test]
fn generated_completion_scripts_cover_commands_and_private_dynamic_mode() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let binary = pohunek_test_support::bin_exe("pohunek");

    for (shell, static_marker, host_marker, dynamic_script) in [
        (
            "bash",
            "complete",
            "--host",
            "source <(POHUNEK_COMPLETE=bash pohunek)\n",
        ),
        (
            "zsh",
            "#compdef pohunek",
            "--host",
            "source <(POHUNEK_COMPLETE=zsh pohunek)\n",
        ),
        (
            "fish",
            "complete -c pohunek",
            "-l host",
            "POHUNEK_COMPLETE=fish pohunek | source\n",
        ),
    ] {
        let static_output = env
            .command(&binary)
            .args(["completions", shell])
            .output()
            .expect("spawn static completion command");
        assert!(static_output.status.success(), "{shell}: {static_output:?}");
        assert!(
            static_output.stderr.is_empty(),
            "{shell}: {static_output:?}"
        );
        let script = String::from_utf8(static_output.stdout).expect("UTF-8 script");
        for marker in [static_marker, host_marker, "session", "agent-skill"] {
            assert!(script.contains(marker), "{shell} is missing {marker:?}");
        }

        let dynamic_output = env
            .command(&binary)
            .args(["completions", shell, "--dynamic"])
            .output()
            .expect("spawn dynamic completion command");
        assert!(
            dynamic_output.status.success(),
            "{shell}: {dynamic_output:?}"
        );
        assert!(
            dynamic_output.stderr.is_empty(),
            "{shell}: {dynamic_output:?}"
        );
        assert_eq!(dynamic_output.stdout, dynamic_script.as_bytes(), "{shell}");
    }
}

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

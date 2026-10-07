//! Tests of how the managed Claude hook scripts find the agent process.
//!
//! The scripts run as `sh '<script>' <action>` below `/bin/sh -c`, the way
//! Claude starts a hook. A shell that does not exec its last command (dash)
//! stays between the agent and the script, so the script must propose the
//! agent, the parent of that shell, as the reporting process.

// Rust guideline compliant 2026-10-07

use std::process::Command;

const CLAUDE_STATE_ASSET: &str = include_str!("assets/claude/pohunek-agent-state.sh");
const CLAUDE_NOTIFY_ASSET: &str = include_str!("assets/claude/pohunek-agent-notify.sh");

/// Markers that delimit the agent-pid resolution in the managed scripts.
const BEGIN: &str = "# BEGIN agent-pid\n";
const END: &str = "# END agent-pid\n";

/// The resolution block of `asset`.
fn resolution_block(asset: &str) -> &str {
    let start = asset.find(BEGIN).expect("the script marks its resolution");
    let end = asset.find(END).expect("the script ends its resolution");
    &asset[start..end]
}

/// Columns `ps` would truncate a command line to when stdout is not a terminal
/// and `COLUMNS` is set; the scripts must read the full command line.
const NARROW_COLUMNS: &str = "20";

/// Longest directory name the test uses for a long hook path, in bytes (below
/// the 255-byte file name limit).
const LONG_DIRECTORY_BYTES: usize = 200;

/// Runs the resolution block as the script `<root>/<directory>/hook.sh` and
/// returns the pid it chose and this process's pid. With `wrapped` the script
/// runs the way Claude starts a hook, `sh -c "sh '<script>' action"` with the
/// path quoted as the installer quotes it, from this process; the trailing `:`
/// keeps that shell from exec'ing the script, as dash does not. Otherwise a
/// shell command line that is not that invocation starts it, and the shell is
/// the process the script must report. `COLUMNS` is narrow in both cases.
fn resolved_pid(block: &str, wrapped: bool, directory: &str) -> (u32, u32) {
    let root = crate::test_support::scoped_dir("ph-hook-pid-");
    let dir = root.join(directory);
    std::fs::create_dir_all(&dir).expect("create the script directory");
    let script = dir.join("hook.sh");
    std::fs::write(&script, format!("set -eu\n{block}\necho \"$agent_pid\"\n"))
        .expect("write the script");
    let quoted = super::shell_single_quote(&script.display().to_string());
    let command = if wrapped {
        format!("sh {quoted} session; :")
    } else {
        format!("printf x | sh {quoted} session; :")
    };
    let output = Command::new("sh")
        .args(["-c", &command])
        .env("COLUMNS", NARROW_COLUMNS)
        .output()
        .expect("run the shell");
    assert!(output.status.success(), "{output:?}");
    let chosen = String::from_utf8(output.stdout)
        .expect("utf-8")
        .trim()
        .parse()
        .expect("a pid");
    (chosen, std::process::id())
}

fn both_assets() -> [(&'static str, &'static str); 2] {
    [
        ("state", CLAUDE_STATE_ASSET),
        ("notify", CLAUDE_NOTIFY_ASSET),
    ]
}

#[test]
fn the_hook_scripts_report_the_agent_not_the_wrapper_shell_between_it_and_them() {
    for (name, asset) in both_assets() {
        let (chosen, agent) = resolved_pid(resolution_block(asset), true, "plain");
        assert_eq!(
            chosen, agent,
            "{name}: the wrapper shell is skipped and its parent, the agent, is chosen"
        );
    }
}

/// `ps` truncates a command line to `COLUMNS` unless asked not to; a long hook
/// path must still be recognised.
#[test]
fn a_long_hook_path_is_recognised_although_columns_is_narrow() {
    let long = "d".repeat(LONG_DIRECTORY_BYTES);
    for (name, asset) in both_assets() {
        let (chosen, agent) = resolved_pid(resolution_block(asset), true, &long);
        assert_eq!(chosen, agent, "{name}");
    }
}

/// The installer quotes an apostrophe in the hook path as `'"'"'`; the script
/// matches the same representation.
#[test]
fn a_hook_path_with_an_apostrophe_is_recognised_as_the_installer_quotes_it() {
    for (name, asset) in both_assets() {
        let (chosen, agent) = resolved_pid(resolution_block(asset), true, "owner's profile");
        assert_eq!(chosen, agent, "{name}");
    }
}

/// Characters that are special in a shell pattern match only themselves.
#[test]
fn a_hook_path_with_pattern_characters_is_matched_literally() {
    for (name, asset) in both_assets() {
        let (chosen, agent) = resolved_pid(resolution_block(asset), true, "a*b[c]?d");
        assert_eq!(chosen, agent, "{name}");
    }
}

#[test]
fn the_hook_scripts_keep_a_parent_shell_that_is_not_the_hook_invocation() {
    for (name, asset) in both_assets() {
        let (chosen, agent) = resolved_pid(resolution_block(asset), false, "plain");
        assert_ne!(
            chosen, agent,
            "{name}: a shell that is the agent's own command stays the reporter"
        );
    }
}

#[test]
fn both_scripts_carry_the_same_resolution_block() {
    assert_eq!(
        resolution_block(CLAUDE_STATE_ASSET),
        resolution_block(CLAUDE_NOTIFY_ASSET)
    );
}

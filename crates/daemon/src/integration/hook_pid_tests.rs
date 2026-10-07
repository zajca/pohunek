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

/// Runs the resolution block as the script `<dir>/hook.sh` and returns the pid
/// it chose and this process's pid. With `wrapped` the script runs the way
/// Claude starts a hook, `sh -c "sh '<script>' action"` from this process; the
/// trailing `:` keeps that shell from exec'ing the script, as dash does not.
/// Otherwise a shell command line that is not that invocation starts it, and
/// the shell is the process the script must report.
fn resolved_pid(block: &str, wrapped: bool) -> (u32, u32) {
    let dir = crate::test_support::scoped_dir("ph-hook-pid-");
    let script = dir.join("hook.sh");
    std::fs::write(&script, format!("set -eu\n{block}\necho \"$agent_pid\"\n"))
        .expect("write the script");
    let command = if wrapped {
        format!("sh '{}' session; :", script.display())
    } else {
        format!("printf x | sh '{}' session; :", script.display())
    };
    let output = Command::new("sh")
        .args(["-c", &command])
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

#[test]
fn the_hook_scripts_report_the_agent_not_the_wrapper_shell_between_it_and_them() {
    for (name, asset) in [
        ("state", CLAUDE_STATE_ASSET),
        ("notify", CLAUDE_NOTIFY_ASSET),
    ] {
        let (chosen, agent) = resolved_pid(resolution_block(asset), true);
        assert_eq!(
            chosen, agent,
            "{name}: the wrapper shell is skipped and its parent, the agent, is chosen"
        );
    }
}

#[test]
fn the_hook_scripts_keep_a_parent_shell_that_is_not_the_hook_invocation() {
    for (name, asset) in [
        ("state", CLAUDE_STATE_ASSET),
        ("notify", CLAUDE_NOTIFY_ASSET),
    ] {
        let (chosen, agent) = resolved_pid(resolution_block(asset), false);
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

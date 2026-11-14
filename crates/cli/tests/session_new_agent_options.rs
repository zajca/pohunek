//! `pohunek session new --opt` and `pohunek plugin profile show` (`#2147`
//! rows 22-23) against a real daemon.
//!
//! The `pohunek` binary runs as a subprocess against a `ControlServer` served
//! from this test process over a real `SessionRegistry`. The profiles run
//! `/bin/sh`, never `$SHELL`.

// Rust guideline compliant 2026-11-14

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;

use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{stderr_text, stdout_text, Harness};

/// The usage exit status of the CLI.
const USAGE_EXIT: i32 = 2;

/// A marker carried by a choice env value; no output may contain it.
const MARKER: &str = "MarkerQ7x2";

/// A profile over the shell with two options; the `next` choice has a floor,
/// which a runtime without a version can never meet.
fn options_profile() -> String {
    format!(
        "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"exec sleep 30\"]\n\
         display_name = \"Work\"\n\
         [options.mode]\nlabel = \"Mode\"\ndefault = \"calm\"\n\
         choices.calm = {{ label = \"Calm\", env = {{ PH_OPT_MODE = \"calm-{MARKER}\" }} }}\n\
         choices.bold = {{ label = \"Bold\", args = [\"--bold\"], env = {{ PH_OPT_MODE = \"bold-{MARKER}\" }} }}\n\
         choices.next = {{ label = \"Next\", min_runtime = \"2.2.0\" }}\n\
         [options.depth]\nlabel = \"Depth\"\ndefault = \"shallow\"\n\
         choices.shallow = {{ label = \"Shallow\" }}\n\
         choices.deep = {{ label = \"Deep\" }}\n"
    )
}

/// Row 22: a malformed `--opt` and an option named twice are usage errors
/// (exit 2) raised before any connection is dialed: a private socket bound
/// where the CLI looks for its daemon never sees a connection.
#[test]
fn malformed_or_repeated_options_are_usage_errors_before_connecting() {
    let env = TestEnv::new().expect("private CLI environment");
    let runtime = env.runtime_dir().join("pohunek");
    std::fs::create_dir(&runtime).expect("create daemon socket directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("make socket directory private");
    let listener = UnixListener::bind(runtime.join("daemon.sock")).expect("bind daemon socket");
    listener.set_nonblocking(true).expect("nonblocking accept");
    for (arguments, fragment) in [
        (
            vec!["--opt", "model=sonnet", "--opt", "model=opus"],
            "model",
        ),
        (vec!["--opt", "model"], "expected OPTION=CHOICE"),
        (vec!["--opt", "=sonnet"], "must not be empty"),
        (vec!["--opt", "model="], "must not be empty"),
    ] {
        let mut all = vec!["session", "new", "--agent", "work"];
        all.extend(arguments.iter().copied());
        all.push("--json");
        let output = env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(&all)
            .output()
            .expect("run CLI");
        assert_eq!(
            output.status.code(),
            Some(USAGE_EXIT),
            "{arguments:?}: {output:?}"
        );
        let document: Value =
            serde_json::from_slice(&output.stdout).expect("one JSON error document");
        assert_eq!(
            document["err"]["code"], "cli_usage",
            "{arguments:?}: {document}"
        );
        assert!(
            document["err"]["msg"]
                .as_str()
                .is_some_and(|message| message.contains(fragment)),
            "{arguments:?}: expected {fragment:?} in {document}"
        );
        assert_eq!(
            listener.accept().map(|_| ()).map_err(|error| error.kind()),
            Err(std::io::ErrorKind::WouldBlock),
            "{arguments:?}: the CLI never connected"
        );
    }
}

/// `session new --opt` sends the selection; the result reports every
/// option's choice, defaults included, in JSON and in the human output.
#[tokio::test]
async fn session_new_sends_the_selection_and_reports_the_resolved_choices() {
    let harness = Harness::start().await;
    harness.profile("work", &options_profile());
    let (code, created) = harness
        .json(&["session", "new", "--agent", "work", "--opt", "mode=bold"])
        .await;
    assert_eq!(code, 0, "{created}");
    assert_eq!(
        created["ok"]["agent_options"],
        json!({"depth": "shallow", "mode": "bold"}),
        "{created}"
    );
    let output = harness
        .run(&["session", "new", "--agent", "work", "--opt", "depth=deep"])
        .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    assert!(
        stdout_text(&output).contains("  options: depth=deep, mode=calm\n"),
        "{}",
        stdout_text(&output)
    );
    let (code, refused) = harness
        .json(&["session", "new", "--agent", "work", "--opt", "colour=red"])
        .await;
    assert_ne!(code, 0);
    assert_eq!(refused["err"]["code"], "agent_option_unknown", "{refused}");
    harness.stop().await;
}

/// Row 23: `plugin profile show --json` reports the options with their
/// choices, availability, reason and floor, from the daemon's inventory; no
/// choice env value reaches stdout or stderr, in JSON or in the human form.
#[tokio::test]
async fn plugin_profile_show_reports_options_without_values() {
    let harness = Harness::start().await;
    harness.profile("work", &options_profile());
    let (code, shown) = harness.json(&["plugin", "profile", "show", "work"]).await;
    assert_eq!(code, 0, "{shown}");
    let entry = &shown["ok"];
    assert_eq!(entry["agent"], "work");
    assert_eq!(entry["display_name"], "Work");
    assert_eq!(
        entry["options"],
        json!([
            {
                "id": "depth",
                "label": "Depth",
                "default": "shallow",
                "choices": [
                    {"id": "deep", "label": "Deep", "available": true},
                    {"id": "shallow", "label": "Shallow", "available": true}
                ]
            },
            {
                "id": "mode",
                "label": "Mode",
                "default": "calm",
                "choices": [
                    {"id": "bold", "label": "Bold", "available": true},
                    {"id": "calm", "label": "Calm", "available": true},
                    {
                        "id": "next",
                        "label": "Next",
                        "available": false,
                        "reason": "runtime_version_unknown",
                        "min_runtime": "2.2.0"
                    }
                ]
            }
        ])
    );
    let human = harness.run(&["plugin", "profile", "show", "work"]).await;
    assert_eq!(human.status.code(), Some(0), "{}", stderr_text(&human));
    let text = stdout_text(&human);
    assert!(text.contains("profile work\n"), "{text}");
    assert!(text.contains("  display name: Work\n"), "{text}");
    assert!(
        text.contains("unavailable (runtime_version_unknown, needs 2.2.0)"),
        "{text}"
    );
    assert!(text.contains("calm*"), "the default is marked: {text}");
    for output in [shown.to_string(), text.clone(), stderr_text(&human)] {
        assert!(
            !output.contains(MARKER),
            "no choice value is printed: {output}"
        );
    }

    let (code, missing) = harness.json(&["plugin", "profile", "show", "nope"]).await;
    assert_ne!(code, 0);
    assert_eq!(
        missing["err"]["code"], "plugin_profile_not_found",
        "{missing}"
    );
    harness.stop().await;
}

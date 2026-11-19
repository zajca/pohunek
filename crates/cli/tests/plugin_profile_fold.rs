//! `pohunek plugin profile fold` (`#2148` rows 12-15): the proposal that
//! collapses a profile family into one parent with options and one preset
//! per input.
//!
//! The `pohunek` binary runs as a subprocess over the harness's hermetic
//! config directory. Fold reads the profile files itself and contacts no
//! daemon; the expected parent and presets follow from the inputs by the
//! documented rules, so the assertions parse the printed TOML and compare
//! keys, never the text layout.

// Rust guideline compliant 2026-11-14

#![cfg(unix)]

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{stderr_text, stdout_text, Harness};
use serde_json::Value;

/// A value marker that must never be printed.
const ENV_MARKER: &str = "MarkerFoldEnv7q";

/// The two profiles of row 12: equal but for `--model` (spelled two ways
/// and placed differently) and the value of `FOO`; `--verbose` and
/// `SHARED` are equal.
fn write_family(harness: &Harness) {
    harness.profile(
        "claude-opus",
        &format!(
            "display_name = \"Opus\"\nbase = \"shell\"\nprogram = \"/bin/sh\"\n\
             args = [\"--model\", \"opus\", \"--verbose\"]\n\
             [env]\nFOO = \"{ENV_MARKER}-a\"\nSHARED = \"{ENV_MARKER}-s\"\n"
        ),
    );
    harness.profile(
        "claude-sonnet",
        &format!(
            "base = \"shell\"\nprogram = \"/bin/sh\"\n\
             args = [\"--verbose\", \"--model=sonnet\"]\n\
             [env]\nFOO = \"{ENV_MARKER}-b\"\nSHARED = \"{ENV_MARKER}-s\"\n"
        ),
    );
}

/// `text` parsed as a TOML table.
fn table(text: &Value) -> toml::Table {
    toml::from_str(text.as_str().expect("a text")).expect("the proposal is TOML")
}

/// `strings` as a TOML array value.
fn array(strings: &[&str]) -> toml::Value {
    toml::Value::Array(
        strings
            .iter()
            .map(|item| toml::Value::String((*item).to_owned()))
            .collect(),
    )
}

/// Folds the [`write_family`] profiles into `claude` and returns the
/// proposal of the JSON output.
async fn folded_family(harness: &Harness) -> Value {
    let (code, document) = harness
        .json(&[
            "plugin",
            "profile",
            "fold",
            "claude-opus",
            "claude-sonnet",
            "--into",
            "claude",
            "--dry-run",
        ])
        .await;
    assert_eq!(code, 0, "{document}");
    document["ok"].clone()
}

/// Row 12: one `model` option with a choice per spelling, one `env-foo`
/// option with a choice per value, the shared flag and env key on the
/// parent, and one preset per input taking its own choices; the input whose
/// flags move behind the shared one is reported reordered.
#[tokio::test]
async fn fold_proposes_a_parent_with_options_and_a_preset_per_input() {
    let harness = Harness::start().await;
    write_family(&harness);
    let proposal = &folded_family(&harness).await;
    assert_eq!(proposal["parent"]["name"], "claude");
    let parent = table(&proposal["parent"]["text"]);
    assert_eq!(parent["base"].as_str(), Some("shell"));
    assert_eq!(parent["program"].as_str(), Some("/bin/sh"));
    assert_eq!(parent["args"], array(&["--verbose"]));
    assert_eq!(
        parent["env"]["SHARED"].as_str(),
        Some("<value of SHARED from claude-opus>")
    );
    assert!(parent.get("display_name").is_none(), "{parent:?}");
    let options = parent["options"].as_table().expect("options");
    assert_eq!(
        options.keys().map(String::as_str).collect::<Vec<_>>(),
        ["env-foo", "model"]
    );
    let model = &options["model"];
    assert_eq!(model["default"].as_str(), Some("opus"));
    assert_eq!(
        model["choices"]["opus"]["args"],
        array(&["--model", "opus"])
    );
    assert_eq!(
        model["choices"]["sonnet"]["args"],
        array(&["--model=sonnet"])
    );
    let env_foo = &options["env-foo"];
    assert_eq!(env_foo["default"].as_str(), Some("v1"));
    assert_eq!(
        env_foo["choices"]["v1"]["env"]["FOO"].as_str(),
        Some("<value of FOO from claude-opus>")
    );
    assert_eq!(
        env_foo["choices"]["v2"]["env"]["FOO"].as_str(),
        Some("<value of FOO from claude-sonnet>")
    );

    let presets = proposal["presets"].as_array().expect("presets");
    let expected = [
        ("claude-opus", "opus", "v1", "reordered", Some("Opus")),
        ("claude-sonnet", "sonnet", "v2", "equal", None),
    ];
    assert_eq!(presets.len(), expected.len(), "{proposal}");
    for (preset, (name, model, env, order, label)) in presets.iter().zip(expected) {
        assert_eq!(preset["name"], name);
        assert_eq!(preset["args_order"], order, "{name}");
        let text = table(&preset["text"]);
        assert_eq!(text["preset"]["parent"].as_str(), Some("claude"), "{name}");
        assert_eq!(
            text["preset"]["options"]["model"].as_str(),
            Some(model),
            "{name}"
        );
        assert_eq!(
            text["preset"]["options"]["env-foo"].as_str(),
            Some(env),
            "{name}"
        );
        assert_eq!(
            text.get("display_name").and_then(toml::Value::as_str),
            label,
            "{name}"
        );
        assert!(
            text.get("base").is_none(),
            "{name}: a preset holds no launch key"
        );
    }

    let output = harness
        .run(&[
            "plugin",
            "profile",
            "fold",
            "claude-opus",
            "claude-sonnet",
            "--into",
            "claude",
            "--dry-run",
        ])
        .await;
    let text = stdout_text(&output);
    assert!(output.status.success(), "{text}{}", stderr_text(&output));
    assert!(
        text.contains(
            "claude-opus: args reordered: --model opus --verbose -> --verbose --model opus"
        ),
        "{text}"
    );
    assert!(text.contains("claude-sonnet: args equal"), "{text}");

    harness.stop().await;
}

/// The proposal of row 12 is a family the daemon loads: written in place of
/// the inputs, the parent and each preset resolve, and each preset offers
/// the parent's options with its own choices as defaults.
#[tokio::test]
async fn the_folded_family_loads_in_the_daemon() {
    let harness = Harness::start().await;
    write_family(&harness);
    let proposal = folded_family(&harness).await;
    harness.profile(
        "claude",
        proposal["parent"]["text"].as_str().expect("parent text"),
    );
    for preset in proposal["presets"].as_array().expect("presets") {
        harness.profile(
            preset["name"].as_str().expect("preset name"),
            preset["text"].as_str().expect("preset text"),
        );
    }
    for (agent, model) in [
        ("claude", "opus"),
        ("claude-opus", "opus"),
        ("claude-sonnet", "sonnet"),
    ] {
        let (code, shown) = harness.json(&["plugin", "profile", "show", agent]).await;
        assert_eq!(code, 0, "{agent}: {shown}");
        let default = shown["ok"]["options"]
            .as_array()
            .expect("options")
            .iter()
            .find(|option| option["id"] == "model")
            .map(|option| option["default"].clone());
        assert_eq!(default, Some(Value::from(model)), "{agent}: {shown}");
    }
    harness.stop().await;
}

/// Row 13: env values never leave the CLI, in the JSON and the text output
/// alike, and an argument that looks like a credential refuses the fold
/// without printing it.
#[tokio::test]
async fn fold_never_prints_an_env_value_or_a_credential_argument() {
    let harness = Harness::start().await;
    write_family(&harness);
    for json in [true, false] {
        let mut arguments = vec![
            "plugin",
            "profile",
            "fold",
            "claude-opus",
            "claude-sonnet",
            "--into",
            "claude",
            "--dry-run",
        ];
        if json {
            arguments.push("--json");
        }
        let output = harness.run(&arguments).await;
        assert!(output.status.success(), "{}", stderr_text(&output));
        let shown = format!("{}{}", stdout_text(&output), stderr_text(&output));
        assert!(!shown.contains(ENV_MARKER), "json={json}: {shown}");
    }

    let token = format!("sk-ant-{ENV_MARKER}0123456789abcdef");
    harness.profile(
        "leaky",
        &format!(
            "base = \"shell\"\nprogram = \"/bin/sh\"\n\
             args = [\"--verbose\", \"--api-key\", \"{token}\"]\n"
        ),
    );
    harness.profile(
        "plain",
        "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"--verbose\"]\n",
    );
    let output = harness
        .run(&[
            "plugin",
            "profile",
            "fold",
            "plain",
            "leaky",
            "--into",
            "base",
            "--dry-run",
            "--json",
        ])
        .await;
    let shown = format!("{}{}", stdout_text(&output), stderr_text(&output));
    assert_eq!(output.status.code(), Some(1), "{shown}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(
        document["err"]["code"], "fold_credential_literal",
        "{shown}"
    );
    assert!(
        document["err"]["msg"]
            .as_str()
            .is_some_and(|msg| msg.contains("args[1] of profile leaky")),
        "{shown}"
    );
    assert!(!shown.contains(ENV_MARKER), "{shown}");
    harness.stop().await;
}

/// The legacy `[resume] fork_args` the daemon reads and drops is no launch
/// input: the proposed parent omits it, so a credential it holds is printed
/// in neither the text nor the JSON output.
#[tokio::test]
async fn fold_drops_the_legacy_fork_args_of_resume() {
    let harness = Harness::start().await;
    let token = format!("sk-ant-{ENV_MARKER}fork0123456789abcdef");
    for (name, model) in [("fork-opus", "opus"), ("fork-sonnet", "sonnet")] {
        harness.profile(
            name,
            &format!(
                "base = \"shell\"\nprogram = \"/bin/sh\"\n\
                 args = [\"--model\", \"{model}\"]\n\
                 [resume]\nreference_kind = \"id\"\n\
                 args = [\"--resume\", \"{{reference}}\"]\n\
                 fork_args = [\"--token\", \"{token}\"]\n"
            ),
        );
    }
    let arguments = [
        "plugin",
        "profile",
        "fold",
        "fork-opus",
        "fork-sonnet",
        "--into",
        "fork",
        "--dry-run",
    ];
    let (code, document) = harness.json(&arguments).await;
    assert_eq!(code, 0, "{document}");
    assert!(!document.to_string().contains(ENV_MARKER), "{document}");
    let parent = table(&document["ok"]["parent"]["text"]);
    let resume = parent["resume"]
        .as_table()
        .expect("the parent keeps [resume]");
    assert_eq!(resume["args"], array(&["--resume", "{reference}"]));
    assert!(resume.get("fork_args").is_none(), "{parent:?}");

    let output = harness.run(&arguments).await;
    let shown = format!("{}{}", stdout_text(&output), stderr_text(&output));
    assert!(output.status.success(), "{shown}");
    assert!(shown.contains("[resume]"), "{shown}");
    assert!(!shown.contains(ENV_MARKER), "{shown}");
    harness.stop().await;
}

/// Row 14: inputs that differ in a launch key other than `args` and
/// `[env]` are refused, naming the key and two profiles; so is a preset.
#[tokio::test]
async fn fold_refuses_inputs_that_differ_in_a_launch_key() {
    let harness = Harness::start().await;
    harness.profile(
        "one",
        "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-a\"]\n",
    );
    harness.profile(
        "two",
        "base = \"shell\"\nprogram = \"/bin/bash\"\nargs = [\"-b\"]\n",
    );
    harness.profile("three", "[preset]\nparent = \"one\"\n");
    let cases = [
        (
            ["one", "two"],
            "profiles one and two differ in `program`; fold needs it equal in every input",
        ),
        (
            ["one", "three"],
            "profile three is a preset; fold takes full profiles",
        ),
    ];
    for (names, detail) in cases {
        let (code, refused) = harness
            .json(&[
                "plugin",
                "profile",
                "fold",
                names[0],
                names[1],
                "--into",
                "parent",
                "--dry-run",
            ])
            .await;
        assert_eq!(code, 1, "{names:?}: {refused}");
        assert_eq!(refused["err"]["code"], "fold_incompatible", "{names:?}");
        assert_eq!(
            refused["err"]["msg"],
            format!("cannot fold the profiles: {detail}"),
            "{names:?}"
        );
    }
    harness.stop().await;
}

/// Row 15: fold only proposes, so `--dry-run` is required, and it needs at
/// least two inputs: both are usage errors that exit 2 before any file is
/// read.
#[tokio::test]
async fn fold_without_dry_run_or_with_one_input_is_a_usage_error() {
    let harness = Harness::start().await;
    write_family(&harness);
    let cases: [(&[&str], &str); 2] = [
        (
            &[
                "plugin",
                "profile",
                "fold",
                "claude-opus",
                "claude-sonnet",
                "--into",
                "claude",
            ],
            "the following required arguments were not provided:\n  --dry-run",
        ),
        (
            &[
                "plugin",
                "profile",
                "fold",
                "claude-opus",
                "--into",
                "claude",
                "--dry-run",
            ],
            "2 values required by '<NAME> <NAME>...'; only 1 was provided",
        ),
    ];
    for (arguments, usage) in cases {
        let output = harness.run(arguments).await;
        let stderr = stderr_text(&output);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {stderr}");
        assert!(stderr.contains(usage), "{arguments:?}: {stderr}");
        assert!(stdout_text(&output).is_empty(), "{arguments:?}");
    }
    harness.stop().await;
}

/// An equal `[rights]` table folds: the parent carries it, because a preset
/// takes its parent's rights.
#[tokio::test]
async fn fold_carries_an_equal_rights_table_to_the_parent() {
    let harness = Harness::start().await;
    let rights = "[rights]\nspawn_profiles = [\"worker\"]\n";
    for (name, model) in [("rights-opus", "opus"), ("rights-sonnet", "sonnet")] {
        harness.profile(
            name,
            &format!(
                "base = \"shell\"\nprogram = \"/bin/sh\"\n\
                 args = [\"--model\", \"{model}\"]\n{rights}"
            ),
        );
    }
    let (code, document) = harness
        .json(&[
            "plugin",
            "profile",
            "fold",
            "rights-opus",
            "rights-sonnet",
            "--into",
            "rights",
            "--dry-run",
        ])
        .await;
    assert_eq!(code, 0, "{document}");
    let parent = table(&document["ok"]["parent"]["text"]);
    let table = parent["rights"]
        .as_table()
        .expect("the parent keeps [rights]");
    assert_eq!(table["spawn_profiles"], array(&["worker"]), "{parent:?}");
    harness.stop().await;
}

/// Inputs whose `[rights]` tables differ cannot share a parent, because a
/// preset takes its parent's rights.
#[tokio::test]
async fn fold_refuses_inputs_that_differ_in_rights() {
    let harness = Harness::start().await;
    harness.profile(
        "rights-a",
        "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-a\"]\n\
         [rights]\nspawn_profiles = [\"worker\"]\n",
    );
    harness.profile(
        "rights-b",
        "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-b\"]\n\
         [rights]\nspawn_profiles = [\"review\"]\n",
    );
    let (code, refused) = harness
        .json(&[
            "plugin",
            "profile",
            "fold",
            "rights-a",
            "rights-b",
            "--into",
            "parent",
            "--dry-run",
        ])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "fold_incompatible");
    assert_eq!(
        refused["err"]["msg"],
        "cannot fold the profiles: profiles rights-a and rights-b differ in \
         `rights`; fold needs it equal in every input",
        "{refused}"
    );
    harness.stop().await;
}

/// A `[rights]` table on only one input counts as different: the parent
/// must not gain rights one input never had.
#[tokio::test]
async fn fold_refuses_rights_on_only_one_input() {
    let harness = Harness::start().await;
    harness.profile(
        "rights-only",
        "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-a\"]\n\
         [rights]\nspawn_profiles = [\"worker\"]\n",
    );
    harness.profile(
        "rights-none",
        "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-b\"]\n",
    );
    let (code, refused) = harness
        .json(&[
            "plugin",
            "profile",
            "fold",
            "rights-only",
            "rights-none",
            "--into",
            "parent",
            "--dry-run",
        ])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "fold_incompatible");
    assert_eq!(
        refused["err"]["msg"],
        "cannot fold the profiles: profiles rights-only and rights-none differ \
         in `rights`; fold needs it equal in every input",
        "{refused}"
    );
    harness.stop().await;
}

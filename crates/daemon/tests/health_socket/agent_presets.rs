//! Profile presets (`#2148`): a preset file names a parent profile and a
//! selection of its options, and launches as the parent with that
//! selection under its own name and labels.
//!
//! Every scenario drives the public control socket over the profile fixtures
//! of [`super::agent_options`]: the parents run `/bin/sh` (never `$SHELL`) or
//! the resumable fixture runtime, which record the launch they received.

// Rust guideline compliant 2026-11-14

use super::agent_options::{
    argv, inspected_choices, launches, log_text, recorded_launch, resumable_profile, resumable_rig,
    selection, OptionRig, RECORDING_HEAD, RESUMABLE_OPTIONS, TWO_OPTIONS,
};
use super::*;

/// The preset `bold` of the parent `opts` selects `mode = "bold"`.
const BOLD_PRESET: &str = "[preset]\nparent = \"opts\"\noptions = { mode = \"bold\" }\n";

/// The launch the `opts` profile records with `mode = "bold"` and the
/// default `depth`: the choices' args in option-id order, then the env.
const BOLD_LAUNCH: &str = "--depth=1\n--mode\nbold\nMODE=bold-env\nDEPTH=\n";

/// A rig over the `opts` parent ([`RECORDING_HEAD`] with [`TWO_OPTIONS`])
/// and its `bold` preset.
async fn preset_rig(tag: &str) -> OptionRig {
    let rig = OptionRig::start(tag, false).await;
    rig.write_profile("opts", &format!("{RECORDING_HEAD}{TWO_OPTIONS}"));
    rig.write_profile("bold", BOLD_PRESET);
    rig
}

/// Row 1: a preset launches its parent with the preset's choices over the
/// parent's defaults. The session carries the preset's name and every
/// option's choice.
#[tokio::test]
async fn a_preset_launches_its_parent_with_its_choices() {
    let mut rig = preset_rig("agent-presets-launch").await;
    let (response, cwd) = rig.create("bold").await;
    let session: SessionInfo =
        serde_json::from_value(ok_payload(response)).expect("the preset launches");
    assert_eq!(session.agent, "bold");
    assert_eq!(
        session.agent_options,
        selection(&[("depth", "shallow"), ("mode", "bold")]),
        "the selection is complete"
    );
    assert_eq!(recorded_launch(&cwd).await, BOLD_LAUNCH);
    rig.finish().await;
}

/// Row 2: a preset session launches exactly as a session of its parent that
/// selects the same choices; only the agent name differs.
#[tokio::test]
async fn a_preset_launches_as_its_parent_with_the_same_selection() {
    let mut rig = preset_rig("agent-presets-equal").await;
    let (from_parent, parent_cwd) = rig.create_with("opts", &[("mode", "bold")], None).await;
    let (from_preset, preset_cwd) = rig.create("bold").await;
    let from_parent: SessionInfo =
        serde_json::from_value(ok_payload(from_parent)).expect("the parent launches");
    let from_preset: SessionInfo =
        serde_json::from_value(ok_payload(from_preset)).expect("the preset launches");
    assert_eq!(
        recorded_launch(&parent_cwd).await,
        recorded_launch(&preset_cwd).await,
        "the launches are equal"
    );
    assert_eq!(from_parent.agent_options, from_preset.agent_options);
    assert_eq!(
        (from_parent.agent.as_str(), from_preset.agent.as_str()),
        ("opts", "bold")
    );
    rig.finish().await;
}

/// Row 3: a request overrides the choices of an unlocked preset, option by
/// option.
#[tokio::test]
async fn a_request_overrides_the_choices_of_an_unlocked_preset() {
    let mut rig = preset_rig("agent-presets-override").await;
    let (response, cwd) = rig
        .create_with("bold", &[("mode", "calm"), ("depth", "deep")], None)
        .await;
    let session: SessionInfo =
        serde_json::from_value(ok_payload(response)).expect("the preset launches");
    assert_eq!(
        session.agent_options,
        selection(&[("depth", "deep"), ("mode", "calm")])
    );
    assert_eq!(
        recorded_launch(&cwd).await,
        "--depth=9\n--mode\ncalm\nMODE=calm-env\nDEPTH=deep-env\n"
    );
    rig.finish().await;
}

/// Row 4: a locked preset refuses any selection before anything is
/// created, naming the preset; without one it launches its own choices.
#[tokio::test]
async fn a_locked_preset_refuses_any_selection() {
    let mut rig = preset_rig("agent-presets-locked").await;
    rig.write_profile(
        "fixed",
        "[preset]\nparent = \"opts\"\nlocked = true\noptions = { mode = \"bold\" }\n",
    );
    for options in [&[("mode", "bold")][..], &[("depth", "deep")][..]] {
        let refused = rig.refused("fixed", options).await;
        assert_eq!(refused.code, "agent_options_locked", "{refused:?}");
        assert_eq!(
            refused.msg,
            "agent preset 'fixed' is locked; its options cannot be selected"
        );
    }
    let (response, cwd) = rig.create("fixed").await;
    ok_payload(response);
    assert_eq!(recorded_launch(&cwd).await, BOLD_LAUNCH);
    rig.finish().await;
}

/// Row 5: a preset names a full host profile and only choices it declares,
/// and holds no launch key; each broken preset is refused on load naming the
/// offending key, parent or ids.
#[tokio::test]
async fn a_malformed_preset_is_refused_on_load() {
    let mut rig = preset_rig("agent-presets-invalid").await;
    let cases = [
        (
            "nested",
            "[preset]\nparent = \"bold\"\n",
            "preset parent 'bold' is a preset; a preset cannot name another preset",
        ),
        (
            "runtime",
            "[preset]\nparent = \"shell\"\n",
            "preset parent 'shell' is not a host profile file",
        ),
        (
            "missing",
            "[preset]\nparent = \"absent\"\n",
            "preset parent 'absent' is not a host profile file",
        ),
        (
            "launching",
            "args = [\"--extra\"]\n[preset]\nparent = \"opts\"\n",
            "a preset holds only [preset], display_name and audience",
        ),
        (
            "unknown-option",
            "[preset]\nparent = \"opts\"\noptions = { speed = \"fast\" }\n",
            "preset option 'speed' is not declared by parent 'opts'",
        ),
        (
            "unknown-choice",
            "[preset]\nparent = \"opts\"\noptions = { mode = \"wild\" }\n",
            "preset option 'mode' selects choice 'wild', which parent 'opts' does not declare",
        ),
        (
            "unparented",
            "[preset]\nlocked = true\n",
            "line 1: missing field `parent`",
        ),
        (
            "scalar",
            "preset = \"opts\"\n",
            "preset must be a table that names the parent",
        ),
        (
            "bad-parent",
            "[preset]\nparent = \"../opts\"\n",
            "preset.parent is not a valid profile name",
        ),
    ];
    for (name, text, _) in &cases {
        rig.write_profile(name, text);
    }
    for (name, _, expected) in cases {
        assert_eq!(rig.load_error(name).await, expected, "{name}");
    }
    rig.finish().await;
}

/// The resumable rig with the preset `bolder` of its `work` profile written
/// from `preset` lines, a session of `bolder` launched with `mode = "bold"`
/// and stopped. Returns the rig, the package digest, the session and its
/// directory.
async fn stopped_preset_session(
    tag: &str,
    preset: &str,
) -> (OptionRig, package::PackageDigest, SessionInfo, PathBuf) {
    let (mut rig, digest) = resumable_rig(tag).await;
    rig.write_profile("bolder", preset);
    let (response, cwd) = rig.create("bolder").await;
    let session: SessionInfo =
        serde_json::from_value(ok_payload(response)).expect("the preset launches");
    assert_eq!(session.agent, "bolder");
    assert_eq!(
        session.agent_options,
        selection(&[("depth", "shallow"), ("mode", "bold")])
    );
    let first = launches(&cwd, 1).await;
    assert!(first[0].contains(&"MODE=bold-env".to_owned()), "{first:?}");
    rig.stop(&session.id).await;
    (rig, digest, session, cwd)
}

/// The `bolder` preset of `work` selecting `mode = "bold"`.
const BOLDER: &str = "[preset]\nparent = \"work\"\noptions = { mode = \"bold\" }\n";

/// Row 6: the preset's labels and `locked` are no launch input: editing them
/// resumes a preset session without a decision, with its recorded choices.
#[tokio::test]
async fn editing_a_presets_labels_or_lock_resumes_without_a_decision() {
    let (mut rig, _digest, session, cwd) =
        stopped_preset_session("agent-presets-resume-labels", BOLDER).await;
    rig.write_profile(
        "bolder",
        "display_name = \"Bolder\"\naudience = \"hidden\"\n\
         [preset]\nparent = \"work\"\nlocked = true\noptions = { mode = \"bold\" }\n",
    );
    ok_payload(rig.resume(&session.id, false).await);
    let blocks = launches(&cwd, 2).await;
    assert!(
        argv(&blocks[1])
            .windows(3)
            .any(|run| run == ["--depth=1", "--mode", "bold"]),
        "the relaunch keeps the recorded choices: {blocks:?}"
    );
    assert!(
        blocks[1].contains(&"MODE=bold-env".to_owned()),
        "{blocks:?}"
    );
    rig.finish().await;
}

/// Row 7: editing the body of the parent's choice a preset session selected
/// is a profile change: resume refuses with `agent_profile_changed`.
#[tokio::test]
async fn editing_the_parents_selected_choice_needs_a_decision() {
    let (mut rig, digest, session, cwd) =
        stopped_preset_session("agent-presets-resume-parent", BOLDER).await;
    let edited = RESUMABLE_OPTIONS.replace(
        "env = { PH_OPT_MODE = \"bold-env\" }",
        "env = { PH_OPT_MODE = \"bold-env-2\" }",
    );
    assert_ne!(edited, RESUMABLE_OPTIONS);
    rig.write_profile("work", &resumable_profile(&digest, &edited));
    let refused = err_payload(rig.resume(&session.id, false).await);
    assert_eq!(refused.code, "agent_profile_changed", "{refused:?}");
    assert_eq!(launches(&cwd, 1).await.len(), 1, "nothing relaunched");
    rig.finish().await;
}

/// Re-parenting a preset is a profile change even when the new parent's text
/// equals the old one's: resume refuses with `agent_profile_changed`.
#[tokio::test]
async fn re_parenting_a_preset_needs_a_decision() {
    let (mut rig, digest, session, _cwd) =
        stopped_preset_session("agent-presets-resume-reparent", BOLDER).await;
    rig.write_profile("twin", &resumable_profile(&digest, RESUMABLE_OPTIONS));
    rig.write_profile(
        "bolder",
        "[preset]\nparent = \"twin\"\noptions = { mode = \"bold\" }\n",
    );
    let refused = err_payload(rig.resume(&session.id, false).await);
    assert_eq!(refused.code, "agent_profile_changed", "{refused:?}");
    rig.finish().await;
}

/// Row 8: changing a preset's choices leaves an existing session alone (it
/// resumes with its recorded choices, without a decision), while a new
/// session of the preset launches the new choices.
#[tokio::test]
async fn changed_preset_choices_apply_to_new_sessions_only() {
    let (mut rig, _digest, session, cwd) =
        stopped_preset_session("agent-presets-resume-choices", BOLDER).await;
    rig.write_profile(
        "bolder",
        "[preset]\nparent = \"work\"\noptions = { mode = \"calm\" }\n",
    );
    ok_payload(rig.resume(&session.id, false).await);
    let blocks = launches(&cwd, 2).await;
    assert!(
        blocks[1].contains(&"MODE=bold-env".to_owned()),
        "the existing session keeps its recorded choice: {blocks:?}"
    );
    assert_eq!(
        rig.listed(&session.id).await.agent_options,
        selection(&[("depth", "shallow"), ("mode", "bold")])
    );
    let (response, fresh_cwd) = rig.create("bolder").await;
    let fresh: SessionInfo =
        serde_json::from_value(ok_payload(response)).expect("a new preset session");
    assert_eq!(
        fresh.agent_options,
        selection(&[("depth", "shallow"), ("mode", "calm")])
    );
    let fresh_blocks = launches(&fresh_cwd, 1).await;
    assert!(
        fresh_blocks[0].contains(&"MODE=calm-env".to_owned()),
        "a new session takes the new choice: {fresh_blocks:?}"
    );
    rig.finish().await;
}

/// Row 9: `package.bind_profile` (what `plugin profile migrate` sends)
/// refuses a preset, in a preview and a commit, before any rewrite, and
/// logs why: the preset carries no pin of its own. The preset still
/// launches afterwards.
#[tokio::test]
async fn binding_a_preset_is_refused_and_leaves_it_unchanged() {
    let log_dir = pohunek_test_support::tempdir().expect("log directory");
    let _capture = pohunek_daemon::logging::capture(&log_dir.path().join("daemon-logs"))
        .expect("the daemon log capture starts");
    let (mut rig, digest) = resumable_rig("agent-presets-bind").await;
    rig.write_profile("bolder", BOLDER);
    let path = rig.agents_dir.join("bolder.toml");
    let before = std::fs::read(&path).expect("read the preset");
    for dry_run in [true, false] {
        let response = exchange(
            &mut rig.control,
            &Request::make(
                "agent-presets-bind",
                method::PACKAGE_BIND_PROFILE,
                serde_json::json!({
                    "profile": "bolder",
                    "digest": digest,
                    "dry_run": dry_run
                }),
            ),
        )
        .await;
        let refused = err_payload(response);
        assert_eq!(
            refused.code, "package_profile_unusable",
            "dry_run={dry_run}: {refused:?}"
        );
        assert_eq!(
            std::fs::read(&path).expect("read the preset"),
            before,
            "dry_run={dry_run}: the preset is unchanged"
        );
    }
    assert_eq!(
        log_text(&log_dir.path().join("daemon-logs"))
            .matches("a preset has no package pin; migrate its parent")
            .count(),
        2,
        "each refusal names the preset rule in the daemon log"
    );
    let (response, _cwd) = rig.create("bolder").await;
    ok_payload(response);
    rig.finish().await;
}

/// Row 11: `host.inspect` lists a preset as an agent of its own, with the
/// preset's labels and the parent's options defaulting to the preset's
/// choices; a locked preset offers no options. A preset of a profile pinned
/// to a package version the host lacks is listed unavailable with it.
#[tokio::test]
async fn host_inspect_lists_a_preset_with_its_labels_and_choices() {
    let mut rig = OptionRig::start("agent-presets-inspect", true).await;
    rig.write_profile(
        "opts",
        &format!("display_name = \"Options\"\naudience = \"agent\"\n{RECORDING_HEAD}{TWO_OPTIONS}"),
    );
    rig.write_profile(
        "bold",
        &format!("display_name = \"Bold preset\"\naudience = \"hidden\"\n{BOLD_PRESET}"),
    );
    rig.write_profile(
        "fixed",
        "[preset]\nparent = \"opts\"\nlocked = true\noptions = { mode = \"bold\" }\n",
    );
    let missing =
        package::PackageDigest::parse(&format!("sha256:{}", "0".repeat(64))).expect("a digest");
    rig.write_profile("gone", &resumable_profile(&missing, ""));
    rig.write_profile("gone-preset", "[preset]\nparent = \"gone\"\n");
    let runtimes = rig.inspect_host().await;
    let entry = |agent: &str| {
        runtimes
            .iter()
            .find(|entry| entry.agent == agent)
            .unwrap_or_else(|| panic!("{agent} is inventoried: {runtimes:?}"))
            .clone()
    };
    let bold = entry("bold");
    assert_eq!(bold.display_name.as_deref(), Some("Bold preset"));
    assert_eq!(bold.audience, Some(protocol::ProfileAudience::Hidden));
    let defaults: Vec<(&str, &str)> = bold
        .options
        .iter()
        .map(|option| (option.id.as_str(), option.default.as_str()))
        .collect();
    assert_eq!(defaults, [("depth", "shallow"), ("mode", "bold")]);
    assert_eq!(
        inspected_choices(&runtimes, "bold", "mode")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["bold", "calm"],
        "the preset offers the parent's choices"
    );
    let parent = entry("opts");
    assert_eq!(parent.display_name.as_deref(), Some("Options"));
    assert_eq!(
        parent.options[1].default, "calm",
        "the parent keeps its own"
    );
    assert!(
        entry("fixed").options.is_empty(),
        "a locked preset offers none"
    );
    assert!(entry("fixed").available);
    assert!(!entry("gone").available);
    assert!(!entry("gone-preset").available);
    rig.finish().await;
}

/// Row 11b: `session.list` shows a preset session by the preset's own
/// `display_name`, not its parent's.
#[tokio::test]
async fn a_preset_session_lists_with_the_presets_display_name() {
    let mut rig = OptionRig::start("agent-presets-label", false).await;
    rig.write_profile(
        "opts",
        &format!("display_name = \"Options\"\n{RECORDING_HEAD}{TWO_OPTIONS}"),
    );
    rig.write_profile(
        "bold",
        &format!("display_name = \"Bold preset\"\n{BOLD_PRESET}"),
    );
    let (response, _cwd) = rig.create("bold").await;
    let session: SessionInfo =
        serde_json::from_value(ok_payload(response)).expect("the preset launches");
    assert_eq!(
        rig.listed(&session.id).await.agent_display_name.as_deref(),
        Some("Bold preset")
    );
    rig.finish().await;
}

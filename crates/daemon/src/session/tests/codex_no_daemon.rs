//! The Codex hook handler adds `--no-daemon` to every Codex launch, driven
//! through a fixture agent that logs its argv: a plain launch, profiles with
//! and without their own arguments, and resumes of sessions whose frozen
//! arguments never held the flag.

// Rust guideline compliant 2026-10-27

use super::native_recovery::Rig;
use super::*;

/// Synthetic conversation reference that cannot name an operator conversation.
const NATIVE_ID: &str = "fixture-no-daemon-conversation";

/// The flag the handler contributes.
const FLAG: &str = "--no-daemon";

/// The argv lines the fixture agent logged, without its environment lines.
fn argv_lines(rig: &Rig) -> Vec<String> {
    fs::read_to_string(&rig.launches)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.starts_with("env="))
        .map(str::to_owned)
        .collect()
}

/// Waits until the fixture agent has logged `count` launches.
async fn wait_for_launches(rig: &Rig, count: usize) {
    wait_until("the launch to log its argv", || async {
        (argv_lines(rig).len() == count).then_some(())
    })
    .await;
}

/// Reports the conversation, settles it and stops the session, so it can resume.
async fn stop_with_reference(rig: &Rig) {
    let snapshot = rig.report(NATIVE_ID).await;
    rig.settle(&snapshot).await;
    rig.finish().await;
}

/// Creates the session, resumes it twice and returns the logged argv lines.
async fn create_and_resume_twice(rig: &Rig) -> Vec<String> {
    stop_with_reference(rig).await;
    for round in 2..=3 {
        rig.registry
            .resume(&rig.session.id)
            .await
            .expect("resume the conversation");
        wait_for_launches(rig, round).await;
        rig.finish().await;
    }
    argv_lines(rig)
}

#[tokio::test]
async fn a_plain_codex_session_gets_the_flag_on_create_and_on_every_resume_exactly_once() {
    let rig = Rig::new("codex-no-daemon-plain", true).await;
    let lines = create_and_resume_twice(&rig).await;
    assert_eq!(
        lines,
        [
            FLAG.to_owned(),
            format!("{FLAG} resume {NATIVE_ID}"),
            format!("{FLAG} resume {NATIVE_ID}"),
        ]
    );
    let record = format!("{:?}", rig.durable_record());
    assert!(
        !record.contains(FLAG),
        "the stored launch snapshot holds no flag, so a session created before the handler contributed it resumes with it: {record}"
    );
    rig.remove().await;
}

#[tokio::test]
async fn a_profile_without_the_flag_gets_it_after_its_own_arguments_and_before_the_resume() {
    let rig = Rig::profiled("codex-no-daemon-profile", &[]).await;
    let lines = create_and_resume_twice(&rig).await;
    assert_eq!(
        lines,
        [
            FLAG.to_owned(),
            format!("{FLAG} resume {NATIVE_ID}"),
            format!("{FLAG} resume {NATIVE_ID}"),
        ]
    );
    rig.remove().await;
}

#[tokio::test]
async fn a_profile_with_its_own_arguments_keeps_them_ahead_of_the_flag() {
    let rig = Rig::profiled("codex-no-daemon-own-args", &["--model", "x"]).await;
    let lines = create_and_resume_twice(&rig).await;
    assert_eq!(
        lines,
        [
            format!("--model x {FLAG}"),
            format!("--model x {FLAG} resume {NATIVE_ID}"),
            format!("--model x {FLAG} resume {NATIVE_ID}"),
        ]
    );
    rig.remove().await;
}

#[tokio::test]
async fn a_profile_that_already_passes_the_flag_is_not_given_a_second_copy() {
    let rig = Rig::profiled("codex-no-daemon-own-flag", &[FLAG]).await;
    let lines = create_and_resume_twice(&rig).await;
    assert_eq!(
        lines,
        [
            FLAG.to_owned(),
            format!("{FLAG} resume {NATIVE_ID}"),
            format!("{FLAG} resume {NATIVE_ID}"),
        ]
    );
    rig.remove().await;
}

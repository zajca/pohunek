//! `session.screen` links are trimmed to the configured response budget: a
//! screen full of links never turns the observation into an error.

// Rust guideline compliant 2026-11-26

#![cfg(target_os = "linux")]

#[path = "support/screen_session.rs"]
mod screen_session;

use screen_session::{links, screen_of, Setup};

/// Serialized bytes the budget leaves beyond the screen without links: room
/// for the `links` member and a couple of entries, not for all ten.
const LINK_ROOM_BYTES: usize = 160;

#[tokio::test]
async fn links_that_do_not_fit_the_response_budget_are_trimmed_not_an_error() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let screen_text = |scheme: &str| {
        (0..10)
            .map(|index| format!("{scheme}://budget.test/{index}\r\n"))
            .collect::<Vec<_>>()
            .concat()
    };
    // The same screen with a scheme the matcher ignores: no links, same size.
    let baseline = screen_of(screen_text("hxxp").as_bytes(), Setup::default()).await;
    assert!(baseline.get("links").is_none());
    let budget = serde_json::to_vec(&baseline).expect("serialize").len() + LINK_ROOM_BYTES;

    let screen = screen_of(
        screen_text("http").as_bytes(),
        Setup {
            tweak: Box::new(move |config| config.observation_screen_bytes = budget),
            ..Setup::default()
        },
    )
    .await;

    let reported = links(&screen);
    assert!(
        (1..10).contains(&reported.len()),
        "some but not all links fit: {}",
        reported.len()
    );
    assert_eq!(reported[0]["url"], "http://budget.test/0");
    assert!(serde_json::to_vec(&screen).expect("serialize").len() <= budget);
}

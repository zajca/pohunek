//! `session.screen` links through a real session: worker, PTY and registry.
//!
//! Each scenario runs a shell that writes fixture bytes to the terminal, waits
//! for a sentinel line to show on the screen, and asserts on the serialized
//! `session.screen` result: the wire form a client sees. The URL query strings
//! in the fixtures are synthetic.

// Rust guideline compliant 2026-11-26

#![cfg(target_os = "linux")]

#[path = "support/screen_session.rs"]
mod screen_session;

use screen_session::{links, screen_of, screen_of_script, Setup};
use serde_json::{json, Value};

fn urls(screen: &Value) -> Vec<String> {
    links(screen)
        .iter()
        .map(|link| link["url"].as_str().expect("link url").to_owned())
        .collect()
}

fn link(url: &str, source: &str, row: u16, col: u16) -> Value {
    json!({"url": url, "source": source, "row": row, "col": col})
}

#[tokio::test]
async fn text_urls_are_reported_with_their_cell_and_trimmed_of_prose_punctuation() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let screen = screen_of(
        b"dev server: http://localhost:5173/app?token=abc.\r\n\
          see (https://example.test/a_(b)) and ftp://nope.test/x javascript:alert(1)\r\n\
          xhttp://glued.test HTTP://UPPER.test/p\r\n",
        Setup::default(),
    )
    .await;

    assert_eq!(
        links(&screen),
        vec![
            link("http://localhost:5173/app?token=abc", "text", 0, 12),
            link("https://example.test/a_(b)", "text", 1, 5),
            link("HTTP://UPPER.test/p", "text", 2, 19),
        ]
    );
}

#[tokio::test]
async fn a_url_wrapped_at_the_right_margin_is_one_link_at_its_first_cell() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let url = format!("http://localhost:5173/{}", "a".repeat(80));
    let screen = screen_of(
        format!("go {url}\r\n").as_bytes(),
        Setup {
            cols: 40,
            ..Setup::default()
        },
    )
    .await;

    assert_eq!(links(&screen), vec![link(&url, "text", 0, 3)]);
}

#[tokio::test]
async fn a_url_split_by_a_hard_line_break_is_not_joined() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let screen = screen_of(b"http://localhost:51\r\n73/x\r\n", Setup::default()).await;

    assert_eq!(
        links(&screen),
        vec![link("http://localhost:51", "text", 0, 0)]
    );
}

#[tokio::test]
async fn an_oversized_url_is_omitted_whole_and_later_links_still_report() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let mut bytes = format!("http://big.test/{}\r\n", "a".repeat(3_000)).into_bytes();
    bytes.extend_from_slice(
        format!(
            "\x1b]8;;http://big8.test/{}\x1b\\big\x1b]8;;\x1b\\\r\n",
            "b".repeat(3_000)
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(b"http://small.test/ok\r\n");
    let screen = screen_of(
        &bytes,
        Setup {
            cols: 500,
            ..Setup::default()
        },
    )
    .await;

    let reported = links(&screen);
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert_eq!(reported[0]["url"], "http://small.test/ok");
}

#[tokio::test]
async fn links_beyond_the_count_cap_are_dropped_in_reading_order() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let bytes = (0..70)
        .map(|index| format!("http://host.test/{index}\r\n"))
        .collect::<Vec<_>>()
        .concat();
    let screen = screen_of(
        bytes.as_bytes(),
        Setup {
            rows: 100,
            ..Setup::default()
        },
    )
    .await;

    let reported = links(&screen);
    assert_eq!(reported.len(), 64);
    assert_eq!(reported[0], link("http://host.test/0", "text", 0, 0));
    assert_eq!(reported[63], link("http://host.test/63", "text", 63, 0));
}

#[tokio::test]
async fn osc8_anchors_with_and_without_an_id_report_the_first_cell_of_their_text() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let screen = screen_of(
        b"\x1b]8;;http://osc8.test/a\x1b\\click\x1b]8;;\x1b\\ then \
          \x1b]8;id=x1;https://osc8.test/b\x07second\x1b]8;;\x07\r\n",
        Setup::default(),
    )
    .await;

    assert_eq!(
        links(&screen),
        vec![
            link("http://osc8.test/a", "osc8", 0, 0),
            link("https://osc8.test/b", "osc8", 0, 11),
        ]
    );
}

#[tokio::test]
async fn an_osc8_anchor_whose_text_is_its_url_is_reported_once_as_osc8() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let screen = screen_of(
        b"\x1b]8;;http://same.test/x\x1b\\http://same.test/x\x1b]8;;\x1b\\\r\n",
        Setup::default(),
    )
    .await;

    assert_eq!(
        links(&screen),
        vec![link("http://same.test/x", "osc8", 0, 0)]
    );
}

#[tokio::test]
async fn malformed_osc8_is_ignored_and_the_terminal_keeps_working() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x1b]8;http://nosep.test\x07noSep ");
    bytes.extend_from_slice(b"\x1b]8;;http://ctl\x01.test\x07ctlText ");
    bytes.extend_from_slice(b"\x1b]8;;\xff\xfe.test\x07badUtf ");
    bytes.extend_from_slice(b"\x1b]8;;\x07 plain http://after.test/ok\r\n");
    let screen = screen_of(&bytes, Setup::default()).await;

    let reported = links(&screen);
    assert!(
        reported.iter().all(|link| link["source"] == "text"),
        "{reported:?}"
    );
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert_eq!(reported[0]["url"], "http://after.test/ok");
}

#[tokio::test]
async fn an_osc8_anchor_without_visible_text_reports_nothing() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let screen = screen_of(
        b"\x1b]8;;http://empty.test\x1b\\\x1b]8;;\x1b\\\
          \x1b]8;;http://blank.test\x1b\\   \x1b]8;;\x1b\\ end\r\n\
          http://ok.test/\r\n",
        Setup::default(),
    )
    .await;

    assert_eq!(urls(&screen), ["http://ok.test/"]);
}

#[tokio::test]
async fn an_osc8_anchor_still_open_ends_at_the_cursor() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let screen = screen_of(
        b"\x1b]8;;http://open.test/p\x1b\\still open",
        Setup::default(),
    )
    .await;

    assert_eq!(
        links(&screen),
        vec![link("http://open.test/p", "osc8", 0, 0)]
    );
}

#[tokio::test]
async fn an_osc8_anchor_is_dropped_once_its_text_scrolls_clears_or_is_overwritten() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let anchor = "\x1b]8;;http://gone.test/p\x1b\\click\x1b]8;;\x1b\\";
    let keep = "http://keep.test/k\r\n";
    let scrolled = format!("{anchor}{}\r\n{keep}", "\r\nfiller".repeat(30));
    let cleared = format!("{anchor}\x1b[2J\x1b[H{keep}");
    let overwritten = format!("{anchor}\x1b[1;1HXXXXX\r\n{keep}");
    for (name, bytes) in [
        ("scrolled", scrolled),
        ("cleared", cleared),
        ("overwritten", overwritten),
    ] {
        let screen = screen_of(
            bytes.as_bytes(),
            Setup {
                rows: 10,
                ..Setup::default()
            },
        )
        .await;
        assert_eq!(urls(&screen), ["http://keep.test/k"], "{name}");
    }
}

#[tokio::test]
async fn an_osc8_sequence_split_across_output_chunks_reports_like_an_unsplit_one() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    // The pause lets the worker read the two halves as separate chunks; the
    // assertion does not depend on it, since the unsplit result is the same.
    let screen = screen_of_script(
        "printf '\\033]8;;http://split.test/a'; sleep 0.3; \
         printf '\\033\\\\split text\\033]8;;\\033\\\\\\r\\n'",
        Setup::default(),
    )
    .await;

    assert_eq!(
        links(&screen),
        vec![link("http://split.test/a", "osc8", 0, 0)]
    );
}

#[tokio::test]
async fn the_links_field_is_present_only_on_a_screen_that_has_links() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let with_link = screen_of(b"open http://localhost:5173/\r\n", Setup::default()).await;
    assert_eq!(urls(&with_link), ["http://localhost:5173/"]);

    let screen = screen_of(b"nothing to open here\r\n", Setup::default()).await;

    assert!(screen.get("links").is_none(), "{screen}");
    assert!(screen["visible_lines"][0]
        .as_str()
        .is_some_and(|line| line.contains("nothing to open here")));
}

#[tokio::test]
async fn an_osc8_anchor_opened_at_a_pending_wrap_starts_on_the_next_row() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let screen = screen_of(
        b"0123456789\x1b]8;;https://wrap.test/x\x1b\\link\x1b]8;;\x1b\\\r\n",
        Setup {
            cols: 10,
            ..Setup::default()
        },
    )
    .await;

    assert_eq!(
        links(&screen),
        vec![link("https://wrap.test/x", "osc8", 1, 0)]
    );
}

#[tokio::test]
async fn a_redrawn_osc8_anchor_is_reported_once_with_its_latest_url() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let screen = screen_of(
        b"\x1b]8;;file:///a/README\x1b\\README\x1b]8;;\x1b\\\x1b[2J\x1b[H\
          \x1b]8;;file:///b/README\x1b\\README\x1b]8;;\x1b\\\r\n",
        Setup::default(),
    )
    .await;

    assert_eq!(links(&screen), vec![link("file:///b/README", "osc8", 0, 0)]);
}

/// A tracing writer that keeps what the daemon logs.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn a_reported_link_is_counted_in_the_log_and_its_query_string_is_not_logged() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let captured = Captured::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    )
    .expect("no other subscriber in this process");

    let screen = screen_of(
        b"open http://localhost:5173/?token=query-secret-marker\r\n",
        Setup::default(),
    )
    .await;
    assert_eq!(
        urls(&screen),
        ["http://localhost:5173/?token=query-secret-marker"]
    );

    let log = String::from_utf8(captured.0.lock().expect("log buffer").clone()).expect("utf-8 log");
    assert!(
        log.contains("links=1"),
        "the screen log counts the link: {log}"
    );
    assert!(
        !log.contains("query-secret-marker"),
        "the query string is not logged: {log}"
    );
}

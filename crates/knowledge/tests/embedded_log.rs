//! The bundle compiled into the binary carries the unreleased log fragments
//! inside its assembled `log.md`, not as files of their own.

// Rust guideline compliant 2026-10-14

use std::fs;

use knowledge::log::{read_fragments, FRAGMENT_DIR, LOG_FILE};
use knowledge::{bundle_index, embedded_bundle};

#[test]
fn embedded_log_shows_every_committed_fragment_as_an_unreleased_section() {
    let source = pohunek_test_support::workspace_root().join("docs/knowledge");
    let fragments = read_fragments(&source.join(FRAGMENT_DIR)).expect("committed fragments parse");
    let log = embedded_bundle()
        .get_text(LOG_FILE)
        .expect("the embedded bundle has a log");

    let mut last = 0;
    for fragment in &fragments {
        let heading = format!("## Unreleased ({})\n", fragment.label());
        let Some(offset) = log[last..].find(&heading) else {
            panic!("the embedded log lacks, or misorders, `{heading}`");
        };
        let position = last + offset;
        last = position + heading.len();
    }
    let committed = fs::read_to_string(source.join(LOG_FILE)).expect("committed log");
    let released = committed
        .find("\n## ")
        .map(|offset| &committed[offset + 1..])
        .expect("the committed log has release sections");
    assert!(
        log.ends_with(released),
        "the release sections follow the fragments unchanged"
    );
}

#[test]
fn embedded_bundle_has_no_fragment_files_and_indexes_without_them() {
    let target = pohunek_test_support::tempdir_with_prefix("knowledge-embedded-")
        .expect("private extraction dir");
    embedded_bundle()
        .extract(target.path())
        .expect("the embedded bundle extracts");

    assert!(!target.path().join(FRAGMENT_DIR).exists());
    bundle_index().expect("the embedded bundle indexes");
}

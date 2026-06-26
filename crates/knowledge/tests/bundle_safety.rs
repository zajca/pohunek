use knowledge::embedded_bundle;

const HOOK_CONFIRMATION_RULE: &str = "explicit per-file confirmation, independent of `--yes`";
const NON_INTERACTIVE_RULE: &str =
    "Non-interactive contexts must quarantine proposed hook content instead of enabling it.";
const REPO_HOOK_QUARANTINE: &str = ".pohunek/quarantine/hooks/<event>.pending";
const HOST_HOOK_QUARANTINE: &str = "~/.config/pohunek/quarantine/hooks/<event>.pending";

fn embedded_markdown(path: &str) -> &'static str {
    embedded_bundle()
        .get_file(path)
        .unwrap_or_else(|| panic!("embedded bundle contains {path}"))
        .contents_utf8()
        .unwrap_or_else(|| panic!("{path} is utf-8 markdown"))
}

fn normalize_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn embedded_bundle_documents_hook_write_hard_gate() {
    for path in ["assistant/system.md", "safety/repo-pohunek.md"] {
        let body = normalize_whitespace(embedded_markdown(path));

        assert!(
            body.contains(HOOK_CONFIRMATION_RULE),
            "{path} must require explicit hook confirmation independent of --yes"
        );
        assert!(
            body.contains(NON_INTERACTIVE_RULE),
            "{path} must quarantine hook writes in non-interactive contexts"
        );
        assert!(
            body.contains(REPO_HOOK_QUARANTINE),
            "{path} must document repo-local hook quarantine"
        );
        assert!(
            body.contains(HOST_HOOK_QUARANTINE),
            "{path} must document host-global hook quarantine"
        );
    }
}

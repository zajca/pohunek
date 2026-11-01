//! Drift checks between hand-maintained docs and their sources of truth, driven
//! through `xtask::check_doc_drift` against fixture checkouts and the real
//! `pohunek` command tree.
//!
//! Each fixture is a small checkout whose `docs/cli.md` is generated from the
//! live command tree, so the scenarios hold for every command the CLI has.

// Rust guideline compliant 2026-10-20

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;

use clap::Command;

/// Hidden entry points of the service manager units; they need no reference row
/// and take no `--json`.
const HIDDEN_PREFIX: &str = "__";

/// A fixture checkout.
struct Checkout {
    root: tempfile::TempDir,
}

/// One executable command of the tree and whether it takes `--json`.
struct Leaf {
    path: String,
    accepts_json: bool,
}

fn leaves(command: &Command, prefix: &mut Vec<String>, found: &mut Vec<Leaf>) {
    let children: Vec<&Command> = command
        .get_subcommands()
        .filter(|child| child.get_name() != "help")
        .collect();
    if children.is_empty() && !prefix.is_empty() {
        found.push(Leaf {
            path: prefix.join(" "),
            accepts_json: command
                .get_arguments()
                .any(|argument| argument.get_long() == Some("json")),
        });
    }
    for child in children {
        prefix.push(child.get_name().to_owned());
        leaves(child, prefix, found);
        prefix.pop();
    }
}

/// The reviewed leaves of the live tree: every executable command except the
/// hidden entry points.
fn tree_leaves() -> Vec<Leaf> {
    let mut tree = cli::command();
    tree.build();
    let mut found = Vec::new();
    leaves(&tree, &mut Vec::new(), &mut found);
    found.retain(|leaf| {
        !leaf
            .path
            .split(' ')
            .any(|word| word.starts_with(HIDDEN_PREFIX))
    });
    found
}

/// A `docs/cli.md` with one row per leaf of the live tree and the list of the
/// commands without `--json`, minus the rows named in `omit` and plus the
/// names in `also_listed` in that list.
fn reference(omit: &[&str], also_listed: &[&str]) -> String {
    let mut page = String::from("# CLI guide\n\n| Command | What it does |\n|---|---|\n");
    let mut without_json: BTreeSet<String> = BTreeSet::new();
    for leaf in tree_leaves() {
        if !leaf.accepts_json {
            without_json.insert(leaf.path.clone());
        }
        if !omit.contains(&leaf.path.as_str()) {
            writeln!(page, "| `pohunek {}` | Documented. |", leaf.path).expect("write to a string");
        }
    }
    without_json.extend(also_listed.iter().map(|name| (*name).to_owned()));
    let listed: Vec<String> = without_json
        .iter()
        .map(|name| format!("`{name}`"))
        .collect();
    write!(
        page,
        "\nThe commands without `--json` are {}.\n",
        listed.join(", ")
    )
    .expect("write to a string");
    page
}

/// The reference without its list of commands without `--json`.
fn reference_without_json_list() -> String {
    let page = reference(&[], &[]);
    let cut = page
        .find("The commands without")
        .expect("generated reference has the list");
    page[..cut].to_owned()
}

impl Checkout {
    /// A checkout in which every check passes.
    fn consistent() -> Self {
        let checkout = Self {
            root: pohunek_test_support::tempdir_with_prefix("pxdd-").expect("fixture root"),
        };
        checkout.write("Cargo.toml", "[workspace]\nmembers = [\"crates/alpha\"]\n");
        checkout.write("runtime-packages/pi/package.toml", "name = \"pi\"\n");
        checkout.write("compat/pi/lock.json", "{}\n");
        checkout.write("compat/matrix.json", "{}\n");
        checkout.write(
            "docs/development.md",
            "| Crate | Role |\n|---|---|\n| `crates/alpha` | The only crate. |\n",
        );
        checkout.write(
            "docs/install.md",
            "The archive holds `runtime/packages/pi.tar.zst`.\n",
        );
        checkout.write("docs/features.md", "Features.\n");
        checkout.write("README.md", "Readme.\n");
        checkout.write(
            "docs/README.md",
            "[CLI](cli.md) [Dev](development.md) [Install](install.md) [Features](features.md)\n",
        );
        checkout.write(
            "docs/knowledge/index.md",
            "# Index\n\n[map](assistant/source-map.md)\n",
        );
        checkout.write(
            "docs/knowledge/assistant/source-map.md",
            "# Map\n\n- `crates/alpha/src/lib.rs`\n",
        );
        checkout.write("docs/cli.md", &reference(&[], &[]));
        checkout
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.root.path().join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, content).expect("write fixture file");
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.root.path().join(relative)).expect("read fixture file")
    }

    fn failed_checks(&self) -> Vec<&'static str> {
        xtask::check_doc_drift(self.root.path()).expect("the checks run")
    }

    /// Writes `docs/knowledge/guides/page.md` with `body` and reports the failed checks.
    fn failed_with_page(&self, body: &str) -> Vec<&'static str> {
        self.write("docs/knowledge/guides/page.md", body);
        self.write(
            "docs/knowledge/index.md",
            "# Index\n\n[page](guides/page.md) [map](assistant/source-map.md)\n",
        );
        self.failed_checks()
    }
}

#[test]
fn a_consistent_checkout_passes_every_check() {
    assert_eq!(Checkout::consistent().failed_checks(), Vec::<&str>::new());
}

#[test]
fn examples_in_every_documented_form_are_accepted() {
    let page = [
        "`pohunek session list [--json]`",
        "`pohunek --host buildbox session list --json`",
        "`pohunek session screen <target> --json`",
        "`pohunek session list \\| screen`",
        "`pohunek notifications list|watch --all-hosts`",
        "`pohunek session rename / stop / rm <target>`",
        "`pohunek session new --name \"explain --removed behavior\"`",
        "`pohunek service status && pohunek health`",
        "`pohunek health --json | jq .`",
        "`pohunek workflow run --json acme.work overview --since 1d`",
        "`pohunek session input s-01J -- --literal`",
        "`pohunek integration ... --agent hermes`",
        "`pohunek assistant diagnose this problem`",
        "`pohunek session new --cwd / --name demo`",
        "```sh\npohunek health   # inspect --removed-option && pohunek removed-command\n```",
        "`pohunek session new --name=\"a|--removed|b\"`",
        "`pohunek notifications|session list --json`",
        "`pohunek session input s-01J \"a | pohunek removed-command / x\"`",
        "```sh\npohunek session list \\\n  --json\n```",
    ]
    .join("\n\n");
    assert_eq!(
        Checkout::consistent().failed_with_page(&page),
        Vec::<&str>::new()
    );
}

#[test]
fn each_drifted_example_fails_the_command_check() {
    let drifted = [
        (
            "a command under the wrong group",
            "`pohunek integration hermes install`",
        ),
        ("a removed flag", "`pohunek session list --removed-flag`"),
        (
            "a removed flag in optional brackets",
            "`pohunek session list [--removed-flag]`",
        ),
        (
            "a removed flag among alternatives",
            "`pohunek session list [--json \\| --removed-flag]`",
        ),
        (
            "a stale sibling",
            "`pohunek session rename / removed-command`",
        ),
        (
            "a stale sibling flag",
            "`pohunek session rename / stop --removed-flag`",
        ),
        (
            "a stale second command of a shell line",
            "`pohunek health && pohunek removed-command`",
        ),
        (
            "a removed flag on a continuation line",
            "```sh\npohunek session list \\\n  --removed-flag\n```",
        ),
        (
            "a removed flag before passthrough arguments",
            "`pohunek workflow run --removed acme.work overview`",
        ),
        (
            "a stale alternative",
            "`pohunek notifications list|removed-command`",
        ),
        (
            "a removed flag after alternatives",
            "`pohunek notifications list|watch --removed-flag`",
        ),
        (
            "a removed flag after a positional alternative",
            "`pohunek session input s-01J yes|no --removed-flag`",
        ),
        (
            "a quoted separator is not a command",
            "`pohunek session input s-01J \"a | b\" --removed-flag`",
        ),
        (
            "a subcommand flag on a global position",
            "`pohunek health --version`",
        ),
    ];
    for (label, example) in drifted {
        assert_eq!(
            Checkout::consistent().failed_with_page(example),
            vec!["doc-commands"],
            "{label}: {example}"
        );
    }
}

#[test]
fn retired_pages_and_the_history_log_are_not_held_to_the_current_tree() {
    let checkout = Checkout::consistent();
    checkout.write(
        "docs/knowledge/guides/old.md",
        "# Old\n\nStatus: historical\n\n`pohunek integration hermes install`\n",
    );
    checkout.write(
        "docs/knowledge/log.md",
        "# Log\n\n- Ran `pohunek integration hermes install`.\n",
    );
    checkout.write(
        "docs/knowledge/index.md",
        "# Index\n\n[old](guides/old.md) [log](log.md) [map](assistant/source-map.md)\n",
    );
    assert_eq!(checkout.failed_checks(), Vec::<&str>::new());
}

#[test]
fn a_leaf_command_without_a_reference_row_fails_coverage() {
    let checkout = Checkout::consistent();
    checkout.write("docs/cli.md", &reference(&["relay login"], &[]));
    assert_eq!(checkout.failed_checks(), vec!["cli-reference-coverage"]);
}

#[test]
fn sibling_shorthand_rows_cover_every_named_command() {
    let checkout = Checkout::consistent();
    let mut page = reference(&["session rename", "session stop"], &[]);
    page = page.replacen(
        "\nThe commands without",
        "\n| `pohunek session rename / stop <target>` | Rename or stop. |\n\nThe commands without",
        1,
    );
    checkout.write("docs/cli.md", &page);
    assert_eq!(checkout.failed_checks(), Vec::<&str>::new());
}

#[test]
fn the_json_exception_list_must_match_the_tree() {
    let missing = Checkout::consistent();
    let page = missing
        .read("docs/cli.md")
        .replace("`service lock`, ", "")
        .replace(", `service lock`", "");
    assert_ne!(
        page,
        missing.read("docs/cli.md"),
        "the fixture lists service lock"
    );
    missing.write("docs/cli.md", &page);
    assert_eq!(missing.failed_checks(), vec!["json-exceptions"]);

    let extra = Checkout::consistent();
    extra.write("docs/cli.md", &reference(&[], &["session list"]));
    assert_eq!(extra.failed_checks(), vec!["json-exceptions"]);

    let absent = Checkout::consistent();
    absent.write("docs/cli.md", &reference_without_json_list());
    assert_eq!(absent.failed_checks(), vec!["json-exceptions"]);
}

#[test]
fn the_crate_table_must_list_exactly_the_workspace_crates() {
    let added = Checkout::consistent();
    added.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\n",
    );
    assert_eq!(
        added.failed_checks(),
        vec!["crate-table", "source-map-crates"]
    );

    let removed = Checkout::consistent();
    removed.write("Cargo.toml", "[workspace]\nmembers = []\n");
    assert_eq!(removed.failed_checks(), vec!["crate-table"]);
}

#[test]
fn the_source_map_must_name_every_workspace_crate_by_whole_path_segment() {
    let unmapped = Checkout::consistent();
    unmapped.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/alpha\", \"crates/relay\"]\n",
    );
    unmapped.write(
        "docs/development.md",
        "| Crate | Role |\n|---|---|\n| `crates/alpha` | A. |\n| `crates/relay` | R. |\n",
    );
    // A longer crate name that starts with `relay` does not map `crates/relay`.
    unmapped.write(
        "docs/knowledge/assistant/source-map.md",
        "- `crates/alpha/src/lib.rs`\n- `crates/relay-client/src/`\n",
    );
    assert_eq!(unmapped.failed_checks(), vec!["source-map-crates"]);

    let mapped = Checkout::consistent();
    mapped.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/alpha\", \"crates/relay\"]\n",
    );
    mapped.write(
        "docs/development.md",
        "| Crate | Role |\n|---|---|\n| `crates/alpha` | A. |\n| `crates/relay` | R. |\n",
    );
    mapped.write(
        "docs/knowledge/assistant/source-map.md",
        "- `crates/alpha/src/lib.rs`\n- `crates/relay/` — the relay.\n",
    );
    assert_eq!(mapped.failed_checks(), Vec::<&str>::new());
}

#[test]
fn the_package_lists_must_match_runtime_packages() {
    let added = Checkout::consistent();
    added.write("runtime-packages/codex/package.toml", "name = \"codex\"\n");
    assert_eq!(added.failed_checks(), vec!["package-lists"]);

    let stale = Checkout::consistent();
    stale.write(
        "docs/install.md",
        "`runtime/packages/pi.tar.zst` `runtime/packages/gone.tar.zst`\n",
    );
    assert_eq!(stale.failed_checks(), vec!["package-lists"]);

    let counted = Checkout::consistent();
    counted.write("README.md", "The three packages ship.\n");
    assert_eq!(counted.failed_checks(), vec!["package-lists"]);

    let numeral = Checkout::consistent();
    numeral.write("README.md", "The 3 packages ship; the one package is pi.\n");
    assert_eq!(numeral.failed_checks(), Vec::<&str>::new());
}

#[test]
fn only_the_four_official_runtimes_may_have_a_plugin_directory() {
    let official = Checkout::consistent();
    for name in ["claude", "codex", "hermes"] {
        official.write(&format!("runtime-packages/{name}/package.toml"), "x = 1\n");
        official.write(&format!("compat/{name}/lock.json"), "{}\n");
    }
    official.write(
        "docs/install.md",
        "`runtime/packages/pi.tar.zst` `runtime/packages/claude.tar.zst` \
         `runtime/packages/codex.tar.zst` `runtime/packages/hermes.tar.zst`\n",
    );
    assert_eq!(official.failed_checks(), Vec::<&str>::new());

    let package = Checkout::consistent();
    package.write("runtime-packages/newthing/package.toml", "x = 1\n");
    package.write(
        "docs/install.md",
        "`runtime/packages/pi.tar.zst` `runtime/packages/newthing.tar.zst`\n",
    );
    assert_eq!(package.failed_checks(), vec!["plugin-directories"]);

    let compat = Checkout::consistent();
    compat.write("compat/newthing/lock.json", "{}\n");
    assert_eq!(compat.failed_checks(), vec!["plugin-directories"]);

    let hidden = Checkout::consistent();
    hidden.write("compat/.newthing/lock.json", "{}\n");
    assert_eq!(hidden.failed_checks(), vec!["plugin-directories"]);

    let linked = Checkout::consistent();
    std::os::unix::fs::symlink("pi", linked.root.path().join("compat/newthing"))
        .expect("link a plugin directory");
    assert_eq!(linked.failed_checks(), vec!["plugin-directories"]);
}

#[test]
fn every_reference_and_knowledge_page_must_be_reachable_from_an_index() {
    let orphan = Checkout::consistent();
    orphan.write("docs/ROADMAP.md", "# Roadmap\n");
    orphan.write("docs/knowledge/guides/lonely.md", "# Lonely\n");
    assert_eq!(orphan.failed_checks(), vec!["doc-index"]);

    let encoded = Checkout::consistent();
    encoded.write("docs/review-notes.md", "# Notes\n");
    encoded.write(
        "docs/README.md",
        "[CLI](cli.md) [Dev](development.md) [Install](install.md) [Features](features.md)\n\
         [Notes](review%2Dnotes.md)\n",
    );
    assert_eq!(encoded.failed_checks(), Vec::<&str>::new());

    let linked = Checkout::consistent();
    linked.write("docs/ROADMAP.md", "# Roadmap\n");
    linked.write("docs/knowledge/guides/lonely.md", "# Lonely\n");
    linked.write(
        "docs/README.md",
        "[CLI](cli.md) [Dev](development.md) [Install](install.md) [Features](features.md)\n\
         [Roadmap](ROADMAP.md) [Lonely][l]\n\n[l]: knowledge/guides/lonely.md \"Lonely\"\n",
    );
    assert_eq!(linked.failed_checks(), Vec::<&str>::new());

    let fenced = Checkout::consistent();
    fenced.write("docs/ROADMAP.md", "# Roadmap\n");
    fenced.write(
        "docs/README.md",
        "[CLI](cli.md) [Dev](development.md) [Install](install.md) [Features](features.md)\n\
         ````md\n```sh\n[Roadmap](ROADMAP.md)\n```\n````\n",
    );
    assert_eq!(fenced.failed_checks(), vec!["doc-index"]);

    let hidden = Checkout::consistent();
    hidden.write("docs/ROADMAP.md", "# Roadmap\n");
    hidden.write(
        "docs/README.md",
        "[CLI](cli.md) [Dev](development.md) [Install](install.md) [Features](features.md)\n\
         <!-- [Roadmap](ROADMAP.md) -->\n`[Roadmap](ROADMAP.md)`\n\n[unused]: ROADMAP.md\n",
    );
    assert_eq!(hidden.failed_checks(), vec!["doc-index"]);
}

#[test]
fn a_symbolic_link_reference_page_is_refused() {
    let checkout = Checkout::consistent();
    let outside = pohunek_test_support::tempdir_with_prefix("pxdd-outside-").expect("outside root");
    fs::write(outside.path().join("cli.md"), reference(&[], &[])).expect("write outside page");
    fs::remove_file(checkout.root.path().join("docs/cli.md")).expect("remove the page");
    std::os::unix::fs::symlink(
        outside.path().join("cli.md"),
        checkout.root.path().join("docs/cli.md"),
    )
    .expect("link the page");
    xtask::check_doc_drift(checkout.root.path()).unwrap_err();
}

#[test]
fn a_symbolic_link_documentation_root_is_refused() {
    let checkout = Checkout::consistent();
    let outside = pohunek_test_support::tempdir_with_prefix("pxdd-outside-").expect("outside root");
    fs::write(outside.path().join("page.md"), "# Outside\n").expect("write outside page");
    std::os::unix::fs::symlink(outside.path(), checkout.root.path().join(".agents"))
        .expect("link the agents root");
    xtask::check_doc_drift(checkout.root.path()).unwrap_err();
}

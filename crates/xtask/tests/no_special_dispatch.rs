//! Keeps per-agent special cases out of the daemon's production code.
//!
//! Launch, resume, fork, input framing, detection, probes, profile bases and
//! inventory resolve an agent through the runtime registry, so production code
//! under `crates/daemon/src` must not name the built-in agents `codex`,
//! `claude` or `hermes`. A name in a string literal or an identifier (for
//! example `AgentKind::Codex` or `run_hermes_version_probe`) is a finding, in
//! any letter case; comments and test code (as classified by
//! `xtask::test_code`) are not scanned.
//!
//! The names that remain are data or work owned by another issue. Each
//! allow-list entry pins the exact trimmed text of every source line that still
//! names an agent in one file (a SHA-256 over those lines for the large
//! integration files owned wholesale by one issue) and states which issue
//! removes them or why they are data. A new occurrence, a replaced line, a
//! change to a line that already names an agent, and a removed occurrence all
//! fail the scan until the entry is updated. `regenerate_allow_list` (ignored)
//! prints the entries of the current tree.

// Rust guideline compliant 2026-10-04

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

use xtask::test_code::{classify, read_sources, SourceFile};

/// Production sources the scan covers, as a workspace-relative path prefix.
const SCANNED_PREFIX: &str = "crates/daemon/src/";

/// Agent names that must not appear in production code (lowercase).
const AGENT_NAMES: [&str; 3] = ["codex", "claude", "hermes"];

/// Who removes an allow-listed occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// Compiled descriptor data that becomes package data when the built-in
    /// Codex, Claude and Hermes packages exist.
    Packages,
    /// Integration handlers (#144).
    Integration,
    /// Transcript parsing of external sessions (#487).
    Transcripts,
    /// Data tables that stay data: reserved ids and wire spellings.
    Data,
}

impl Owner {
    fn issue(self) -> &'static str {
        match self {
            Self::Packages => "#145 #146 #147",
            Self::Integration => "#144",
            Self::Transcripts => "#487",
            Self::Data => "data",
        }
    }
}

/// The approved occurrences of one file.
#[derive(Debug, Clone, Copy)]
enum Approved {
    /// The exact trimmed text of each naming line, sorted, duplicates kept.
    Lines(&'static [&'static str]),
    /// Number of naming lines and the SHA-256 (hex) over their sorted trimmed
    /// text, each followed by a newline.
    Digest(usize, &'static str),
}

/// Files with more naming lines than this are pinned by digest.
const DIGEST_THRESHOLD: usize = 60;

/// One file that may still name an agent: `(workspace-relative path, approved
/// occurrences, owner, reason)`.
type Entry = (&'static str, Approved, Owner, &'static str);

/// The trimmed text of the lines naming an agent, sorted.
type Occurrences = Vec<String>;

const ALLOW_LIST: &[Entry] = &[
    (
        "crates/daemon/src/agent/host/builtin.rs",
        Approved::Lines(&[
    "\"claude\" => detect::claude_manifest(),",
    "\"codex\" => detect::codex_manifest(),",
    "\"hermes\" => detect::hermes_manifest(),",
    "include_str!(\"../builtin/claude.toml\"),",
    "include_str!(\"../builtin/codex.toml\"),",
    "include_str!(\"../builtin/hermes.toml\"),",
]),
        Owner::Packages,
        "embedded built-in descriptors and their manifest name table",
    ),
    (
        "crates/daemon/src/agent/host/definition.rs",
        Approved::Lines(&[
    "HermesSafeText,",
    "InputTextPolicy::HermesSafeText => \"hermes_safe_text\",",
    "InputTextPolicy::HermesSafeText => InputRules::hermes(bracketed_paste, submit_delay),",
    "Self::HermesSafeText => InputTextPolicy::HermesSafeText,",
]),
        Owner::Data,
        "wire spelling of the `hermes_safe_text` input text policy",
    ),
    (
        "crates/daemon/src/agent/host/registry.rs",
        Approved::Lines(&[
    "pub const RESERVED_RUNTIME_IDS: [&str; 4] = [\"shell\", \"codex\", \"claude\", \"hermes\"];",
]),
        Owner::Data,
        "RESERVED_RUNTIME_IDS: ids no other source may claim",
    ),
    (
        "crates/daemon/src/agent/mod.rs",
        Approved::Lines(&[
    "HermesSafeText,",
    "if self.text_policy == InputTextPolicy::HermesSafeText",
    "pub(crate) const fn hermes(bracketed_paste: bool, submit_delay: Duration) -> Self {",
    "text_policy: InputTextPolicy::HermesSafeText,",
]),
        Owner::Data,
        "the Hermes safe-text input policy a descriptor selects by name",
    ),
    (
        "crates/daemon/src/agent/profile.rs",
        Approved::Lines(&[
    "\"use shell|codex|claude|hermes, or add ~/.config/pohunek/agents/<name>.toml on the target host\"",
]),
        Owner::Data,
        "recovery hint listing the built-in runtime ids",
    ),
    (
        "crates/daemon/src/capabilities.rs",
        Approved::Lines(&[
    "\"hermes-v1\" => Some(Self::HermesV1),",
    "\"pohunek-hermes-version-probe-{}\",",
    "&self.hermes_home_path,",
    ".env(\"HERMES_HOME\", &sandbox.hermes_home_path)",
    ".env(\"LANG\", HERMES_PROBE_LOCALE)",
    ".env(\"LC_ALL\", HERMES_PROBE_LOCALE)",
    ".env(\"PATH\", HERMES_PROBE_PATH)",
    ".expect(\"embedded Hermes compatibility lock must be valid JSON\")",
    ".strip_prefix(\"Hermes Agent v\")?",
    ".take(u64::try_from(HERMES_VERSION_OUTPUT_LIMIT).ok()?)",
    "HermesV1,",
    "Self::HermesV1 => supported_hermes_version(),",
    "Self::HermesV1 => {",
    "const HERMES_COMPATIBILITY_LOCK: &str =",
    "const HERMES_PROBE_LOCALE: &str = \"C\";",
    "const HERMES_PROBE_PATH: &str = \"/usr/bin:/bin\";",
    "const HERMES_VERSION_OUTPUT_LIMIT: usize = 4 * 1024;",
    "const HERMES_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);",
    "const MAX_HERMES_VERSION_BYTES: usize = 64;",
    "fn parse_hermes_version(output: &str) -> Option<String> {",
    "fn probe_hermes_version(",
    "fn probe_hermes_version_with_clock(",
    "fn run_hermes_version_probe(path: &std::path::Path) -> Option<String> {",
    "fn run_hermes_version_probe_with_seeded_env(",
    "fn run_hermes_version_probe_with_timeout(",
    "fn supported_hermes_version() -> &'static str {",
    "hermes_home_path: PathBuf,",
    "hermes_home_path: dir.join(\"hermes-home\"),",
    "if value.is_empty() || value.len() > MAX_HERMES_VERSION_BYTES {",
    "include_str!(\"../../../compat/hermes/compatibility-lock.json\");",
    "let mut output = Vec::with_capacity(HERMES_VERSION_OUTPUT_LIMIT);",
    "match probe_hermes_version(path, timeout, seeded_env) {",
    "probe_hermes_version_with_clock(path, timeout, seeded_env, Instant::now)",
    "rlim_cur: HERMES_VERSION_OUTPUT_LIMIT as libc::rlim_t,",
    "rlim_max: HERMES_VERSION_OUTPUT_LIMIT as libc::rlim_t,",
    "run_hermes_version_probe(path).and_then(|output| parse_hermes_version(&output))",
    "run_hermes_version_probe_with_seeded_env(path, timeout, &[])",
    "run_hermes_version_probe_with_timeout(path, HERMES_VERSION_PROBE_TIMEOUT)",
    "serde_json::from_str::<HermesCompatibilityLock>(HERMES_COMPATIBILITY_LOCK)",
    "struct HermesCompatibilityLock {",
]),
        Owner::Packages,
        "compiled Hermes version probe selected by a descriptor's probe name",
    ),
    (
        "crates/daemon/src/detect/mod.rs",
        Approved::Lines(&[
    ".get_or_init(|| Manifest::parse_str(CLAUDE_MANIFEST).expect(\"claude manifest must parse\"))",
    ".get_or_init(|| Manifest::parse_str(HERMES_MANIFEST).expect(\"Hermes manifest must parse\"))",
    "MANIFEST.get_or_init(|| Manifest::parse_str(CODEX_MANIFEST).expect(\"codex manifest must parse\"))",
    "const CLAUDE_MANIFEST: &str = include_str!(\"manifests/claude.toml\");",
    "const CODEX_MANIFEST: &str = include_str!(\"manifests/codex.toml\");",
    "const HERMES_MANIFEST: &str = include_str!(\"manifests/hermes.toml\");",
    "pub fn claude_manifest() -> &'static Manifest {",
    "pub fn codex_manifest() -> &'static Manifest {",
    "pub fn hermes_manifest() -> &'static Manifest {",
]),
        Owner::Packages,
        "compiled detection manifests of the built-in descriptors",
    ),
    (
        "crates/daemon/src/external/mod.rs",
        Approved::Lines(&[
    "CLAUDE_CONFIG_DIR_ENV,",
    "CLAUDE_HOME_RELATIVE,",
    "CLAUDE_TRANSCRIPT_SUBDIR,",
    "agent_base: AgentKind::Claude,",
    "agent_base: AgentKind::Codex,",
    "const CLAUDE_CONFIG_DIR_ENV: &str = \"CLAUDE_CONFIG_DIR\";",
    "const CLAUDE_HOME_RELATIVE: &str = \".claude\";",
    "const CLAUDE_TRANSCRIPT_SUBDIR: &str = \"projects\";",
    "const CODEX_HOME_ENV: &str = \"CODEX_HOME\";",
    "const CODEX_HOME_RELATIVE: &str = \".codex\";",
    "const CODEX_TRANSCRIPT_SUBDIR: &str = \"sessions\";",
    "provider_root(CODEX_HOME_ENV, CODEX_HOME_RELATIVE, CODEX_TRANSCRIPT_SUBDIR)",
]),
        Owner::Transcripts,
        "external transcript roots and parsing per provider",
    ),
    (
        "crates/daemon/src/integration/doctor.rs",
        Approved::Lines(&[
    "\"list the agent config directory (and the Claude hooks directory) by hand for entries whose names start with `.pohunek-`, review each, and remove unrelated files so the scan can complete\",",
    "\"point CLAUDE_CONFIG_DIR or CODEX_HOME at an existing absolute directory owned by the daemon user (use the canonical path when the current one goes through a symlink), then re-run the doctor\".to_owned(),",
    "(IntegrationFindingCode::CodexHooksFeatureDisabled, install)",
    "(IntegrationFindingCode::CodexTrustDrift, install)",
    "None => vec![StatusAgent::Claude, StatusAgent::Codex],",
    "Some(AgentKind::Claude) => vec![StatusAgent::Claude],",
    "Some(AgentKind::Codex) => vec![StatusAgent::Codex],",
    "Some(AgentKind::Hermes) => return Err(status_unsupported(&AgentKind::Hermes)),",
    "StatusAgent::Claude => claude_config_dir(),",
    "StatusAgent::Codex => codex_config_dir(),",
    "claude_config_dir, codex_config_dir, reported_agent_status, status_unsupported, StatusAgent,",
    "if *agent == AgentKind::Claude {",
    "} else if warning.contains(\"Codex hooks feature is not enabled\") {",
]),
        Owner::Integration,
        "integration handlers",
    ),
    (
        "crates/daemon/src/integration/mod.rs",
        Approved::Digest(225, "dc42f2fb71b5abe6fa2f1601bffdf794a4c083a2241c44331db7ae43cd7c41cf"),
        Owner::Integration,
        "integration handlers",
    ),
    (
        "crates/daemon/src/integration/uninstall.rs",
        Approved::Lines(&[
    "\"invalid TOML in Codex config.toml: {}\",",
    "(NOTIFY_HOOK_INSTALL_NAME, \"Claude notification hook\"),",
    "(NOTIFY_HOOK_INSTALL_NAME, \"Codex notification hook\"),",
    "(STATE_HOOK_INSTALL_NAME, \"Claude state hook\"),",
    "(STATE_HOOK_INSTALL_NAME, \"Codex state hook\"),",
    ".map(|file| (\"settings.json\", \"Claude settings.json\", file))",
    "AgentKind::Claude => uninstall_claude(&claude_config_dir()?)?,",
    "AgentKind::Claude,",
    "AgentKind::Codex => uninstall_codex(&codex_config_dir()?)?,",
    "AgentKind::Codex,",
    "AgentKind::Hermes => return Err(super::status_unsupported(&AgentKind::Hermes)),",
    "apply_trust_moves, claude_config_dir, claude_notify_hook_commands, codex_config_dir,",
    "claude_dir,",
    "claude_dir: &Path,",
    "codex_dir,",
    "codex_dir: &Path,",
    "codex_managed_hooks, codex_notify_hook_commands, config_dir_is_symlink, config_path_kind,",
    "const CLAUDE_OWNERSHIP_MARKER: &str = \"# POHUNEK_INTEGRATION_ID=claude\";",
    "const CODEX_OWNERSHIP_MARKER: &str = \"# POHUNEK_INTEGRATION_ID=codex\";",
    "fn strip_codex_trust(",
    "fn unchanged_codex_inputs<'a>(",
    "if !config_dir_present(claude_dir, \"Claude config directory\")? {",
    "if !config_dir_present(codex_dir, \"Codex config directory\")? {",
    "label: \"Claude settings.json\",",
    "label: \"Codex config.toml\",",
    "label: \"Codex hooks.json\",",
    "let config_file = root.read_optional(\"config.toml\", \"Codex config.toml\")?;",
    "let config_path = codex_dir.join(\"config.toml\");",
    "let hook_path = claude_dir.join(\"hooks\").join(STATE_HOOK_INSTALL_NAME);",
    "let hook_path = codex_dir.join(STATE_HOOK_INSTALL_NAME);",
    "let hooks = root.open_child(\"hooks\", \"Claude hooks directory\")?;",
    "let hooks_file = root.read_optional(\"hooks.json\", \"Codex hooks.json\")?;",
    "let hooks_path = codex_dir.join(\"hooks.json\");",
    "let managed_hooks = codex_managed_hooks(&hook_path, &notify_path);",
    "let notify_path = claude_dir.join(\"hooks\").join(NOTIFY_HOOK_INSTALL_NAME);",
    "let notify_path = codex_dir.join(NOTIFY_HOOK_INSTALL_NAME);",
    "let root = TrustedDir::open(claude_dir, \"Claude config directory\")?;",
    "let root = TrustedDir::open(codex_dir, \"Codex config directory\")?;",
    "let settings = root.read_optional(\"settings.json\", \"Claude settings.json\")?;",
    "let settings_path = claude_dir.join(\"settings.json\");",
    "let unchanged = unchanged_codex_inputs(",
    "managed_hooks: &[CodexManagedHook],",
    "marker: CLAUDE_OWNERSHIP_MARKER,",
    "marker: CODEX_OWNERSHIP_MARKER,",
    "owned.extend(claude_notify_hook_commands(&notify_path));",
    "owned.extend(codex_notify_hook_commands(&notify_path));",
    "pub fn uninstall_claude(claude_dir: &Path) -> Result<IntegrationUninstallReport, ProtocolError> {",
    "pub fn uninstall_codex(codex_dir: &Path) -> Result<IntegrationUninstallReport, ProtocolError> {",
    "pub(super) fn uninstall_claude_gated(",
    "pub(super) fn uninstall_codex_gated(",
    "return Ok(empty_report(AgentKind::Claude));",
    "return Ok(empty_report(AgentKind::Codex));",
    "strip_codex_trust(",
    "toml_error_summary, trust_rekeys, validate_config_dir, CodexManagedHook, ConfigPath,",
    "unchanged.push((\"config.toml\", \"Codex config.toml\", file));",
    "unchanged.push((\"hooks.json\", \"Codex hooks.json\", file));",
    "uninstall_claude_gated(claude_dir, &mut |_index, _name| Ok(()))",
    "uninstall_codex_gated(codex_dir, &mut |_index, _name| Ok(()))",
]),
        Owner::Integration,
        "integration handlers",
    ),
    (
        "crates/daemon/src/notifications/mod.rs",
        Approved::Lines(&[
    "if source.provider.eq_ignore_ascii_case(\"codex\")",
    "|| source.provider.eq_ignore_ascii_case(\"claude\")",
]),
        Owner::Packages,
        "provider names that select notification sources",
    ),
    (
        "crates/daemon/src/notifications/policy.rs",
        Approved::Lines(&[
    "(\"claude\".to_owned(), provider_policy.clone()),",
    "(\"codex\".to_owned(), provider_policy.clone()),",
    "(\"hermes\".to_owned(), provider_policy),",
]),
        Owner::Packages,
        "default per-provider notification policy",
    ),
    (
        "crates/daemon/src/session/reconcile.rs",
        Approved::Lines(&[
    "\"claude\" => AgentKind::Claude,",
    "\"claude\" => Some(protocol::AgentKind::Claude),",
    "\"codex\" => AgentKind::Codex,",
    "\"codex\" => Some(protocol::AgentKind::Codex),",
    "\"hermes\" => Some(protocol::AgentKind::Hermes),",
]),
        Owner::Packages,
        "worker provider strings mapped to agent kinds until the AgentKind to RuntimeRef swap and the built-in packages land",
    ),
];

/// Trimmed text of every line of `file` that names a built-in agent outside
/// comments and test code, sorted. A line counts once however often it names
/// an agent.
fn finding_lines(file: &SourceFile) -> Occurrences {
    let code = &file.views().code;
    let lowered = code.to_ascii_lowercase();
    let lines: BTreeSet<usize> = AGENT_NAMES
        .iter()
        .flat_map(|name| lowered.match_indices(name).map(|(offset, _)| offset))
        .filter(|&offset| !file.in_test_code(offset))
        .map(|offset| file.line_of(offset))
        .collect();
    let mut texts: Vec<String> = lines
        .into_iter()
        .filter_map(|line| code.lines().nth(line - 1))
        .map(|text| text.trim().to_owned())
        .collect();
    texts.sort();
    texts
}

fn digest_of(lines: &[String]) -> String {
    let mut hasher = Sha256::new();
    for line in lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Occurrences per scanned file for `files` (workspace-relative path to text).
fn scan_files(files: &BTreeMap<String, String>) -> BTreeMap<String, Occurrences> {
    classify(files)
        .values()
        .filter(|file| file.path().starts_with(SCANNED_PREFIX))
        .map(|file| (file.path().to_owned(), finding_lines(file)))
        .filter(|(_, lines)| !lines.is_empty())
        .collect()
}

fn scan_tree(root: &Path) -> BTreeMap<String, Occurrences> {
    let files = read_sources(root).unwrap_or_else(|e| panic!("read workspace sources: {e}"));
    scan_files(&files)
}

/// Lines in `found` that `approved` lacks and the reverse (multiset difference).
fn line_difference(found: &[String], approved: &[&str]) -> (Vec<String>, Vec<String>) {
    let mut remaining: Vec<&str> = approved.to_vec();
    let mut added = Vec::new();
    for line in found {
        match remaining.iter().position(|text| *text == line) {
            Some(at) => {
                remaining.swap_remove(at);
            }
            None => added.push(line.clone()),
        }
    }
    (added, remaining.into_iter().map(str::to_owned).collect())
}

/// Compares `found` with `allow`: unlisted files, new or changed lines, removed
/// lines and entries of files that no longer name an agent.
fn mismatches(found: &BTreeMap<String, Occurrences>, allow: &[Entry]) -> Vec<String> {
    let mut problems = Vec::new();
    for (path, lines) in found {
        let Some(&(_, approved, owner, _)) = allow.iter().find(|entry| entry.0 == path) else {
            problems.push(format!(
                "{path}: names a built-in agent on {lines:?}; resolve agents through the runtime \
                 registry"
            ));
            continue;
        };
        match approved {
            Approved::Lines(approved) => {
                let (added, removed) = line_difference(lines, approved);
                if !added.is_empty() {
                    problems.push(format!(
                        "{path}: new or changed lines naming a built-in agent (allowed for {}): \
                         {added:?}",
                        owner.issue()
                    ));
                }
                if !removed.is_empty() {
                    problems.push(format!(
                        "{path}: approved lines are gone or changed; update the allow-list entry: \
                         {removed:?}"
                    ));
                }
            }
            Approved::Digest(count, digest) => {
                if lines.len() != count || digest_of(lines) != digest {
                    problems.push(format!(
                        "{path}: the lines naming a built-in agent differ from the approved \
                         digest for {} ({} lines now, {count} approved); inspect the diff and run \
                         `regenerate_allow_list`",
                        owner.issue(),
                        lines.len()
                    ));
                }
            }
        }
    }
    for (path, ..) in allow {
        if !found.contains_key(*path) {
            problems.push(format!(
                "{path}: no longer names a built-in agent; remove its entry"
            ));
        }
    }
    problems
}

#[test]
fn production_code_names_no_builtin_agent_outside_the_allow_list() {
    let found = scan_tree(&pohunek_test_support::workspace_root());
    let problems = mismatches(&found, ALLOW_LIST);
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn allow_list_entries_are_unique_existing_files_with_a_reason() {
    let root = pohunek_test_support::workspace_root();
    let mut seen = BTreeSet::new();
    for (path, approved, _, reason) in ALLOW_LIST {
        assert!(path.starts_with(SCANNED_PREFIX), "{path} is not scanned");
        assert!(root.join(path).is_file(), "{path} does not exist");
        assert!(seen.insert(*path), "{path} is listed twice");
        assert!(!reason.trim().is_empty(), "{path} has no reason");
        match approved {
            Approved::Lines(lines) => {
                assert!(!lines.is_empty(), "{path} approves no lines");
                assert!(
                    lines.len() <= DIGEST_THRESHOLD,
                    "{path} should pin a digest"
                );
            }
            Approved::Digest(count, digest) => {
                assert!(*count > DIGEST_THRESHOLD, "{path} should list its lines");
                assert_eq!(digest.len(), 64, "{path} digest is not a SHA-256 hex");
            }
        }
    }
}

/// Prints the allow-list entries of the current tree (owner and reason are
/// kept from the existing entries, `data` and `TODO` for new files).
#[test]
#[ignore = "prints the allow-list for the current tree"]
fn regenerate_allow_list() {
    let found = scan_tree(&pohunek_test_support::workspace_root());
    for (path, lines) in &found {
        let known = ALLOW_LIST.iter().find(|entry| entry.0 == path);
        let owner = known.map_or("Data", |entry| match entry.2 {
            Owner::Packages => "Packages",
            Owner::Integration => "Integration",
            Owner::Transcripts => "Transcripts",
            Owner::Data => "Data",
        });
        let reason = known.map_or("TODO", |entry| entry.3);
        let approved = if lines.len() > DIGEST_THRESHOLD {
            format!("Approved::Digest({}, {:?})", lines.len(), digest_of(lines))
        } else {
            format!("Approved::Lines(&{lines:#?})")
        };
        println!("    (\n        {path:?},\n        {approved},\n        Owner::{owner},\n        {reason:?},\n    ),");
    }
}

fn scan_one(path: &str, text: &str) -> Occurrences {
    let files = BTreeMap::from([(path.to_owned(), text.to_owned())]);
    scan_files(&files).remove(path).unwrap_or_default()
}

const PATH: &str = "crates/daemon/src/x.rs";

#[test]
fn flags_literals_and_identifiers_in_any_letter_case() {
    let text = "fn f() {\n    let a = \"codex\";\n    let b = AgentKind::Claude;\n    let c = run_hermes_probe();\n    let d = \"HERMES\";\n}\n";
    assert_eq!(
        scan_one(PATH, text),
        [
            "let a = \"codex\";",
            "let b = AgentKind::Claude;",
            "let c = run_hermes_probe();",
            "let d = \"HERMES\";",
        ]
    );
}

#[test]
fn counts_a_line_once_however_often_it_names_an_agent() {
    let text = "const T: [&str; 3] = [\"codex\", \"claude\", \"hermes\"];\n";
    assert_eq!(scan_one(PATH, text).len(), 1);
}

#[test]
fn ignores_comments_and_test_code() {
    let text = "// codex\n/// claude\n/* hermes */\nfn f() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {\n        let a = \"codex\";\n    }\n}\n";
    assert!(scan_one(PATH, text).is_empty());
    assert!(scan_one("crates/daemon/src/session/tests.rs", "let a = \"codex\";\n").is_empty());
}

#[test]
fn ignores_files_outside_the_daemon_sources() {
    assert!(scan_one("crates/cli/src/x.rs", "let a = \"codex\";\n").is_empty());
    assert!(scan_one("crates/daemon/tests/x.rs", "let a = \"codex\";\n").is_empty());
}

fn found(path: &str, text: &str) -> BTreeMap<String, Occurrences> {
    BTreeMap::from([(path.to_owned(), scan_one(path, text))])
}

const ENTRY_OK: &[Entry] = &[(
    PATH,
    Approved::Lines(&["\"codex\" => data(),"]),
    Owner::Data,
    "data",
)];

#[test]
fn an_exact_allow_list_has_no_mismatch() {
    let scan = found(PATH, "fn f() {\n    \"codex\" => data(),\n}\n");
    assert!(mismatches(&scan, ENTRY_OK).is_empty());
}

#[test]
fn a_replaced_occurrence_in_an_allow_listed_file_fails() {
    // One approved line is removed and a different one added: the count is
    // unchanged, the content is not.
    let scan = found(PATH, "fn f() {\n    \"claude\" => spawn(),\n}\n");
    let problems = mismatches(&scan, ENTRY_OK);
    assert_eq!(problems.len(), 2, "{problems:#?}");
    assert!(problems
        .iter()
        .any(|p| p.contains("new or changed") && p.contains("spawn")));
    assert!(problems.iter().any(|p| p.contains("gone or changed")));
}

#[test]
fn new_dispatch_on_a_line_that_already_names_an_agent_fails() {
    let scan = found(
        PATH,
        "fn f() {\n    \"codex\" => data(), \"hermes\" => run(),\n}\n",
    );
    assert_eq!(mismatches(&scan, ENTRY_OK).len(), 2);
}

#[test]
fn an_unlisted_file_and_a_stale_entry_fail() {
    let scan = found("crates/daemon/src/y.rs", "let a = \"codex\";\n");
    let problems = mismatches(&scan, ENTRY_OK);
    assert_eq!(problems.len(), 2, "{problems:#?}");
    assert!(problems
        .iter()
        .any(|p| p.contains("y.rs") && p.contains("runtime registry")));
    assert!(problems.iter().any(|p| p.contains("remove its entry")));
}

#[test]
fn a_removed_occurrence_fails_until_the_entry_is_updated() {
    let two: &[Entry] = &[(
        PATH,
        Approved::Lines(&["\"claude\" => data(),", "\"codex\" => data(),"]),
        Owner::Data,
        "data",
    )];
    let scan = found(PATH, "fn f() {\n    \"codex\" => data(),\n}\n");
    let problems = mismatches(&scan, two);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].contains("gone or changed"));
}

#[test]
fn duplicate_lines_are_counted() {
    let scan = found(PATH, "\"codex\" => data(),\n\"codex\" => data(),\n");
    assert_eq!(mismatches(&scan, ENTRY_OK).len(), 1);
}

#[test]
fn a_digest_entry_detects_a_replaced_or_changed_line() {
    let text = "let a = \"codex\";\nlet b = \"claude\";\n";
    let scan = found(PATH, text);
    let lines = &scan[PATH];
    let digest = Box::leak(digest_of(lines).into_boxed_str());
    let entry = |count| -> Vec<Entry> {
        vec![(
            PATH,
            Approved::Digest(count, digest),
            Owner::Integration,
            "wholesale",
        )]
    };
    assert!(mismatches(&scan, &entry(2)).is_empty());
    let replaced = found(PATH, "let a = \"codex\";\nlet b = \"hermes\";\n");
    assert_eq!(mismatches(&replaced, &entry(2)).len(), 1);
    let extended = found(PATH, "let a = \"codex\";\nlet b = \"claude\"; run();\n");
    assert_eq!(mismatches(&extended, &entry(2)).len(), 1);
}

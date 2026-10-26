//! Drift checks between hand-maintained docs and their sources of truth.
//!
//! - The command checks hold every `pohunek ...` example outside the knowledge
//!   runbooks to the clap tree, require `docs/cli.md` to show every leaf
//!   command, and require its list of commands without `--json` to match the
//!   tree.
//! - The fact checks hold the crate table of `docs/development.md` to the
//!   workspace members, the official package lists and counts of the user docs
//!   to `runtime-packages/`, and the documentation indexes to every page.
//! - The plugin boundary check rejects a top-level directory under
//!   `runtime-packages/` or `compat/` that is not one of the four official
//!   runtimes: a new plugin lives in its own repository.

// Rust guideline compliant 2026-10-20

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use clap::Command;
use regex::Regex;

use crate::checks::{collect_pohunek_examples, parse_failure_message};
use crate::XtaskError;

/// The CLI reference the reverse and `--json` checks read.
const CLI_REFERENCE: &str = "docs/cli.md";

/// Sentence of [`CLI_REFERENCE`] that introduces the commands without `--json`;
/// the backticked command paths after it, up to the next blank line, are the
/// list the check compares with the clap tree.
const JSON_EXCEPTION_MARKER: &str = "The commands without `--json` are";

/// Command paths that exist in the clap tree and are intentionally absent from
/// [`CLI_REFERENCE`]. Each entry needs a reason.
const UNDOCUMENTED_LEAVES: [(&str, &str); 2] = [
    (
        "workflow __exec",
        "hidden: the service manager unit entry point of a plugin job",
    ),
    (
        "workflow __dispatch",
        "hidden: the service manager unit entry point of the reaction dispatcher",
    ),
];

/// Leaves that accept no `--json` for a reason the reference does not list.
const JSON_EXEMPT_LEAVES: [(&str, &str); 2] = [
    (
        "workflow __exec",
        "hidden: the service manager unit entry point of a plugin job",
    ),
    (
        "workflow __dispatch",
        "hidden: the service manager unit entry point of the reaction dispatcher",
    ),
];

/// Lines at the top of a page searched for a `Status:` marker.
const STATUS_SCAN_LINES: usize = 30;

/// Roots of the pages the forward check reads; a directory is read
/// recursively for `*.md`.
const COMMAND_SOURCES: [&str; 4] = ["README.md", "AGENTS.md", "docs", ".agents"];

/// Pages that record history, so their commands are not held to the current
/// tree: the knowledge log and its unreleased fragments.
const HISTORY_PAGES: [&str; 2] = ["docs/knowledge/log.md", "docs/knowledge/log.d"];

/// Design, phase, plan and review pages that carry no `Status:` header yet and
/// describe commands that are proposed or gone. #1291 gives each a status and
/// replaces this list with the header.
const UNMARKED_PAGES: [&str; 8] = [
    "docs/design",
    "docs/phases",
    "docs/superpowers",
    "docs/architecture-review-2026-07-04.md",
    "docs/architecture-review-2026-07-04-remaining.md",
    "docs/architecture-review-2026-07-04-codex-prompt.md",
    "docs/refactor-ms-rust-guidelines-audit.md",
    "docs/plan-phase-1.md",
];

/// The crate table of the development guide.
const CRATE_TABLE_PAGE: &str = "docs/development.md";

/// User pages that state which official runtime packages exist.
const PACKAGE_PAGES: [&str; 3] = ["README.md", "docs/install.md", "docs/features.md"];

/// The page that lists the packages of a daemon archive by path.
const PACKAGE_LISTING_PAGE: &str = "docs/install.md";

/// Trees in which only the official runtimes of [`TRANSITIONAL_PLUGIN_DIRECTORIES`]
/// may have a top-level directory.
const PLUGIN_TREES: [&str; 2] = ["runtime-packages", "compat"];

/// The official runtimes that still live in this repository. #1622 removes
/// them when they move to their own repositories; no entry is ever added.
const TRANSITIONAL_PLUGIN_DIRECTORIES: [&str; 4] = ["claude", "codex", "hermes", "pi"];

/// Indexes every documentation page must be reachable from.
const INDEX_ROOTS: [&str; 2] = ["docs/README.md", "docs/knowledge/index.md"];

/// One executable command of the clap tree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Leaf {
    path: Vec<String>,
    accepts_json: bool,
}

impl Leaf {
    fn name(&self) -> String {
        self.path.join(" ")
    }
}

/// Every leaf command of `root`, depth first, without the generated `help`.
fn leaves(root: &Command) -> Vec<Leaf> {
    let mut built = root.clone();
    built.build();
    let mut found = Vec::new();
    collect_leaves(&built, &mut Vec::new(), &mut found);
    found
}

fn collect_leaves(command: &Command, prefix: &mut Vec<String>, found: &mut Vec<Leaf>) {
    let mut has_children = false;
    for child in command.get_subcommands() {
        if child.get_name() == "help" {
            continue;
        }
        has_children = true;
        prefix.push(child.get_name().to_owned());
        collect_leaves(child, prefix, found);
        prefix.pop();
    }
    if !has_children && !prefix.is_empty() {
        found.push(Leaf {
            path: prefix.clone(),
            accepts_json: command
                .get_arguments()
                .any(|argument| argument.get_long() == Some("json")),
        });
    }
}

/// How an example resolves against the command tree.
#[derive(Debug, Default, PartialEq, Eq)]
struct Walk {
    /// The command path the leading words name.
    path: Vec<String>,
    /// A group command is followed by a word that is none of its subcommands.
    unknown_subcommand: bool,
    /// A long flag that the command at its position does not accept.
    unknown_flag: bool,
}

fn is_flag_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn find_argument<'a>(command: &'a Command, name: &str) -> Option<&'a clap::Arg> {
    command.get_arguments().find(|argument| {
        argument.get_long() == Some(name)
            || argument
                .get_all_aliases()
                .is_some_and(|aliases| aliases.contains(&name))
    })
}

fn takes_value(argument: &clap::Arg) -> bool {
    argument
        .get_num_args()
        .is_some_and(|range| range.takes_values())
}

/// Whether `command` passes the arguments after its action through verbatim,
/// so flags in an example belong to that action.
fn passes_arguments_through(command: &Command) -> bool {
    command
        .get_arguments()
        .any(clap::Arg::is_trailing_var_arg_set)
}

fn is_word(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && token
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn has_subcommands(command: &Command) -> bool {
    command
        .get_subcommands()
        .any(|child| child.get_name() != "help")
}

fn find_subcommand<'a>(command: &'a Command, name: &str) -> Option<&'a Command> {
    command.get_subcommands().find(|child| {
        child.get_name() == name || child.get_all_aliases().any(|alias| alias == name)
    })
}

/// The words of one command segment with the optional-argument brackets and
/// the escaped table pipes of the docs removed.
fn tokens_of(segment: &str) -> Vec<String> {
    shell_words(segment)
        .iter()
        .map(|token| {
            let bare = token.trim_matches(['[', ']', '(', ')', ',', ';']);
            if bare.starts_with('-') {
                bare
            } else {
                token.as_str()
            }
        })
        .filter(|token| !token.is_empty() && !matches!(*token, "|" | "\\|"))
        .map(str::to_owned)
        .collect()
}

/// `text` split at each `separator` outside single and double quotes.
fn split_unquoted(text: &str, separator: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut quote: Option<char> = None;
    let mut rest = text;
    while let Some(character) = rest.chars().next() {
        if quote.is_none() && rest.starts_with(separator) {
            parts.push(String::new());
            rest = &rest[separator.len()..];
            continue;
        }
        match quote {
            Some(open) if character == open => quote = None,
            None if character == '"' || character == '\'' => quote = Some(character),
            _ => {}
        }
        parts.last_mut().expect("one part").push(character);
        rest = &rest[character.len_utf8()..];
    }
    parts
}

/// Splits on whitespace outside single and double quotes; a quoted value stays
/// one word, quotes included, so flag-like text inside it is never a flag.
fn shell_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for character in text.chars() {
        match quote {
            Some(open) => {
                current.push(character);
                if character == open {
                    quote = None;
                }
            }
            None if character == '"' || character == '\'' => {
                quote = Some(character);
                current.push(character);
            }
            None if character.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            None => current.push(character),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// The `pohunek` commands of a shell line: `a && b`, `a; b`, `a || b` and
/// `a | b` each contribute their own command; other programs are dropped.
/// A separator counts outside quotes, after whitespace and before whitespace
/// or the end, so `a|b` alternatives and quoted text stay whole. An unquoted
/// `#` that starts a word begins a comment, which is dropped.
fn shell_commands(line: &str) -> Vec<String> {
    let characters: Vec<char> = line.chars().collect();
    let mut commands = vec![String::new()];
    let mut quote: Option<char> = None;
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if let Some(open) = quote {
            quote = (character != open).then_some(open);
        } else if character == '"' || character == '\'' {
            quote = Some(character);
        } else if character == '#' && (index == 0 || characters[index - 1].is_whitespace()) {
            break;
        } else {
            let width = [("&&", 2), ("||", 2), ("|", 1), (";", 1)]
                .iter()
                .find(|(separator, _)| {
                    characters[index..].starts_with(&separator.chars().collect::<Vec<_>>())
                })
                .map(|(_, width)| *width);
            if let Some(width) = width {
                let after = characters.get(index + width);
                let before_ok =
                    index == 0 || characters[index - 1].is_whitespace() || character == ';';
                let after_ok = after.is_none_or(|next| next.is_whitespace());
                if before_ok && after_ok {
                    commands.push(String::new());
                    index += width;
                    continue;
                }
            }
        }
        commands.last_mut().expect("one command").push(character);
        index += 1;
    }
    commands
        .iter()
        .map(|command| command.trim().to_owned())
        .filter(|command| command.starts_with("pohunek "))
        .collect()
}

/// Walks an example down the command tree: global flags and subcommand words
/// first, then the flags of the command they select. `root` must be built.
///
/// An example is held to its command path and to the long flags it names, not
/// to its required arguments: a doc may abbreviate a command line. Words
/// joined by `|` are alternatives: flags are each checked, subcommand words
/// each must exist, and the path ends there.
fn walk(root: &Command, tokens: &[String]) -> Walk {
    let mut node = root;
    let mut outcome = Walk::default();
    let mut index = 1;
    let mut in_path = true;
    let mut positional_seen = false;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        index += 1;
        if token == "--" {
            break;
        }
        if token == TOO_MANY_ALTERNATIVES {
            outcome.unknown_subcommand = true;
            break;
        }
        let alternatives: Vec<&str> = if token.contains(['"', '\'']) {
            vec![token]
        } else {
            token
                .split('|')
                .map(|piece| piece.trim_matches('\\'))
                .filter(|piece| !piece.is_empty())
                .collect()
        };
        let leaf = !has_subcommands(node);
        let mut skip_value = false;
        let mut is_flag = false;
        for piece in &alternatives {
            let Some(flag) = piece.strip_prefix("--").filter(|flag| !flag.is_empty()) else {
                continue;
            };
            let name = flag.split('=').next().unwrap_or_default();
            if !is_flag_name(name) {
                continue;
            }
            is_flag = true;
            if leaf && positional_seen && passes_arguments_through(node) {
                continue;
            }
            match find_argument(node, name) {
                None => outcome.unknown_flag = true,
                Some(argument) if takes_value(argument) && !flag.contains('=') => {
                    let next_is_flag = tokens.get(index).is_some_and(|next| next.starts_with('-'));
                    skip_value = alternatives.len() == 1
                        && (!next_is_flag || argument.is_allow_hyphen_values_set());
                }
                Some(_) => {}
            }
        }
        if is_flag {
            if skip_value {
                index += 1;
            }
            continue;
        }
        if token.starts_with('-') || !in_path {
            continue;
        }
        if !is_word(token) {
            if has_subcommands(node) {
                break;
            }
            in_path = false;
            positional_seen = true;
            continue;
        }
        match find_subcommand(node, token) {
            Some(child) => {
                outcome.path.push(child.get_name().to_owned());
                node = child;
            }
            None if has_subcommands(node) && node.get_positionals().next().is_none() => {
                outcome.unknown_subcommand = true;
                break;
            }
            None => {
                in_path = false;
                positional_seen = true;
            }
        }
    }
    outcome
}

/// The token lists an example names: itself, and for `pohunek group a / b / c`
/// one list per sibling after the first, with the group's path in front.
/// `root` must be built.
fn branches(root: &Command, command: &str) -> Vec<Vec<String>> {
    let mut found = Vec::new();
    for single in shell_commands(command) {
        for tokens in sibling_branches(root, &single) {
            found.extend(expand_alternatives(&tokens));
        }
    }
    found
}

/// The word that stands for an expansion past [`MAX_ALTERNATIVE_VARIANTS`]; no
/// command tree has it.
const TOO_MANY_ALTERNATIVES: &str = "<too-many-alternatives>";

/// Most variants one example may expand to through `a|b` alternatives; a
/// longer expansion is reported as drift so the example gets simplified.
const MAX_ALTERNATIVE_VARIANTS: usize = 64;

/// The token lists an example stands for when words joined by `|` are
/// alternatives: one list per choice. Past [`MAX_ALTERNATIVE_VARIANTS`] the
/// expansion stops and the last list ends in an unresolvable word.
fn expand_alternatives(tokens: &[String]) -> Vec<Vec<String>> {
    let mut variants: Vec<Vec<String>> = vec![Vec::new()];
    for token in tokens {
        let pieces: Vec<&str> = token
            .split('|')
            .map(|piece| piece.trim_matches('\\'))
            .filter(|piece| !piece.is_empty())
            .collect();
        let choices: Vec<String> = if pieces.len() > 1 && pieces.iter().all(|piece| is_word(piece))
        {
            pieces.iter().map(|piece| (*piece).to_owned()).collect()
        } else {
            vec![token.clone()]
        };
        let mut next = Vec::new();
        for variant in &variants {
            for choice in &choices {
                let mut extended = variant.clone();
                extended.push(choice.clone());
                next.push(extended);
            }
        }
        if next.len() > MAX_ALTERNATIVE_VARIANTS {
            variants.truncate(1);
            variants[0].push(TOO_MANY_ALTERNATIVES.to_owned());
            return variants;
        }
        variants = next;
    }
    variants
}

fn sibling_branches(root: &Command, command: &str) -> Vec<Vec<String>> {
    let mut segments = split_unquoted(command, " / ").into_iter();
    let first = tokens_of(&segments.next().unwrap_or_default());
    let outcome = walk(root, &first);
    let ends_at_command = first.last() == outcome.path.last();
    let mut found = vec![first];
    let split = outcome.path.split_last().filter(|_| ends_at_command);
    if let Some((_, parent)) = split {
        for segment in segments {
            let mut tokens = vec!["pohunek".to_owned()];
            tokens.extend(parent.iter().cloned());
            tokens.extend(tokens_of(&segment));
            found.push(tokens);
        }
        // The arguments after the last sibling belong to every sibling.
        let shared: Vec<String> = found
            .last()
            .and_then(|last| last.get(parent.len() + 2..))
            .map(<[String]>::to_vec)
            .unwrap_or_default();
        let last_index = found.len() - 1;
        for tokens in &mut found[..last_index] {
            tokens.extend(shared.iter().cloned());
        }
    }
    found
}

/// One finding of the forward check, located without echoing command text.
/// `root` must be built.
fn forward_failure(root: &Command, command: &str, location: &str) -> Option<String> {
    let outcomes: Vec<Walk> = branches(root, command)
        .iter()
        .map(|tokens| walk(root, tokens))
        .collect();
    if outcomes.iter().any(|outcome| outcome.unknown_subcommand) {
        Some(parse_failure_message(location, "unrecognized subcommand"))
    } else if outcomes.iter().any(|outcome| outcome.unknown_flag) {
        Some(parse_failure_message(location, "unknown flag"))
    } else {
        None
    }
}

fn read_error(path: &Path, source: std::io::Error) -> XtaskError {
    XtaskError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Markdown files under `root`, relative to `repo`, in path order. Symbolic
/// links are not followed.
fn markdown_files(repo: &Path, root: &str) -> Result<Vec<PathBuf>, XtaskError> {
    let start = repo.join(root);
    let metadata = match fs::symlink_metadata(&start) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(read_error(&start, source)),
    };
    if metadata.file_type().is_symlink() {
        return Err(XtaskError::UnsupportedFileType(start));
    }
    let mut found = Vec::new();
    if metadata.is_file() {
        found.push(PathBuf::from(root));
        return Ok(found);
    }
    let mut pending = vec![PathBuf::from(root)];
    while let Some(directory) = pending.pop() {
        let absolute = repo.join(&directory);
        let mut entries: Vec<_> = fs::read_dir(&absolute)
            .map_err(|source| read_error(&absolute, source))?
            .collect::<Result<_, _>>()
            .map_err(|source| read_error(&absolute, source))?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let relative = directory.join(entry.file_name());
            let kind = entry
                .file_type()
                .map_err(|source| read_error(&entry.path(), source))?;
            if kind.is_dir() {
                pending.push(relative);
            } else if kind.is_file() && relative.extension().is_some_and(|ext| ext == "md") {
                found.push(relative);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// `content` with each backslash-continued shell command on one line, followed
/// by as many blank lines as it absorbed, so line numbers stay those of the page.
fn join_continuations(content: &str) -> String {
    let mut joined = String::new();
    let mut absorbed = 0usize;
    for line in content.lines() {
        if let Some(head) = line.strip_suffix('\\') {
            joined.push_str(head);
            joined.push(' ');
            absorbed += 1;
            continue;
        }
        if absorbed == 0 {
            joined.push_str(line);
            joined.push('\n');
        } else {
            joined.push_str(line.trim_start());
            joined.push('\n');
            joined.push_str(&"\n".repeat(absorbed));
            absorbed = 0;
        }
    }
    joined
}

fn read_page(repo: &Path, relative: &Path) -> Result<String, XtaskError> {
    let mut walked = repo.to_path_buf();
    for component in relative.components() {
        walked.push(component);
        let metadata =
            fs::symlink_metadata(&walked).map_err(|source| read_error(&walked, source))?;
        if metadata.file_type().is_symlink() {
            return Err(XtaskError::UnsupportedFileType(walked));
        }
    }
    let path = repo.join(relative);
    fs::read_to_string(&path).map_err(|source| read_error(&path, source))
}

/// Whether the top of the page declares it `historical` or `superseded`, so it
/// is no longer held to the current command tree.
fn is_retired(content: &str) -> bool {
    let marker = Regex::new(r"(?i)^[*_>\s-]*status[*_]*\s*:[*_\s]*(historical|superseded)\b")
        .expect("valid status regex");
    content
        .lines()
        .take(STATUS_SCAN_LINES)
        .any(|line| marker.is_match(line))
}

fn is_exempt_page(relative: &Path) -> bool {
    HISTORY_PAGES
        .iter()
        .chain(UNMARKED_PAGES.iter())
        .any(|exempt| relative.starts_with(exempt))
}

/// Forward check: every `pohunek ...` example of the user, agent and design
/// docs resolves against the command tree.
fn check_commands_resolve(root: &Command, repo: &Path) -> Result<bool, XtaskError> {
    let mut built = root.clone();
    built.build();
    let mut failures = Vec::new();
    let mut checked = 0usize;
    for source in COMMAND_SOURCES {
        for relative in markdown_files(repo, source)? {
            if is_exempt_page(&relative) {
                continue;
            }
            let content = read_page(repo, &relative)?;
            if is_retired(&content) {
                continue;
            }
            for example in collect_pohunek_examples(&join_continuations(&content)) {
                checked += 1;
                let location = format!("{}:{}", relative.display(), example.line);
                failures.extend(forward_failure(&built, &example.command, &location));
            }
        }
    }
    if failures.is_empty() {
        println!("[PASS] doc-commands: {checked} example(s) resolve against the command tree");
        return Ok(true);
    }
    println!(
        "[FAIL] doc-commands: {} example(s) do not match the command tree (checked {checked}):",
        failures.len()
    );
    for failure in &failures {
        println!("        {failure}");
    }
    Ok(false)
}

/// The command paths the examples of `content` name, including the siblings
/// of a `pohunek group a / b / c` example.
fn documented_paths(root: &Command, content: &str) -> BTreeSet<String> {
    let mut built = root.clone();
    built.build();
    let mut paths = BTreeSet::new();
    for example in collect_pohunek_examples(&join_continuations(content)) {
        for tokens in branches(&built, &example.command) {
            let outcome = walk(&built, &tokens);
            if outcome.unknown_subcommand {
                continue;
            }
            paths.insert(outcome.path.join(" "));
        }
    }
    paths
}

/// Reverse check: every leaf command is shown in the CLI reference.
fn check_reference_covers_tree(root: &Command, repo: &Path) -> Result<bool, XtaskError> {
    let content = read_page(repo, Path::new(CLI_REFERENCE))?;
    let documented = documented_paths(root, &content);
    let missing: Vec<String> = leaves(root)
        .iter()
        .map(Leaf::name)
        .filter(|name| !documented.contains(name))
        .filter(|name| !UNDOCUMENTED_LEAVES.iter().any(|(leaf, _)| leaf == name))
        .collect();
    if missing.is_empty() {
        println!("[PASS] cli-reference-coverage: every leaf command appears in {CLI_REFERENCE}");
        return Ok(true);
    }
    println!(
        "[FAIL] cli-reference-coverage: {} command(s) missing from {CLI_REFERENCE}:",
        missing.len()
    );
    for name in &missing {
        println!("        pohunek {name}");
    }
    Ok(false)
}

/// The command paths listed after [`JSON_EXCEPTION_MARKER`].
fn listed_json_exceptions(content: &str) -> Option<BTreeSet<String>> {
    let start = content.find(JSON_EXCEPTION_MARKER)? + JSON_EXCEPTION_MARKER.len();
    let section = content[start..].split("\n\n").next()?;
    let span = Regex::new(r"`([^`]+)`").expect("valid span regex");
    Some(
        span.captures_iter(section)
            .map(|captures| captures[1].to_owned())
            .collect(),
    )
}

/// The reference's list of commands without `--json` equals the tree's.
fn check_json_exceptions(root: &Command, repo: &Path) -> Result<bool, XtaskError> {
    let content = read_page(repo, Path::new(CLI_REFERENCE))?;
    let Some(listed) = listed_json_exceptions(&content) else {
        println!("[FAIL] json-exceptions: {CLI_REFERENCE} has no sentence starting `{JSON_EXCEPTION_MARKER}`");
        return Ok(false);
    };
    let actual: BTreeSet<String> = leaves(root)
        .iter()
        .filter(|leaf| !leaf.accepts_json)
        .map(Leaf::name)
        .filter(|name| !JSON_EXEMPT_LEAVES.iter().any(|(leaf, _)| leaf == name))
        .collect();
    if listed == actual {
        println!(
            "[PASS] json-exceptions: the {} command(s) without --json match the tree",
            actual.len()
        );
        return Ok(true);
    }
    println!("[FAIL] json-exceptions: the list in {CLI_REFERENCE} differs from the command tree:");
    for name in actual.difference(&listed) {
        println!("        not listed, but has no --json: pohunek {name}");
    }
    for name in listed.difference(&actual) {
        println!("        listed, but is unknown or accepts --json: {name}");
    }
    Ok(false)
}

/// `crates/<name>` paths named in the table rows of `content`.
fn table_crates(content: &str) -> BTreeSet<String> {
    let path = Regex::new(r"`(crates/[a-z0-9_-]+)`").expect("valid crate path regex");
    content
        .lines()
        .filter(|line| line.starts_with('|'))
        .flat_map(|line| {
            path.captures_iter(line)
                .map(|captures| captures[1].to_owned())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The `crates/` members of the workspace manifest.
fn workspace_crates(manifest: &str) -> Result<BTreeSet<String>, String> {
    let parsed: toml::Table = manifest.parse().map_err(|error| format!("{error}"))?;
    let members = parsed
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "Cargo.toml has no [workspace] members".to_owned())?;
    Ok(members
        .iter()
        .filter_map(toml::Value::as_str)
        .filter(|member| member.starts_with("crates/"))
        .map(str::to_owned)
        .collect())
}

/// The crate table lists exactly the workspace crates.
fn check_crate_table(repo: &Path) -> Result<bool, XtaskError> {
    let manifest = read_page(repo, Path::new("Cargo.toml"))?;
    let members = match workspace_crates(&manifest) {
        Ok(members) => members,
        Err(reason) => {
            println!("[FAIL] crate-table: {reason}");
            return Ok(false);
        }
    };
    let listed = table_crates(&read_page(repo, Path::new(CRATE_TABLE_PAGE))?);
    if members == listed {
        println!(
            "[PASS] crate-table: {CRATE_TABLE_PAGE} lists the {} workspace crate(s)",
            members.len()
        );
        return Ok(true);
    }
    println!("[FAIL] crate-table: {CRATE_TABLE_PAGE} differs from the workspace members:");
    for name in members.difference(&listed) {
        println!("        missing from the table: {name}");
    }
    for name in listed.difference(&members) {
        println!("        in the table, not a workspace crate: {name}");
    }
    Ok(false)
}

/// Names of the official runtime packages, one directory each.
fn package_names(repo: &Path) -> Result<BTreeSet<String>, XtaskError> {
    let directory = repo.join("runtime-packages");
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(&directory).map_err(|source| read_error(&directory, source))? {
        let entry = entry.map_err(|source| read_error(&directory, source))?;
        let is_dir = entry
            .file_type()
            .map_err(|source| read_error(&entry.path(), source))?
            .is_dir();
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_dir && !name.starts_with('.') {
            names.insert(name);
        }
    }
    Ok(names)
}

const COUNT_WORDS: [(&str, usize); 8] = [
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("six", 6),
    ("seven", 7),
    ("eight", 8),
    ("nine", 9),
];

/// Counts a page states for the official packages, as `(count, line)`.
fn stated_package_counts(content: &str) -> Vec<(usize, usize)> {
    let phrase = Regex::new(r"(?i)\b(two|three|four|five|six|seven|eight|nine)\s+(?:official\s+)?(?:runtime\s+)?packages\b")
        .expect("valid package count regex");
    content
        .lines()
        .enumerate()
        .flat_map(|(index, line)| {
            phrase
                .captures_iter(line)
                .filter_map(|captures| {
                    let word = captures[1].to_ascii_lowercase();
                    COUNT_WORDS
                        .iter()
                        .find(|(candidate, _)| *candidate == word)
                        .map(|(_, count)| (*count, index + 1))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Package names the archive listing of `content` shows.
fn listed_packages(content: &str) -> BTreeSet<String> {
    let path = Regex::new(r"runtime/packages/([a-z0-9-]+)\.tar\.zst").expect("valid package regex");
    path.captures_iter(content)
        .map(|captures| captures[1].to_owned())
        .collect()
}

/// The archive listing names exactly the official packages, and no page
/// states a wrong count.
fn check_package_lists(repo: &Path) -> Result<bool, XtaskError> {
    let packages = package_names(repo)?;
    let mut problems = Vec::new();
    let listed = listed_packages(&read_page(repo, Path::new(PACKAGE_LISTING_PAGE))?);
    for name in packages.difference(&listed) {
        problems.push(format!(
            "{PACKAGE_LISTING_PAGE} does not list runtime/packages/{name}.tar.zst"
        ));
    }
    for name in listed.difference(&packages) {
        problems.push(format!(
            "{PACKAGE_LISTING_PAGE} lists runtime/packages/{name}.tar.zst, but runtime-packages/{name} does not exist"
        ));
    }
    for page in PACKAGE_PAGES {
        let content = read_page(repo, Path::new(page))?;
        for (count, line) in stated_package_counts(&content) {
            if count != packages.len() {
                problems.push(format!(
                    "{page}:{line} states {count} packages, but runtime-packages/ holds {}",
                    packages.len()
                ));
            }
        }
    }
    if problems.is_empty() {
        println!(
            "[PASS] package-lists: {} official package(s) match the user docs",
            packages.len()
        );
        return Ok(true);
    }
    println!("[FAIL] package-lists: the user docs differ from runtime-packages/:");
    for problem in &problems {
        println!("        {problem}");
    }
    Ok(false)
}

/// Relative Markdown link targets of `content`, fragments and queries removed.
fn link_targets(content: &str) -> Vec<String> {
    let text = without_code(content);
    let inline = Regex::new(r"\]\(<?([^)\s>]+)>?(?:\s+[^)]*)?\)").expect("valid link regex");
    let definition = Regex::new(
        r#"(?m)^\s*\[([^\]]+)\]:\s*<?(\S+?)>?(?:\s+(?:"[^"]*"|'[^']*'|\([^)]*\)))?\s*$"#,
    )
    .expect("valid reference regex");
    let reference = Regex::new(r"\[([^\]\n]+)\](\[([^\]\n]*)\])?").expect("valid use regex");
    let mut used = BTreeSet::new();
    for captures in reference.captures_iter(&text) {
        let whole = captures.get(0).expect("whole match");
        let after = text[whole.end()..].chars().next();
        if captures.get(2).is_none() && matches!(after, Some('(' | ':')) {
            continue;
        }
        let label = match captures.get(3) {
            Some(label) if !label.as_str().is_empty() => label.as_str(),
            _ => &captures[1],
        };
        used.insert(label.to_lowercase());
    }
    let targets = inline
        .captures_iter(&text)
        .map(|captures| captures[1].to_owned())
        .chain(
            definition
                .captures_iter(&text)
                .filter(|captures| used.contains(&captures[1].to_lowercase()))
                .map(|captures| captures[2].to_owned()),
        );
    targets
        .filter(|target| !target.contains("://") && !target.starts_with("mailto:"))
        .map(|target| percent_decoded(target.split(['#', '?']).next().unwrap_or_default()))
        .filter(|target| !target.is_empty())
        .collect()
}

/// `target` with its `%XX` escapes decoded; an escape that is not valid UTF-8
/// leaves the target as written.
fn percent_decoded(target: &str) -> String {
    let bytes = target.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| target.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(byte) = escaped {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| target.to_owned())
}

/// `content` without fenced code blocks, inline code spans and HTML comments,
/// whose links are examples or hidden, not navigation.
fn without_code(content: &str) -> String {
    let mut kept = String::new();
    let mut open_fence: Option<(char, usize)> = None;
    for line in content.lines() {
        let trimmed = line.trim_start();
        let marker = trimmed
            .chars()
            .next()
            .filter(|first| matches!(first, '`' | '~'));
        let run = marker.map_or(0, |marker| {
            trimmed.chars().take_while(|c| *c == marker).count()
        });
        match (open_fence, marker) {
            (Some((fence, length)), Some(marker))
                if marker == fence && run >= length && trimmed[run..].trim().is_empty() =>
            {
                open_fence = None;
            }
            (Some(_), _) => {}
            (None, Some(marker)) if run >= 3 => open_fence = Some((marker, run)),
            (None, _) => {
                kept.push_str(line);
                kept.push('\n');
            }
        }
    }
    let comment = Regex::new(r"(?s)<!--.*?-->").expect("valid comment regex");
    let span = Regex::new(r"`[^`\n]*`").expect("valid code span regex");
    span.replace_all(&comment.replace_all(&kept, ""), "")
        .into_owned()
}

/// `target` resolved against the directory of `page`, without `.` and `..`.
fn resolve_link(page: &Path, target: &str) -> Option<PathBuf> {
    let mut resolved = page.parent().map(Path::to_path_buf).unwrap_or_default();
    for component in Path::new(target).components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::ParentDir => {
                if !resolved.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(resolved)
}

/// Pages that need an index entry: the reference pages under `docs/` and every
/// knowledge page except the unreleased log fragments.
fn indexed_pages(repo: &Path) -> Result<BTreeSet<PathBuf>, XtaskError> {
    let mut pages = BTreeSet::new();
    for page in markdown_files(repo, "docs")? {
        let top_level = page.parent() == Some(Path::new("docs"));
        let knowledge =
            page.starts_with("docs/knowledge") && !page.starts_with("docs/knowledge/log.d");
        if top_level || knowledge {
            pages.insert(page);
        }
    }
    Ok(pages)
}

/// Every reference and knowledge page is reachable from a documentation index
/// by relative Markdown links.
fn check_index_reachability(repo: &Path) -> Result<bool, XtaskError> {
    let pages = indexed_pages(repo)?;
    let mut links: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for page in markdown_files(repo, "docs")? {
        let content = read_page(repo, &page)?;
        let targets = link_targets(&content)
            .iter()
            .filter_map(|target| resolve_link(&page, target))
            .collect();
        links.insert(page, targets);
    }
    let mut reached: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue: VecDeque<PathBuf> = INDEX_ROOTS.iter().map(PathBuf::from).collect();
    while let Some(page) = queue.pop_front() {
        if !reached.insert(page.clone()) {
            continue;
        }
        queue.extend(links.get(&page).cloned().unwrap_or_default());
    }
    let orphans: Vec<&PathBuf> = pages
        .iter()
        .filter(|page| {
            !reached.contains(*page)
                && !INDEX_ROOTS
                    .iter()
                    .any(|root| page.as_path() == Path::new(root))
        })
        .collect();
    if orphans.is_empty() {
        println!(
            "[PASS] doc-index: all {} page(s) are reachable from the documentation indexes",
            pages.len()
        );
        return Ok(true);
    }
    println!(
        "[FAIL] doc-index: {} page(s) are not linked from {}:",
        orphans.len(),
        INDEX_ROOTS.join(" or ")
    );
    for page in orphans {
        println!("        {}", page.display());
    }
    Ok(false)
}

/// Top-level directory names of `tree`, dot-named and symbolic-link entries
/// included; a missing tree holds none.
fn plugin_tree_directories(repo: &Path, tree: &str) -> Result<BTreeSet<String>, XtaskError> {
    let directory = repo.join(tree);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeSet::new());
        }
        Err(source) => return Err(read_error(&directory, source)),
    };
    let mut names = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|source| read_error(&directory, source))?;
        let kind = entry
            .file_type()
            .map_err(|source| read_error(&entry.path(), source))?;
        if kind.is_dir() || kind.is_symlink() {
            names.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(names)
}

/// Only the four official runtimes have a directory under `runtime-packages/`
/// or `compat/`; a new plugin goes to its own repository.
fn check_plugin_directories(repo: &Path) -> Result<bool, XtaskError> {
    let mut problems = Vec::new();
    for tree in PLUGIN_TREES {
        for name in plugin_tree_directories(repo, tree)? {
            if !TRANSITIONAL_PLUGIN_DIRECTORIES.contains(&name.as_str()) {
                problems.push(format!("{tree}/{name}"));
            }
        }
    }
    if problems.is_empty() {
        println!("[PASS] plugin-directories: no plugin directory beyond the official runtimes");
        return Ok(true);
    }
    println!(
        "[FAIL] plugin-directories: a new plugin lives in its own repository \
         (docs/knowledge/guides/plugin-repositories.md), not in:"
    );
    for problem in &problems {
        println!("        {problem}");
    }
    Ok(false)
}

/// Runs every drift check of this module against the checkout at `repo` and
/// returns the names of the checks that failed.
pub(crate) fn check(repo: &Path) -> Result<Vec<&'static str>, XtaskError> {
    let tree = cli::command();
    let outcomes = [
        ("doc-commands", check_commands_resolve(&tree, repo)?),
        (
            "cli-reference-coverage",
            check_reference_covers_tree(&tree, repo)?,
        ),
        ("json-exceptions", check_json_exceptions(&tree, repo)?),
        ("crate-table", check_crate_table(repo)?),
        ("package-lists", check_package_lists(repo)?),
        ("doc-index", check_index_reachability(repo)?),
        ("plugin-directories", check_plugin_directories(repo)?),
    ];
    Ok(outcomes
        .into_iter()
        .filter_map(|(name, passed)| (!passed).then_some(name))
        .collect())
}

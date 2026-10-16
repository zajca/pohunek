//! `cargo xtask affected guard`: every repository path a test reads selects it.
//!
//! Test code reaches repository files at run time through
//! `pohunek_test_support::workspace_root()` and `manifest_dir()` (the
//! `test_artifact_lookup_guard` keeps compile-time lookups out of test code),
//! or through a path relative to the package directory, which nextest makes
//! the working directory. The guard finds those reads in the test code of every
//! package, as `xtask::test_code` classifies it:
//!
//! - an anchor call followed by a chain of `.join(..)` calls. The anchors are
//!   `workspace_root()`, `manifest_dir()`, and every argument-less test
//!   function of the same package whose body is an anchor expression. A string
//!   literal or a string constant of the file is a path segment; a plain
//!   variable is the wildcard `*`, one path component; any other argument ends
//!   the chain, and the resolved prefix is read as a directory;
//! - a `let` binding of such an expression: each later use of the bound name
//!   reads the bound path, extended by the `.join(..)` chain that follows it;
//! - a string literal that starts with `../`, relative to the package directory.
//!
//! A read path is matched against the tracked files: the files it names and
//! the files below it. Each of them must select the reader when it changes: it
//! lies in the reader's directory, or its classification ([`RULES`] through
//! the closure) selects the reader or names tests of it, or it selects every
//! test. A use that resolves to the repository root itself (a whole-repository
//! scan, or the root passed on) is not a path the guard can judge: its file
//! must be listed in [`SCANS`], whose declared scopes are checked in its place.
//!
//! Limits: a path built at run time (`format!`, a field, a value read from a
//! file) is not found. The runtime guards of the CI test jobs cover those.
//!
//! [`RULES`]: super::RULES

// Rust guideline compliant 2026-10-16

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, PathBuf};

use super::{
    classify, command_output, ensure_git_toplevel, graph, is_below, owning_package, parse_nul_list,
    printable, workspace, Decision, Effect, Mode, Package, Reason, GIT, RULES,
};
use crate::test_code::{
    classify as classify_sources, is_ident, read_sources, skip_whitespace, string_literal_at,
    SourceFile,
};
use crate::XtaskError;

/// Test-support functions that return a repository directory, and what they return.
const ANCHORS: [(&str, Base); 2] = [
    ("workspace_root", Base::Root),
    ("manifest_dir", Base::Package),
];

/// Method that extends a path by one argument.
const JOIN: &str = ".join(";

/// Path component that matches any one component.
const WILDCARD: &str = "*";

/// Test files that use the repository root itself, with the paths they read.
///
/// Each entry is `(file, scopes, why)`. A scope is a read path as the guard
/// spells it (workspace-relative, `*` for one component) and is checked like a
/// read found in the file. A file that uses the bare root and is missing here
/// is a violation, as is an entry whose tracked file no longer uses it.
pub(crate) const SCANS: &[(&str, &[&str], &str)] = &[
    (
        "crates/cli/tests/claude_package.rs",
        &["runtime-packages/claude", "compat/claude"],
        "goldens and TOML files of the Claude package, read through helpers that take the path",
    ),
    (
        "crates/cli/tests/codex_package.rs",
        &["runtime-packages/codex", "compat/codex"],
        "goldens and TOML files of the Codex package, read through helpers that take the path",
    ),
    (
        "crates/cli/tests/hermes_package.rs",
        &["runtime-packages/hermes", "compat/hermes"],
        "goldens and TOML files of the Hermes package, read through helpers that take the path",
    ),
    (
        "crates/xtask/src/affected/mod.rs",
        &["Cargo.toml", "crates"],
        "member manifests and the Rust sources of every crate, scanned by the include check",
    ),
    (
        "crates/xtask/src/agent_skill.rs",
        &[
            "docs/knowledge/guides/agent-skill.md",
            "crates/cli/src/commands/agent_skill/SKILL.md",
        ],
        "agent skill source and its generated copy",
    ),
    (
        "crates/xtask/src/attestation.rs",
        &["compat/matrix.json", "compat/*/compatibility-lock.json", "runtime-packages"],
        "matrix check over the compatibility matrix, the locks and the package list",
    ),
    (
        "crates/xtask/src/test_runtime.rs",
        &["runtime-packages/claude", "runtime-packages/codex"],
        "test-runtime scenarios that build the Claude and Codex packages",
    ),
    (
        "crates/xtask/src/upstream_stage_tests.rs",
        &[
            "compat/*/compatibility-lock.json",
            "compat/*/npm",
            "scripts/provision-hermes-compat",
            "runtime-packages",
        ],
        "upstream staging scenarios over the locks, npm projects, the Hermes provisioner and the package list",
    ),
    (
        "crates/xtask/tests/affected_ci.rs",
        &["crates", "rust-toolchain.toml"],
        "the guard run over every crate's test code, and the toolchain pin the fixtures copy",
    ),
    (
        "crates/xtask/tests/dependency_policy.rs",
        &["Cargo.toml", "Cargo.lock", ".clippy.toml", ".cargo", "crates"],
        "workspace resolution and member manifests",
    ),
    (
        "crates/xtask/tests/docs_build.rs",
        &[
            "docs/knowledge",
            "crates/protocol/src/lib.rs",
            "crates/knowledge/src/lib.rs",
        ],
        "knowledge bundle validation and the crate roots it links",
    ),
    (
        "crates/xtask/tests/hermetic_scan.rs",
        &["crates"],
        "scan of every crate's Rust sources",
    ),
    (
        "crates/xtask/tests/no_special_dispatch.rs",
        &["crates"],
        "scan of every crate's Rust sources",
    ),
    (
        "crates/xtask/tests/official_lock_shape.rs",
        &["compat/*/compatibility-lock.json"],
        "shape check of every official compatibility lock",
    ),
    (
        "crates/xtask/tests/test_artifact_lookup_guard.rs",
        &["crates"],
        "scan of every crate's Rust sources",
    ),
    (
        "crates/xtask/tests/timing_ratchet.rs",
        &["crates"],
        "scan of every crate's Rust sources against the timing baseline",
    ),
];

/// Resolved reads that are not reads, as `(file, path, why)`.
///
/// A stale entry, whose read the file no longer has, is a violation.
pub(crate) const NOT_READS: &[(&str, &str, &str)] = &[(
    "crates/xtask/src/test_runtime.rs",
    "crates",
    "an output directory inside the checkout that the command refuses before reading",
)];

/// What a read path is relative to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Base {
    Root,
    Package,
}

/// A function or binding whose value is a repository path.
#[derive(Clone, Debug)]
struct Anchor {
    base: Base,
    /// Segments below the base.
    prefix: Vec<String>,
}

/// One repository path read by test code.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Read {
    /// Workspace-relative source file of the read.
    pub(crate) file: String,
    pub(crate) line: usize,
    /// Workspace-relative path read, `/`-separated, `*` for one component; empty
    /// for the repository root.
    pub(crate) path: String,
    /// Found from an anchor, so it must name a tracked path; a `../` literal
    /// may name anything.
    pub(crate) anchored: bool,
}

/// A read whose path does not select the reading package.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Violation {
    pub(crate) read: Read,
    pub(crate) reader: String,
    /// What is wrong: the uncovered file and its decision, or the missing scan.
    pub(crate) problem: String,
}

/// Options for one `cargo xtask affected guard` invocation.
#[derive(Debug, clap::Args)]
pub(crate) struct Options {
    /// Print every read found, with its reader, before the verdict.
    #[arg(long)]
    list: bool,
}

/// Checks the repository of the working directory; fails listing every violation.
pub(crate) fn run(options: &Options) -> Result<(), XtaskError> {
    let cwd = std::env::current_dir().map_err(|source| XtaskError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    let (root, packages) = workspace(&cwd)?;
    ensure_git_toplevel(&root)?;
    let tracked: BTreeSet<String> =
        parse_nul_list(&command_output(&root, GIT, &["ls-files", "-z"])?).collect();
    let sources = read_sources(&root)?;
    let reads = find_reads(&classify_sources(&sources), &packages);
    if options.list {
        for (reader, read) in &reads {
            println!(
                "{}:{}: {reader} reads {}",
                printable(&read.file),
                read.line,
                shown(&read.path)
            );
        }
    }
    let violations = check(&reads, &packages, &tracked, SCANS, NOT_READS);
    if violations.is_empty() {
        println!(
            "affected guard ok: {} repository reads in test code select their readers",
            reads.len()
        );
        return Ok(());
    }
    for violation in &violations {
        eprintln!(
            "{}:{}: {} reads {}: {}",
            printable(&violation.read.file),
            violation.read.line,
            violation.reader,
            shown(&violation.read.path),
            violation.problem
        );
    }
    Err(XtaskError::Usage(format!(
        "affected guard: {} repository reads of test code are not covered; map each \
         path to its reader in RULES, or declare a root scan in SCANS \
         (crates/xtask/src/affected/)",
        violations.len()
    )))
}

/// A read path for display; the empty path is the repository root.
fn shown(path: &str) -> String {
    if path.is_empty() {
        "the repository root".to_owned()
    } else {
        printable(path)
    }
}

/// Every repository read in the test code of `files`, with its reading package.
pub(crate) fn find_reads(
    files: &BTreeMap<String, SourceFile>,
    packages: &[Package],
) -> Vec<(String, Read)> {
    let mut by_package: BTreeMap<&Package, Vec<&SourceFile>> = BTreeMap::new();
    for file in files.values() {
        if let Some(owner) = owning_package(file.path(), packages) {
            by_package.entry(owner).or_default().push(file);
        }
    }
    let mut reads = BTreeSet::new();
    for (package, files) in by_package {
        let (shared, bodies) = package_anchors(&files);
        for file in files {
            // A file's own wrappers win over a same-named wrapper elsewhere.
            let mut anchors = shared.clone();
            for (name, anchor, _) in wrappers(file, &shared) {
                anchors.insert(name, anchor);
            }
            for (offset, path, anchored) in reads_in(file, &package.dir, &anchors, &bodies) {
                reads.insert((
                    package.name.clone(),
                    Read {
                        file: file.path().to_owned(),
                        line: file.line_of(offset),
                        path,
                        anchored,
                    },
                ));
            }
        }
    }
    reads.into_iter().collect()
}

/// The anchors of one package: [`ANCHORS`] and the wrapper functions of its test code.
///
/// A wrapper is an argument-less function in test code whose body is an anchor
/// expression; wrappers of wrappers resolve because the search repeats until
/// it finds nothing new. A name that files of the package define with
/// different values is left out, so each of those files resolves its own.
/// Also returns the `(file, offset)` of each wrapper's body call, which
/// defines the wrapper and reads nothing itself.
fn package_anchors(files: &[&SourceFile]) -> (BTreeMap<String, Anchor>, BTreeSet<(String, usize)>) {
    let mut anchors: BTreeMap<String, Anchor> = ANCHORS
        .iter()
        .map(|(name, base)| {
            (
                (*name).to_owned(),
                Anchor {
                    base: *base,
                    prefix: Vec::new(),
                },
            )
        })
        .collect();
    let mut bodies = BTreeSet::new();
    let mut ambiguous = BTreeSet::new();
    loop {
        let mut found: BTreeMap<String, Anchor> = BTreeMap::new();
        for file in files {
            for (name, anchor, body) in wrappers(file, &anchors) {
                bodies.insert((file.path().to_owned(), body));
                if anchors.contains_key(&name) || ambiguous.contains(&name) {
                    continue;
                }
                match found.get(&name) {
                    Some(other) if other.base != anchor.base || other.prefix != anchor.prefix => {
                        found.remove(&name);
                        ambiguous.insert(name);
                    }
                    Some(_) => {}
                    None => {
                        found.insert(name, anchor);
                    }
                }
            }
        }
        if found.is_empty() {
            return (anchors, bodies);
        }
        anchors.extend(found);
    }
}

/// Argument-less test functions of `file` whose body is an anchor expression,
/// with the offset of the body's anchor call.
fn wrappers(file: &SourceFile, anchors: &BTreeMap<String, Anchor>) -> Vec<(String, Anchor, usize)> {
    let views = file.views();
    let skeleton = views.skeleton.as_bytes();
    let mut found = Vec::new();
    for (offset, _) in views.skeleton.match_indices("fn ") {
        if !file.in_test_code(offset) || (offset > 0 && is_ident(skeleton[offset - 1])) {
            continue;
        }
        let name_start = skip_whitespace(skeleton, offset + 3);
        let name_end = ident_end(skeleton, name_start);
        let open = skip_whitespace(skeleton, name_end);
        if name_end == name_start || !views.skeleton[open..].starts_with("()") {
            continue;
        }
        let Some(body) = views.skeleton[open..].find('{').map(|i| open + i) else {
            continue;
        };
        let start = skip_whitespace(skeleton, body + 1);
        let Some((anchor, call_end)) = anchor_call_at(&views.skeleton, start, anchors) else {
            continue;
        };
        let (segments, end) = join_chain(file, &string_constants(file), call_end);
        let close = skip_whitespace(skeleton, end);
        if !views.skeleton[close..].starts_with('}') {
            continue;
        }
        let mut prefix = anchor.prefix.clone();
        prefix.extend(segments);
        found.push((
            views.skeleton[name_start..name_end].to_owned(),
            Anchor {
                base: anchor.base,
                prefix,
            },
            start,
        ));
    }
    found
}

/// The anchor called at `start` (with an optional path prefix such as
/// `pohunek_test_support::`) and the offset after its `()`.
fn anchor_call_at<'a>(
    skeleton: &str,
    start: usize,
    anchors: &'a BTreeMap<String, Anchor>,
) -> Option<(&'a Anchor, usize)> {
    let bytes = skeleton.as_bytes();
    let mut at = start;
    loop {
        let end = ident_end(bytes, at);
        if end == at {
            return None;
        }
        if skeleton[end..].starts_with("::") {
            at = end + 2;
            continue;
        }
        let anchor = anchors.get(&skeleton[at..end])?;
        let open = skip_whitespace(bytes, end);
        if !skeleton[open..].starts_with('(') {
            return None;
        }
        let close = skip_whitespace(bytes, open + 1);
        return skeleton[close..]
            .starts_with(')')
            .then_some((anchor, close + 1));
    }
}

/// Index one past the identifier starting at `start` (equal to `start` when none does).
fn ident_end(bytes: &[u8], start: usize) -> usize {
    start
        + bytes[start.min(bytes.len())..]
            .iter()
            .take_while(|byte| is_ident(**byte))
            .count()
}

/// Reads in the test code of `file`, as `(offset, workspace-relative path,
/// anchored)`.
fn reads_in(
    file: &SourceFile,
    package_dir: &str,
    anchors: &BTreeMap<String, Anchor>,
    bodies: &BTreeSet<(String, usize)>,
) -> Vec<(usize, String, bool)> {
    let views = file.views();
    let skeleton = views.skeleton.as_bytes();
    let constants = string_constants(file);
    // (offset, base, segments)
    let mut found: Vec<(usize, Base, Vec<String>)> = Vec::new();
    // (offset, name, anchor)
    let mut bindings: Vec<(usize, String, Anchor)> = Vec::new();

    for name in anchors.keys() {
        for (offset, _) in views.skeleton.match_indices(name.as_str()) {
            let before = &views.skeleton[..offset];
            if !file.in_test_code(offset)
                || (offset > 0 && is_ident(skeleton[offset - 1]))
                || before.trim_end().ends_with("fn")
                || before.ends_with('.')
            {
                continue;
            }
            let mut start = offset;
            while let Some(qualified) = anchor_start(&views.skeleton[..start]) {
                start = qualified;
            }
            if bodies.contains(&(file.path().to_owned(), start)) {
                continue;
            }
            let Some((anchor, call_end)) = anchor_call_at(&views.skeleton, offset, anchors) else {
                continue;
            };
            let (segments, end) = join_chain(file, &constants, call_end);
            let mut path = anchor.prefix.clone();
            path.extend(segments);
            match bound_name(&views.skeleton, offset, end) {
                Some(binding) => bindings.push((
                    offset,
                    binding,
                    Anchor {
                        base: anchor.base,
                        prefix: path,
                    },
                )),
                None => found.push((offset, anchor.base, path)),
            }
        }
    }

    // Every later use of a bound name in its block, until it is bound again.
    bindings.sort_by_key(|(offset, ..)| *offset);
    for (index, (offset, name, anchor)) in bindings.iter().enumerate() {
        let until = bindings[index + 1..]
            .iter()
            .find(|(_, other, _)| other == name)
            .map_or(views.skeleton.len(), |(other, ..)| *other)
            .min(block_end(&views.skeleton, *offset));
        let statement_end = views.skeleton[*offset..until]
            .find(';')
            .map_or(until, |i| offset + i);
        for (use_offset, _) in views.skeleton[statement_end..until].match_indices(name.as_str()) {
            let at = statement_end + use_offset;
            let end = at + name.len();
            let whole = (at == 0 || !is_ident(skeleton[at - 1]))
                && !skeleton.get(end).copied().is_some_and(is_ident)
                && !views.skeleton[..at].ends_with('.')
                && !views.skeleton[..at].ends_with("::");
            if !whole || !file.in_test_code(at) {
                continue;
            }
            // Formatting a path for a message reads nothing.
            if views.skeleton[end..].starts_with(".display()") {
                continue;
            }
            let (segments, _) = join_chain(file, &constants, end);
            let mut path = anchor.prefix.clone();
            path.extend(segments);
            found.push((at, anchor.base, path));
        }
    }

    let mut reads: Vec<(usize, String, bool)> = found
        .into_iter()
        .filter_map(|(offset, base, segments)| {
            let start = match base {
                // A root path whose first component is unknown can be any
                // path: it is judged like the root itself.
                Base::Root if segments.first().is_some_and(|first| first == WILDCARD) => {
                    return Some((offset, String::new(), true));
                }
                Base::Root => "",
                Base::Package => package_dir,
            };
            Some((offset, normalize(start, &segments)?, true))
        })
        .collect();

    // Literals relative to the package directory, the test's working directory.
    for (offset, literal) in literals(file) {
        if literal.starts_with("../") {
            if let Some(path) = normalize(package_dir, &[literal.to_owned()]) {
                if !path.is_empty() {
                    reads.push((offset, path, false));
                }
            }
        }
    }
    reads
}

/// Offset of the `}` that closes the innermost block containing `offset`, or
/// the text length when the block is not closed.
fn block_end(skeleton: &str, offset: usize) -> usize {
    let bytes = skeleton.as_bytes();
    let mut depth = 0_usize;
    for (index, byte) in bytes.iter().enumerate().skip(offset) {
        match byte {
            b'{' => depth += 1,
            b'}' if depth == 0 => return index,
            b'}' => depth -= 1,
            _ => {}
        }
    }
    bytes.len()
}

/// Start of the path-qualified call ending at `before` (which ends with `::`).
fn anchor_start(before: &str) -> Option<usize> {
    let trimmed = before.strip_suffix("::")?;
    let start = trimmed
        .bytes()
        .rposition(|byte| !is_ident(byte))
        .map_or(0, |index| index + 1);
    (start < trimmed.len()).then_some(start)
}

/// Arguments of the `.join(..)` chain starting at `from`, and the offset after
/// the last call taken.
///
/// A string literal or a string constant of the file is a segment, and a plain
/// variable is [`WILDCARD`]; any other argument ends the chain, so the path
/// read is the prefix resolved so far.
fn join_chain(
    file: &SourceFile,
    constants: &BTreeMap<String, String>,
    mut from: usize,
) -> (Vec<String>, usize) {
    let views = file.views();
    let skeleton = views.skeleton.as_bytes();
    // Whitespace before an argument is skipped in the code view: the skeleton
    // blanks a literal into spaces, which would be skipped with it.
    let code = views.code.as_bytes();
    let mut segments = Vec::new();
    loop {
        let at = skip_whitespace(skeleton, from);
        if !views.skeleton[at..].starts_with(JOIN) {
            return (segments, from);
        }
        let mut arg = skip_whitespace(code, at + JOIN.len());
        if views.skeleton[arg..].starts_with('&') {
            arg = skip_whitespace(code, arg + 1);
        }
        let (value, end) = if let Some(literal) = string_literal_at(&views.code, arg) {
            (literal.to_owned(), literal_end(&views.skeleton, arg))
        } else {
            let end = ident_end(skeleton, arg);
            if end == arg {
                return (segments, from);
            }
            let value = constants
                .get(&views.skeleton[arg..end])
                .cloned()
                .unwrap_or_else(|| WILDCARD.to_owned());
            (value, end)
        };
        let close = skip_whitespace(skeleton, end);
        if !views.skeleton[close..].starts_with(')') {
            return (segments, from);
        }
        segments.push(value);
        from = close + 1;
    }
}

/// Offset one past the string literal starting at `start`.
///
/// The skeleton blanks a literal, quotes included, into spaces, so its end is
/// where the blanks stop.
fn literal_end(skeleton: &str, start: usize) -> usize {
    let bytes = skeleton.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end] == b' ' {
        end += 1;
    }
    end
}

/// The name bound by `let [mut] <name> = <expression from anchor to end>;`.
fn bound_name(skeleton: &str, anchor: usize, end: usize) -> Option<String> {
    let mut start = anchor;
    while let Some(qualified) = anchor_start(&skeleton[..start]) {
        start = qualified;
    }
    let before = skeleton[..start].trim_end().strip_suffix('=')?.trim_end();
    let name_start = before
        .bytes()
        .rposition(|byte| !is_ident(byte))
        .map_or(0, |index| index + 1);
    let name = &before[name_start..];
    let head = before[..name_start].trim_end();
    let head = head.strip_suffix("mut").map_or(head, str::trim_end);
    if name.is_empty() || !head.ends_with("let") {
        return None;
    }
    let after = skip_whitespace(skeleton.as_bytes(), end);
    skeleton[after..].starts_with(';').then(|| name.to_owned())
}

/// `const` and `static` string items of `file`, by name.
fn string_constants(file: &SourceFile) -> BTreeMap<String, String> {
    let views = file.views();
    let skeleton = views.skeleton.as_bytes();
    let mut constants = BTreeMap::new();
    for keyword in ["const ", "static "] {
        for (offset, _) in views.skeleton.match_indices(keyword) {
            if offset > 0 && is_ident(skeleton[offset - 1]) {
                continue;
            }
            let name_start = skip_whitespace(skeleton, offset + keyword.len());
            let name_end = ident_end(skeleton, name_start);
            let colon = skip_whitespace(skeleton, name_end);
            if name_end == name_start || !views.skeleton[colon..].starts_with(':') {
                continue;
            }
            let Some(eq) = views.skeleton[colon..].find('=').map(|i| colon + i) else {
                continue;
            };
            let ty = views.skeleton[colon + 1..eq].trim();
            if ty != "&str" && ty != "&'static str" {
                continue;
            }
            let value_at = skip_whitespace(views.code.as_bytes(), eq + 1);
            if let Some(value) = string_literal_at(&views.code, value_at) {
                constants.insert(
                    views.skeleton[name_start..name_end].to_owned(),
                    value.to_owned(),
                );
            }
        }
    }
    constants
}

/// String literals in the test code of `file`, as `(offset, content)`.
fn literals(file: &SourceFile) -> Vec<(usize, &str)> {
    let views = file.views();
    let code = views.code.as_bytes();
    let skeleton = views.skeleton.as_bytes();
    let mut found = Vec::new();
    let mut offset = 0;
    while offset < code.len() {
        let starts = code[offset] == b'"' && skeleton[offset] == b' ';
        if starts && file.in_test_code(offset) {
            if let Some(literal) = string_literal_at(&views.code, offset) {
                found.push((offset, literal));
                offset = literal_end(&views.skeleton, offset).max(offset + 1);
                continue;
            }
        }
        offset += 1;
    }
    found
}

/// `segments` joined below `start` with `.` and `..` resolved, or `None` when
/// a segment is absolute or the path leaves the repository. The repository
/// root itself is the empty string.
fn normalize(start: &str, segments: &[String]) -> Option<String> {
    let mut joined = PathBuf::from(start);
    for segment in segments {
        joined.push(segment);
    }
    let mut parts: Vec<String> = Vec::new();
    for component in joined.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

/// Whether the tracked `file` is named by `pattern` or lies below a path it names.
fn matches(pattern: &str, file: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let file: Vec<&str> = file.split('/').collect();
    file.len() >= pattern.len()
        && pattern
            .iter()
            .zip(&file)
            .all(|(want, have)| *want == WILDCARD || want == have)
}

/// The violations among `reads`, judged against the tracked files and `scans`.
pub(crate) fn check(
    reads: &[(String, Read)],
    packages: &[Package],
    tracked: &BTreeSet<String>,
    scans: &[(&str, &[&str], &str)],
    not_reads: &[(&str, &str, &str)],
) -> Vec<Violation> {
    let mut violations = BTreeSet::new();
    let mut scanned = BTreeSet::new();
    let mut excused = BTreeSet::new();
    for (reader, read) in reads {
        if let Some(entry) = not_reads
            .iter()
            .find(|(file, path, _)| *file == read.file && *path == read.path)
        {
            excused.insert((entry.0, entry.1));
            continue;
        }
        let paths: Vec<String> = if read.path.is_empty() {
            if let Some((file, scopes, _)) = scans.iter().find(|(file, ..)| *file == read.file) {
                scanned.insert(*file);
                scopes.iter().map(|scope| (*scope).to_owned()).collect()
            } else {
                violations.insert(Violation {
                    read: read.clone(),
                    reader: reader.clone(),
                    problem: "the file uses the repository root itself; declare what it \
                              reads in SCANS (crates/xtask/src/affected/guard.rs)"
                        .to_owned(),
                });
                continue;
            }
        } else {
            vec![read.path.clone()]
        };
        for path in paths {
            let mut named = tracked
                .iter()
                .filter(|file| matches(&path, file))
                .peekable();
            // A path that names no tracked file (an absence check, or an input
            // that moved) must still select the reader once a file is there.
            let absent = (read.anchored && named.peek().is_none() && !path.contains(WILDCARD))
                .then(|| uncovered(&path, reader, &read.file, packages).map(|why| (&path, why)))
                .flatten();
            let uncovered = absent.or_else(|| {
                named.find_map(|file| {
                    uncovered(file, reader, &read.file, packages).map(|why| (file, why))
                })
            });
            if let Some((file, why)) = uncovered {
                violations.insert(Violation {
                    read: Read {
                        path: path.clone(),
                        ..read.clone()
                    },
                    reader: reader.clone(),
                    problem: format!("a change to {} does not select it: {why}", printable(file)),
                });
            }
        }
    }
    // An entry for a file that is not tracked here judges nothing; only an
    // entry whose file exists and no longer has the use is stale.
    for (file, path, _) in not_reads {
        if tracked.contains(*file) && !excused.contains(&(*file, *path)) {
            violations.insert(Violation {
                read: Read {
                    file: (*file).to_owned(),
                    line: 0,
                    path: (*path).to_owned(),
                    anchored: false,
                },
                reader: String::from("NOT_READS"),
                problem: "the entry is stale: the file no longer has this read".to_owned(),
            });
        }
    }
    for (file, _, _) in scans {
        if tracked.contains(*file) && !scanned.contains(file) {
            violations.insert(Violation {
                read: Read {
                    file: (*file).to_owned(),
                    line: 0,
                    path: String::new(),
                    anchored: false,
                },
                reader: String::from("SCANS"),
                problem: "the entry is stale: the file no longer uses the repository root"
                    .to_owned(),
            });
        }
    }
    violations.into_iter().collect()
}

/// Whether `path` lies in the directory of package `reader`.
fn is_own(path: &str, reader: &str, packages: &[Package]) -> bool {
    packages.iter().any(|package| {
        package.name == reader
            && (package.dir.is_empty() || path == package.dir || is_below(path, &package.dir))
    })
}

/// The decision of `file` when its change does not select the test code of
/// package `reader` in source `source`.
///
/// A narrowed rule covers the read only when it names `source` among the
/// sources its filter selects; naming the package is not enough.
fn uncovered(file: &str, reader: &str, source: &str, packages: &[Package]) -> Option<String> {
    if is_own(file, reader, packages) {
        return None;
    }
    let (decision, _) = classify(file, packages, Mode::Local);
    match &decision {
        Decision::Everything(_) | Decision::Unmapped => None,
        Decision::Packages { names, tests, .. } => {
            let seeds = names
                .iter()
                .map(|name| {
                    (
                        name.clone(),
                        BTreeSet::from([Reason::Changed(String::new())]),
                    )
                })
                .collect();
            let selected = graph::close(&seeds, packages);
            let narrowed = tests.iter().any(|(package, ..)| package == reader)
                && names_source(file, reader, source);
            let covered = selected.contains_key(reader) || narrowed;
            (!covered).then(|| {
                format!(
                    "{} (no rule selects the tests in {})",
                    decision.describe(),
                    printable(source)
                )
            })
        }
        Decision::Dependencies(_) | Decision::NoRustTests(_) => Some(decision.describe()),
    }
}

/// Whether a narrowed rule for `file` names `source` as a test source of `reader`.
fn names_source(file: &str, reader: &str, source: &str) -> bool {
    RULES
        .iter()
        .filter(|rule| rule.pattern.matches(file))
        .flat_map(|rule| match rule.effect {
            Effect::Tests(entries) | Effect::AlsoTests(entries) => entries,
            _ => &[],
        })
        .filter(|entry| entry.package == reader)
        .flat_map(|entry| entry.files)
        .any(|named| source == *named || is_below(source, named))
}

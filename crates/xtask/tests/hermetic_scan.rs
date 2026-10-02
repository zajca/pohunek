//! Keeps test code off host state: no shared temp directories, no host `/tmp`,
//! no "bind port 0, drop, reuse the port" race.
//!
//! A test owns its fixtures. Directories come from `pohunek_test_support`
//! (`tempdir()` for a plain fixture root, `env::TestEnv` for a private home,
//! working directory and scrubbed child environment), never from the host's
//! shared temporary directory, and a socket a test needs stays bound.
//!
//! In test code (as classified by `xtask::test_code`), outside
//! `crates/test-support` and this file (its fixtures spell out the patterns),
//! this scan reports:
//! - `std::env::temp_dir()` and `env::temp_dir()`, also a bare `temp_dir()`
//!   imported from `std::env`;
//! - a string literal that holds the host temporary path `/tmp`,
//!   `/var/tmp` or `/private/tmp` as a path of its own (start of the literal,
//!   or after whitespace, a quote or one of `= : ; , ( [ < > | &`; a
//!   component such as `/x/tmp` or `{}/tmp` is not host `/tmp`), followed by
//!   the end of the literal, `/`, or any character that cannot extend a file
//!   name. The scan cannot tell a path that is used from a fake path that only
//!   feeds a validator, and a shell script that mentions `/tmp` writes there
//!   for real, so every such literal needs a rewrite (the migrations use
//!   `/work`) or a marker;
//! - `tempfile::tempdir()`, `tempdir_in(..)`, `tempfile()`, `TempDir::new()`,
//!   `TempDir::with_prefix(..)`, `NamedTempFile::new()` (and `with_prefix`,
//!   `with_suffix`), and a `tempfile::Builder` chain ending in `.tempdir(..)`,
//!   `.tempdir_in(..)` or `.tempfile(..)`. `NamedTempFile::new_in(dir)` and
//!   `.tempfile_in(dir)` name their directory and are allowed, and so is the
//!   type name `tempfile::TempDir`;
//! - a `TcpListener::bind(..)` of port 0 whose listener is not kept. Port 0
//!   in the bind argument is recognised as a string literal ending in `:0` or
//!   a trailing `, 0` operand (`("127.0.0.1", 0)`, `SocketAddr::new(ip, 0)`).
//!   Whether the listener is dropped cannot be decided from tokens alone, so
//!   the rule is conservative: the bind is accepted only when it initializes a
//!   named struct field, or when it is the whole initializer of a
//!   `let [mut] name = ..` (suffixes `?`, `.await`, `.unwrap()`, `.expect(..)`,
//!   `.map_err(..)` only; a name that is `_` or never `drop(name)`d in the
//!   enclosing block). Anything else, such as
//!   `bind(..).unwrap().local_addr().unwrap().port()`, consumes or reduces the
//!   listener and is reported; hand the bound listener to the code under test
//!   (`from_std`, a socket path from `TestEnv::socket_path`) instead.
//!
//! An occurrence on a line that carries, or follows a line that carries, a
//! comment starting `// hermetic-allowed: #<issue> <reason>` is exempt. The
//! marker must name an issue number and a non-empty reason; a malformed
//! marker is itself a failure. The scan does not look the issue up.
//!
//! This is a zero-violation gate: there is no baseline. To survey another
//! checkout, run the ignored `survey_hermetic_violations` test with
//! `POHUNEK_HERMETIC_SCAN_ROOT=<workspace root>`.

// Rust guideline compliant 2026-10-02

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use xtask::test_code::{classify, is_ident, read_sources, skip_whitespace, SourceFile};

/// Comment keyword that exempts an occurrence (after `// `).
const MARKER: &str = "hermetic-allowed";

/// Paths the scan skips: the crate that owns the sanctioned temp helpers, and
/// this file, whose fixtures spell out every pattern it looks for.
const EXEMPT_PATHS: [&str; 2] = [
    "crates/test-support/",
    "crates/xtask/tests/hermetic_scan.rs",
];

/// Host temporary directories a literal must not name.
const HOST_TEMP_PATHS: [&str; 3] = ["/tmp", "/var/tmp", "/private/tmp"];

/// Characters that may precede a host temp path inside a literal.
const PATH_START_DELIMITERS: &str = "\"'= :;,([<>|&\t\n\r";

/// Characters that extend a file name, so `/tmp` followed by one is another path.
const NAME_CHARS: &str = "_.-";

/// Methods that may follow a bind and still leave the listener as the value.
const LISTENER_SUFFIXES: [&str; 4] = ["unwrap", "expect", "map_err", "await"];

/// Environment variable naming the checkout the survey test scans.
const SURVEY_ROOT_VAR: &str = "POHUNEK_HERMETIC_SCAN_ROOT";

/// Which rule an occurrence breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Rule {
    TempDir,
    TmpLiteral,
    Tempfile,
    PortZeroBind,
}

impl Rule {
    fn label(self) -> &'static str {
        match self {
            Self::TempDir => "std::env::temp_dir()",
            Self::TmpLiteral => "host /tmp path literal",
            Self::Tempfile => "tempfile crate fixture outside pohunek_test_support",
            Self::PortZeroBind => "TcpListener::bind(..:0) whose listener is not kept",
        }
    }

    fn fix(self) -> &'static str {
        match self {
            Self::TempDir | Self::Tempfile => {
                "use `pohunek_test_support::tempdir()` for a fixture root or \
                 `pohunek_test_support::env::TestEnv` for a private home, cwd and TMPDIR"
            }
            Self::TmpLiteral => {
                "use a path below a fixture root, or a neutral fake path such as `/work`"
            }
            Self::PortZeroBind => {
                "keep the bound listener and hand it to the code under test, or use a \
                 socket path from `TestEnv::socket_path`; never bind port 0, drop it and \
                 reuse the number"
            }
        }
    }
}

/// One unexempted occurrence.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Violation {
    path: String,
    line: usize,
    rule: Rule,
}

/// A `hermetic-allowed` comment that does not name an issue and a reason.
#[derive(Debug, PartialEq, Eq)]
struct Malformed {
    path: String,
    line: usize,
}

/// Result of scanning a set of files.
#[derive(Debug, Default)]
struct Scan {
    violations: Vec<Violation>,
    malformed: Vec<Malformed>,
}

/// Index of the byte matching the opener at `open` in `bytes`, or the length.
fn matching_close(bytes: &[u8], open: usize) -> usize {
    let (opener, closer) = match bytes[open] {
        b'[' => (b'[', b']'),
        b'(' => (b'(', b')'),
        _ => (b'{', b'}'),
    };
    let mut depth = 0_usize;
    for (i, &byte) in bytes.iter().enumerate().skip(open) {
        if byte == opener {
            depth += 1;
        } else if byte == closer {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    bytes.len()
}

/// Index one past the identifier that starts at `start`.
fn end_of_ident(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while bytes.get(end).copied().is_some_and(is_ident) {
        end += 1;
    }
    end
}

/// Whether an identifier-bounded word starts at `offset`.
fn starts_word(bytes: &[u8], offset: usize) -> bool {
    offset == 0 || !is_ident(bytes[offset - 1])
}

/// The identifier that ends right before `end`, skipping whitespace first.
fn ident_before(bytes: &[u8], end: usize) -> &str {
    let mut stop = end;
    while stop > 0 && bytes[stop - 1].is_ascii_whitespace() {
        stop -= 1;
    }
    let mut start = stop;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    std::str::from_utf8(&bytes[start..stop]).unwrap_or("")
}

/// Index just past `token` when it follows `from` after optional whitespace.
fn expect_token(bytes: &[u8], from: usize, token: &str) -> Option<usize> {
    let at = skip_whitespace(bytes, from);
    bytes[at..]
        .starts_with(token.as_bytes())
        .then_some(at + token.len())
}

/// Whether `text` contains `word` bounded by non-identifier bytes.
fn contains_word(text: &str, word: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices(word).any(|(at, _)| {
        starts_word(bytes, at) && !bytes.get(at + word.len()).copied().is_some_and(is_ident)
    })
}

/// What precedes the identifier at `offset`.
#[derive(Debug, PartialEq, Eq)]
enum Qualifier<'a> {
    /// A method call (`.name`).
    Method,
    /// A path: the segment before `::`.
    Path(&'a str),
    /// A plain name.
    Bare,
}

fn qualifier(bytes: &[u8], offset: usize) -> Qualifier<'_> {
    let mut before = offset;
    while before > 0 && bytes[before - 1].is_ascii_whitespace() {
        before -= 1;
    }
    if before > 0 && bytes[before - 1] == b'.' {
        Qualifier::Method
    } else if before >= 2 && &bytes[before - 2..before] == b"::" {
        Qualifier::Path(ident_before(bytes, before - 2))
    } else {
        Qualifier::Bare
    }
}

/// Names imported by `use` statements whose text mentions `module`.
fn imported_names<'a>(skeleton: &'a str, module: &str, names: &[&'a str]) -> Vec<&'a str> {
    let bytes = skeleton.as_bytes();
    let mut found = Vec::new();
    for (at, _) in skeleton.match_indices("use ") {
        if !starts_word(bytes, at) {
            continue;
        }
        let end = skeleton[at..].find(';').map_or(skeleton.len(), |n| at + n);
        let statement = &skeleton[at..end];
        if !contains_word(statement, module) {
            continue;
        }
        found.extend(
            names
                .iter()
                .copied()
                .filter(|name| contains_word(statement, name)),
        );
    }
    found
}

/// Offsets of identifier `name` followed by `(` (or, when `call` is false, any
/// use), with the qualifier that precedes it.
fn named_uses<'a>(skeleton: &'a str, name: &str, call: bool) -> Vec<(usize, Qualifier<'a>)> {
    let bytes = skeleton.as_bytes();
    skeleton
        .match_indices(name)
        .filter(|&(offset, _)| {
            starts_word(bytes, offset) && end_of_ident(bytes, offset) == offset + name.len()
        })
        .filter(|&(offset, _)| {
            !call || bytes.get(skip_whitespace(bytes, offset + name.len())) == Some(&b'(')
        })
        .map(|(offset, _)| (offset, qualifier(bytes, offset)))
        .collect()
}

/// `env::temp_dir` uses.
fn temp_dir_candidates(skeleton: &str) -> Vec<usize> {
    let bytes = skeleton.as_bytes();
    let imported = !imported_names(skeleton, "env", &["temp_dir"]).is_empty();
    let mut found = Vec::new();
    for (offset, qualifier) in named_uses(skeleton, "temp_dir", false) {
        let is_call = bytes.get(skip_whitespace(bytes, offset + "temp_dir".len())) == Some(&b'(');
        let hit = match qualifier {
            Qualifier::Path("env") => true,
            Qualifier::Bare => is_call && imported && ident_before(bytes, offset) != "fn",
            Qualifier::Path(_) | Qualifier::Method => false,
        };
        if hit {
            found.push(offset);
        }
    }
    found
}

/// Whether the host temp path at `at` in `text` stands alone.
fn is_host_temp_path(text: &str, at: usize, needle: &str) -> bool {
    let before = text[..at].chars().next_back();
    let after = text[at + needle.len()..].chars().next();
    before.is_none_or(|c| PATH_START_DELIMITERS.contains(c))
        && after.is_none_or(|c| !(c.is_ascii_alphanumeric() || NAME_CHARS.contains(c)))
}

/// Offsets of host temp paths inside string literals.
fn tmp_literal_candidates(file: &SourceFile) -> Vec<usize> {
    let views = file.views();
    let (code, skeleton) = (views.code.as_bytes(), views.skeleton.as_bytes());
    let mut found = Vec::new();
    let mut run_start = None;
    for i in 0..=code.len() {
        let in_literal = i < code.len() && code[i] != skeleton[i];
        match (in_literal, run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(start)) => {
                run_start = None;
                // Literal runs are ASCII-delimited, so both ends are char boundaries.
                let Some(text) = views.code.get(start..i) else {
                    continue;
                };
                for needle in HOST_TEMP_PATHS {
                    found.extend(
                        text.match_indices(needle)
                            .filter(|&(at, _)| is_host_temp_path(text, at, needle))
                            .map(|(at, _)| start + at),
                    );
                }
            }
            _ => {}
        }
    }
    found
}

/// End of the statement or expression that contains `from`: the first `;`
/// outside nested groups, or the unmatched closer of the enclosing group.
fn end_of_statement(bytes: &[u8], from: usize) -> usize {
    let mut depth = 0_usize;
    for (i, &byte) in bytes.iter().enumerate().skip(from) {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            b';' if depth == 0 => return i,
            _ => {}
        }
    }
    bytes.len()
}

/// `tempfile` crate fixtures that create entries in the host temp directory
/// or take their root from outside the test-support helpers.
fn tempfile_candidates(skeleton: &str) -> Vec<usize> {
    const NAMES: [&str; 6] = [
        "tempdir",
        "tempdir_in",
        "tempfile",
        "NamedTempFile",
        "TempDir",
        "Builder",
    ];
    let bytes = skeleton.as_bytes();
    let imported = imported_names(skeleton, "tempfile", &NAMES);
    let mut found = Vec::new();
    for function in ["tempdir", "tempdir_in", "tempfile"] {
        for (offset, qualifier) in named_uses(skeleton, function, true) {
            let hit = match qualifier {
                Qualifier::Path("tempfile") => true,
                Qualifier::Bare => imported.contains(&function),
                Qualifier::Path(_) | Qualifier::Method => false,
            };
            if hit {
                found.push(offset);
            }
        }
    }
    for (kind, constructors) in [
        ("NamedTempFile", ["new", "with_prefix", "with_suffix"]),
        ("TempDir", ["new", "with_prefix", "with_suffix"]),
    ] {
        for (offset, qualifier) in named_uses(skeleton, kind, false) {
            let from_tempfile = match qualifier {
                Qualifier::Path("tempfile") => true,
                Qualifier::Bare => imported.contains(&kind),
                Qualifier::Path(_) | Qualifier::Method => false,
            };
            if !from_tempfile {
                continue;
            }
            let after = offset + kind.len();
            let constructor = expect_token(bytes, after, "::").map(|at| skip_whitespace(bytes, at));
            let hit = constructor.is_some_and(|at| {
                let end = end_of_ident(bytes, at);
                constructors.contains(&&skeleton[at..end])
                    && bytes.get(skip_whitespace(bytes, end)) == Some(&b'(')
            });
            if hit {
                found.push(offset);
            }
        }
    }
    for (offset, qualifier) in named_uses(skeleton, "Builder", false) {
        let is_tempfile = match qualifier {
            Qualifier::Path("tempfile") => true,
            Qualifier::Bare => imported.contains(&"Builder"),
            Qualifier::Path(_) | Qualifier::Method => false,
        };
        if !is_tempfile {
            continue;
        }
        let end = end_of_statement(bytes, offset);
        let statement = &skeleton[offset..end];
        for method in [".tempdir", ".tempdir_in", ".tempfile"] {
            for (at, _) in statement.match_indices(method) {
                let after = offset + at + method.len();
                if bytes.get(skip_whitespace(bytes, after)) == Some(&b'(') {
                    found.push(offset + at + 1);
                }
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// Whether the bind argument (code view) names port 0.
fn binds_port_zero(argument_code: &str, argument_skeleton: &str) -> bool {
    let trailing = argument_skeleton
        .trim_end_matches(|c: char| c.is_whitespace() || c == ')' || c == ']')
        .trim_end();
    argument_code.contains(":0\"")
        || trailing.ends_with(", 0")
        || trailing.ends_with(",0")
        || trailing.ends_with(", 0u16")
}

/// Whether the rest of a statement (after the bind call) only unwraps the
/// listener: `?`, `.await`, `.unwrap()`, `.expect(..)`, `.map_err(..)`.
fn only_unwraps(bytes: &[u8], from: usize, to: usize) -> bool {
    let mut at = skip_whitespace(bytes, from);
    while at < to {
        if bytes[at] == b'?' {
            at = skip_whitespace(bytes, at + 1);
            continue;
        }
        if bytes[at] != b'.' {
            return false;
        }
        let name_start = skip_whitespace(bytes, at + 1);
        let name_end = end_of_ident(bytes, name_start);
        let name = std::str::from_utf8(&bytes[name_start..name_end]).unwrap_or("");
        if !LISTENER_SUFFIXES.contains(&name) {
            return false;
        }
        at = skip_whitespace(bytes, name_end);
        if bytes.get(at) == Some(&b'(') {
            at = skip_whitespace(bytes, matching_close(bytes, at) + 1);
        }
    }
    true
}

/// Name bound by a `let` whose initializer starts at `bind_path_start`, if the
/// text between the statement start and the initializer has that shape.
fn let_binding(skeleton: &str, bind_path_start: usize) -> Option<&str> {
    let bytes = skeleton.as_bytes();
    let statement_start = bytes[..bind_path_start]
        .iter()
        .rposition(|&b| matches!(b, b';' | b'{' | b'}'))
        .map_or(0, |at| at + 1);
    let head = skeleton[statement_start..bind_path_start].trim_end();
    let head = head.strip_suffix('=')?.trim_end();
    let head = head.trim_start().strip_prefix("let")?;
    if !head.starts_with(char::is_whitespace) {
        return None;
    }
    let head = head.trim_start();
    let head = head
        .strip_prefix("mut")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map_or(head, str::trim_start);
    let name_end = head
        .bytes()
        .position(|b| !is_ident(b))
        .unwrap_or(head.len());
    let (name, rest) = head.split_at(name_end);
    let rest = rest.trim_start();
    (!name.is_empty() && name != "_" && (rest.is_empty() || rest.starts_with(':'))).then_some(name)
}

/// Whether a field initializer `name: <bind>` precedes the bind path.
fn is_field_initializer(bytes: &[u8], bind_path_start: usize) -> bool {
    let mut before = bind_path_start;
    while before > 0 && bytes[before - 1].is_ascii_whitespace() {
        before -= 1;
    }
    if before == 0 || bytes[before - 1] != b':' || (before >= 2 && bytes[before - 2] == b':') {
        return false;
    }
    bytes[..before - 1]
        .iter()
        .rev()
        .find(|b| !b.is_ascii_whitespace())
        .copied()
        .is_some_and(is_ident)
}

/// Whether `name` is dropped explicitly somewhere in the block that follows.
fn dropped_later(skeleton: &str, from: usize, name: &str) -> bool {
    let bytes = skeleton.as_bytes();
    let mut depth = 0_usize;
    let mut block_end = bytes.len();
    for (i, &byte) in bytes.iter().enumerate().skip(from) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                if depth == 0 {
                    block_end = i;
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let block = &skeleton[from..block_end];
    named_uses(block, "drop", true)
        .into_iter()
        .any(|(offset, _)| {
            let open = skip_whitespace(block.as_bytes(), offset + "drop".len());
            let close = matching_close(block.as_bytes(), open);
            block[open + 1..close].trim() == name
        })
}

/// Port-0 `TcpListener::bind` calls whose listener is not demonstrably kept.
fn bind_candidates(file: &SourceFile) -> Vec<usize> {
    let views = file.views();
    let (skeleton, bytes) = (&views.skeleton, views.skeleton.as_bytes());
    let mut found = Vec::new();
    for (offset, qualifier) in named_uses(skeleton, "bind", true) {
        let Qualifier::Path("TcpListener") = qualifier else {
            continue;
        };
        let open = skip_whitespace(bytes, offset + "bind".len());
        let close = matching_close(bytes, open);
        if !binds_port_zero(&views.code[open + 1..close], &skeleton[open + 1..close]) {
            continue;
        }
        // Start of the whole path expression (`std::net::TcpListener::bind`).
        let mut start = offset;
        while start > 0 && (is_ident(bytes[start - 1]) || bytes[start - 1] == b':') {
            start -= 1;
        }
        let statement_end = end_of_statement(bytes, close + 1);
        let kept = if is_field_initializer(bytes, start) {
            true
        } else {
            let_binding(skeleton, start).is_some_and(|name| {
                only_unwraps(bytes, close + 1, statement_end)
                    && !dropped_later(skeleton, statement_end, name)
            })
        };
        if !kept {
            found.push(offset);
        }
    }
    found
}

/// Lines (one-based) carrying a valid marker and the malformed marker lines.
fn markers(file: &SourceFile, text: &str) -> (Vec<usize>, Vec<usize>) {
    let code = file.views().code.as_bytes();
    let mut valid = Vec::new();
    let mut malformed = Vec::new();
    let mut line_start = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let comment = line
            .match_indices("//")
            .map(|(at, _)| at)
            .find(|&at| code[line_start + at] == b' ');
        if let Some(rest) = comment.map(|at| &line[at + 2..]) {
            if !rest.starts_with(['/', '!']) {
                if let Some(body) = rest.trim_start().strip_prefix(MARKER) {
                    if is_valid_marker(body) {
                        valid.push(index + 1);
                    } else {
                        malformed.push(index + 1);
                    }
                }
            }
        }
        line_start += line.len();
    }
    (valid, malformed)
}

/// Whether the text after `hermetic-allowed` is `: #<digits> <reason>`.
fn is_valid_marker(body: &str) -> bool {
    let Some(rest) = body.strip_prefix(':') else {
        return false;
    };
    let Some(rest) = rest.trim().strip_prefix('#') else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0
        && rest[digits..].starts_with(char::is_whitespace)
        && !rest[digits..].trim().is_empty()
}

/// Scans `files` (workspace-relative path to text).
fn scan_files(files: &BTreeMap<String, String>) -> Scan {
    let mut scan = Scan::default();
    for (path, file) in classify(files) {
        let text = &files[&path];
        let (valid, malformed) = markers(&file, text);
        scan.malformed
            .extend(malformed.into_iter().map(|line| Malformed {
                path: path.clone(),
                line,
            }));
        if EXEMPT_PATHS.iter().any(|exempt| path.starts_with(exempt)) {
            continue;
        }
        let skeleton = &file.views().skeleton;
        let mut candidates: Vec<(usize, Rule)> = Vec::new();
        candidates.extend(
            temp_dir_candidates(skeleton)
                .into_iter()
                .map(|at| (at, Rule::TempDir)),
        );
        candidates.extend(
            tmp_literal_candidates(&file)
                .into_iter()
                .map(|at| (at, Rule::TmpLiteral)),
        );
        candidates.extend(
            tempfile_candidates(skeleton)
                .into_iter()
                .map(|at| (at, Rule::Tempfile)),
        );
        candidates.extend(
            bind_candidates(&file)
                .into_iter()
                .map(|at| (at, Rule::PortZeroBind)),
        );
        for (offset, rule) in candidates {
            let line = file.line_of(offset);
            let allowed = valid.contains(&line) || valid.contains(&line.saturating_sub(1));
            if file.in_test_code(offset) && !allowed {
                scan.violations.push(Violation {
                    path: path.clone(),
                    line,
                    rule,
                });
            }
        }
    }
    scan.violations.sort();
    scan
}

/// One failure line per occurrence; empty when the gate holds.
fn evaluate(scan: &Scan) -> Vec<String> {
    let mut failures = Vec::new();
    for marker in &scan.malformed {
        failures.push(format!(
            "{}:{}: malformed marker; write `// {MARKER}: #<issue> <reason>` with an issue \
             number and a non-empty reason",
            marker.path, marker.line
        ));
    }
    for violation in &scan.violations {
        failures.push(format!(
            "{}:{}: {}",
            violation.path,
            violation.line,
            violation.rule.label()
        ));
    }
    failures
}

/// How to fix each rule that `scan` broke, with the marker escape hatch.
fn guidance(scan: &Scan) -> String {
    let rules: BTreeSet<Rule> = scan.violations.iter().map(|v| v.rule).collect();
    let mut text = String::new();
    for rule in rules {
        let _ = writeln!(text, "- {}: {}", rule.label(), rule.fix());
    }
    let _ = write!(
        text,
        "A case where the host state is the subject of the test takes \
         `// {MARKER}: #<issue> <reason>` on or above the line."
    );
    text
}

fn scan_root(root: &std::path::Path) -> Scan {
    let files = read_sources(root).unwrap_or_else(|e| panic!("read workspace sources: {e}"));
    scan_files(&files)
}

#[test]
fn test_code_is_hermetic() {
    let scan = scan_root(&pohunek_test_support::workspace_root());
    let failures = evaluate(&scan);
    assert!(
        failures.is_empty(),
        "{} hermeticity violation(s):\n{}\n\n{}",
        failures.len(),
        failures.join("\n"),
        guidance(&scan)
    );
}

#[test]
#[ignore = "prints the violations of another checkout; set POHUNEK_HERMETIC_SCAN_ROOT"]
fn survey_hermetic_violations() {
    let root = std::env::var_os(SURVEY_ROOT_VAR)
        .map_or_else(pohunek_test_support::workspace_root, PathBuf::from);
    let scan = scan_root(&root);
    let mut report = String::new();
    for failure in &scan.malformed {
        let _ = writeln!(report, "MALFORMED {}:{}", failure.path, failure.line);
    }
    for violation in &scan.violations {
        let _ = writeln!(
            report,
            "{:?} {}:{}",
            violation.rule, violation.path, violation.line
        );
    }
    println!("{report}");
}

/// Scan of one file at `path`.
fn scan_one(path: &str, text: &str) -> Scan {
    scan_files(&BTreeMap::from([(path.to_owned(), text.to_owned())]))
}

const DEMO: &str = "crates/demo/src/lib.rs";

/// `body` as the body of a test fn, so its statements are test code.
fn in_test(body: &str) -> String {
    format!("#[test]\nfn f() {{\n{body}\n}}\n")
}

fn rules_of(scan: &Scan) -> Vec<(usize, Rule)> {
    scan.violations.iter().map(|v| (v.line, v.rule)).collect()
}

fn flagged(body: &str) -> Vec<(usize, Rule)> {
    rules_of(&scan_one(DEMO, &in_test(body)))
}

#[test]
fn env_temp_dir_is_flagged_with_any_path_prefix() {
    for call in [
        "let p = std::env::temp_dir();",
        "let p = env::temp_dir();",
        "let p = ::std::env::temp_dir ();",
        "let p = std::env :: temp_dir();",
        "let f = std::env::temp_dir;",
    ] {
        assert_eq!(flagged(call), [(3, Rule::TempDir)], "{call}");
    }
    let imported = "use std::env::temp_dir;\n#[test]\nfn f() {\n    let p = temp_dir();\n}\n";
    assert_eq!(rules_of(&scan_one(DEMO, imported)), [(4, Rule::TempDir)]);
    let grouped =
        "use std::env::{self, temp_dir};\n#[test]\nfn f() {\n    let p = temp_dir();\n}\n";
    assert_eq!(rules_of(&scan_one(DEMO, grouped)), [(4, Rule::TempDir)]);
}

#[test]
fn temp_dir_lookalikes_are_not_flagged() {
    for call in [
        "let p = pohunek_test_support::temp_root();",
        "let p = builder.temp_dir();",
        "let p = temp_dir();",
        "let p = other::temp_dir();",
        "fn temp_dir() {}",
        "let temp_dir = 1;",
        "let s = \"std::env::temp_dir()\";",
        "// std::env::temp_dir();",
    ] {
        assert!(flagged(call).is_empty(), "{call}");
    }
    let local = "use other::temp_dir;\n#[test]\nfn f() {\n    let p = temp_dir();\n}\n";
    assert!(scan_one(DEMO, local).violations.is_empty());
}

#[test]
fn host_tmp_literals_are_flagged() {
    for literal in [
        r#"let p = Path::new("/tmp");"#,
        r#"let p = Path::new("/tmp/work");"#,
        r#"let p = PathBuf::from("/var/tmp/x");"#,
        r#"let p = "/private/tmp";"#,
        r##"let p = r#"/tmp/raw"#;"##,
        r#"let s = "cd /tmp && ls";"#,
        r#"let s = "TMPDIR=/tmp sh";"#,
        r#"let s = "PATH=/usr/bin:/tmp";"#,
        r#"let s = "touch '/tmp/x'";"#,
        r#"let s = "a,/tmp/x";"#,
        r#"let s = b"/tmp/bytes";"#,
        "let s = \"line one\n/tmp/two\";",
    ] {
        let found = flagged(literal);
        assert_eq!(found.len(), 1, "{literal}: {found:?}");
        assert_eq!(found[0].1, Rule::TmpLiteral, "{literal}");
    }
}

#[test]
fn tmp_lookalikes_are_not_flagged() {
    for literal in [
        r#"let p = "/tmpfile";"#,
        r#"let p = "/tmp.txt";"#,
        r#"let p = "/tmp-x/y";"#,
        r#"let p = "/work/tmp";"#,
        r#"let p = "/x/tmp/y";"#,
        r#"let p = format!("{}/tmp", base);"#,
        r#"let p = "~/tmp";"#,
        r#"let p = "relative/tmp/x";"#,
        r#"let p = "/temporary";"#,
        r#"let p = "/var/tmpfs";"#,
        "// let p = \"/tmp\";",
        "/* \"/tmp\" */",
    ] {
        assert!(
            flagged(literal).is_empty(),
            "{literal}: {:?}",
            flagged(literal)
        );
    }
}

#[test]
fn tempfile_fixtures_are_flagged() {
    for call in [
        "let d = tempfile::tempdir().unwrap();",
        "let d = tempfile::tempdir_in(base).unwrap();",
        "let f = tempfile::tempfile().unwrap();",
        "let d = ::tempfile::tempdir().unwrap();",
        "let d = tempfile::TempDir::new().unwrap();",
        "let d = tempfile::TempDir::with_prefix(\"x\").unwrap();",
        "let f = tempfile::NamedTempFile::new().unwrap();",
        "let f = tempfile::NamedTempFile::with_suffix(\".rs\").unwrap();",
        "let d = tempfile::Builder::new().prefix(\"x\").tempdir().unwrap();",
        "let d = tempfile::Builder::new().prefix(\"x\").tempdir_in(base).unwrap();",
        "let f = tempfile::Builder::new().suffix(\".x\").tempfile().unwrap();",
        "let d = tempfile::Builder::new()\n        .prefix(\"x\")\n        .tempdir()\n        .unwrap();",
    ] {
        let found = flagged(call);
        assert_eq!(found.len(), 1, "{call}: {found:?}");
        assert_eq!(found[0].1, Rule::Tempfile, "{call}");
        assert_eq!(found[0].0, if call.contains('\n') { 5 } else { 3 }, "{call}");
    }
    for (import, call) in [
        ("use tempfile::tempdir;", "let d = tempdir().unwrap();"),
        (
            "use tempfile::{tempdir_in, TempDir};",
            "let d = tempdir_in(b).unwrap();",
        ),
        (
            "use tempfile::NamedTempFile;",
            "let f = NamedTempFile::new().unwrap();",
        ),
        (
            "use tempfile::Builder;",
            "let d = Builder::new().tempdir().unwrap();",
        ),
    ] {
        let text = format!("{import}\n{}", in_test(call));
        let found = rules_of(&scan_one(DEMO, &text));
        assert_eq!(found, [(4, Rule::Tempfile)], "{import} {call}");
    }
}

#[test]
fn allowed_tempfile_shapes_and_lookalikes_are_not_flagged() {
    for call in [
        "let d = pohunek_test_support::tempdir().unwrap();",
        "let d = pohunek_test_support::tempdir_with_prefix(\"x\").unwrap();",
        "let f = tempfile::NamedTempFile::new_in(dir).unwrap();",
        "let f = tempfile::Builder::new().tempfile_in(dir).unwrap();",
        "let d: tempfile::TempDir = fixture();",
        "let d = tempdir();",
        "let d = other::tempdir();",
        "let b = other::Builder::new().tempdir();",
        "let n = TempDir::new_thing();",
        "let d = TempDir::new(\"x\");",
        "let d = helpers::TempDir::new(\"x\");",
        "let f = NamedTempFile::new();",
        "fn tempdir() {}",
        "let s = \"tempfile::tempdir()\";",
        "// tempfile::tempdir();",
    ] {
        assert!(flagged(call).is_empty(), "{call}: {:?}", flagged(call));
    }
    let type_only =
        "use tempfile::TempDir;\n#[test]\nfn f() {\n    let d: TempDir = fixture();\n}\n";
    assert!(scan_one(DEMO, type_only).violations.is_empty());
    let local = "use other::tempdir;\n#[test]\nfn f() {\n    let d = tempdir();\n}\n";
    assert!(scan_one(DEMO, local).violations.is_empty());
}

#[test]
fn a_port_zero_bind_that_is_not_kept_is_flagged() {
    for call in [
        "let port = TcpListener::bind(\"127.0.0.1:0\").unwrap().local_addr().unwrap().port();",
        "let port = std::net::TcpListener::bind(\"127.0.0.1:0\").unwrap().local_addr().unwrap().port();",
        "let addr = tokio::net::TcpListener::bind(\"127.0.0.1:0\").await.unwrap().local_addr().unwrap();",
        "TcpListener::bind(\"[::1]:0\").unwrap();",
        "let _ = TcpListener::bind(\"127.0.0.1:0\").unwrap();",
        "let _listener_never_named = 1; TcpListener::bind((\"127.0.0.1\", 0)).unwrap();",
        "let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    let port = l.local_addr().unwrap().port();\n    drop(l);",
        "let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    std::mem::drop(l);",
        "let port = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap().local_addr().unwrap().port();",
        "let port = TcpListener::bind(SocketAddr::new(ip, 0)).unwrap().local_addr().unwrap().port();",
        "let l = TcpListener::bind(\"127.0.0.1:0\").unwrap().into_std();",
        "let r = match 1 { _ => TcpListener::bind(\"127.0.0.1:0\").unwrap() };",
        "serve(TcpListener::bind(\"127.0.0.1:0\").unwrap());",
    ] {
        let found = flagged(call);
        assert_eq!(found.len(), 1, "{call}: {found:?}");
        assert_eq!(found[0].1, Rule::PortZeroBind, "{call}");
    }
}

#[test]
fn a_kept_port_zero_bind_and_other_binds_are_not_flagged() {
    for call in [
        "let listener = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    let port = listener.local_addr().unwrap().port();\n    serve(listener, port);",
        "let mut listener = std::net::TcpListener::bind(\"127.0.0.1:0\").expect(\"bind\");",
        "let listener: TcpListener = TcpListener::bind(\"127.0.0.1:0\").await.unwrap();",
        "let _listener = TcpListener::bind(\"127.0.0.1:0\")?;",
        "let listener = tokio::net::TcpListener::bind((\"127.0.0.1\", 0)).await.map_err(fail).unwrap();",
        "let s = Server { listener: TcpListener::bind(\"127.0.0.1:0\").unwrap(), port: 1 };",
        "let s = Server {\n        listener:\n            TcpListener::bind(\"127.0.0.1:0\").unwrap(),\n    };",
        "let l = TcpListener::bind(\"127.0.0.1:8080\").unwrap().local_addr();",
        "let l = TcpListener::bind(\"127.0.0.1:10\").unwrap().local_addr();",
        "let l = TcpListener::bind(addr).unwrap().local_addr();",
        "let l = UdpSocket::bind(\"127.0.0.1:0\").unwrap().local_addr();",
        "let l = other::bind(\"127.0.0.1:0\").unwrap().local_addr();",
        "let l = listener.bind(\"127.0.0.1:0\");",
        "let s = \"TcpListener::bind(\\\"127.0.0.1:0\\\")\";",
        "// TcpListener::bind(\"127.0.0.1:0\").unwrap().local_addr();",
    ] {
        assert!(flagged(call).is_empty(), "{call}: {:?}", flagged(call));
    }
    let dropped_elsewhere = in_test(
        "let a = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    { let b = 1; drop(b); }\n    serve(a);",
    );
    assert!(scan_one(DEMO, &dropped_elsewhere).violations.is_empty());
}

#[test]
fn a_helper_that_returns_a_dropped_free_port_is_flagged() {
    let text = "#[cfg(test)]\nmod tests {\n    fn free_port() -> u16 {\n        std::net::TcpListener::bind(\"127.0.0.1:0\")\n            .unwrap()\n            .local_addr()\n            .unwrap()\n            .port()\n    }\n}\n";
    assert_eq!(rules_of(&scan_one(DEMO, text)), [(4, Rule::PortZeroBind)]);
}

#[test]
fn only_test_code_is_scanned_and_test_support_is_exempt() {
    let body = "fn f() {\n    let p = std::env::temp_dir();\n}\n";
    assert!(scan_one(DEMO, body).violations.is_empty());
    assert_eq!(
        rules_of(&scan_one("crates/demo/tests/run.rs", body)),
        [(2, Rule::TempDir)]
    );
    let mixed = format!("#[cfg(test)]\nmod tests {{\n{body}}}\n{body}");
    assert_eq!(rules_of(&scan_one(DEMO, &mixed)), [(4, Rule::TempDir)]);
    let helper =
        "pub fn tempdir() {\n    let p = std::env::temp_dir();\n    let q = \"/tmp\";\n}\n";
    assert!(scan_one("crates/test-support/src/lib.rs", helper)
        .violations
        .is_empty());
    assert!(scan_one("crates/test-support/tests/x.rs", helper)
        .violations
        .is_empty());
    assert!(!scan_one("crates/test-supporter/tests/x.rs", helper)
        .violations
        .is_empty());
}

#[test]
fn a_valid_marker_on_or_above_the_line_exempts_the_occurrence() {
    let forms = [
        "    let p = std::env::temp_dir(); // hermetic-allowed: #363 asserts the host temp dir\n",
        "    // hermetic-allowed: #363 asserts the host temp dir\n    let p = std::env::temp_dir();\n",
        "    //hermetic-allowed:#363 reason\n    let p = std::env::temp_dir();\n",
        "    // hermetic-allowed: #1 r\n    let p = \"/tmp\";\n",
        "    // hermetic-allowed: #363 the sticky bit of the real /tmp is the subject\n    let p = Path::new(\"/tmp\");\n",
    ];
    for form in forms {
        let scan = scan_one(DEMO, &format!("#[test]\nfn f() {{\n{form}}}\n"));
        assert!(scan.violations.is_empty(), "{form}: {:?}", scan.violations);
        assert!(scan.malformed.is_empty(), "{form}");
    }
    let two_above =
        "#[test]\nfn f() {\n    // hermetic-allowed: #363 reason\n\n    let p = std::env::temp_dir();\n}\n";
    assert_eq!(scan_one(DEMO, two_above).violations.len(), 1);
    let other_line = "#[test]\nfn f() {\n    let a = 1; // hermetic-allowed: #363 reason\n    let b = 2;\n    let p = std::env::temp_dir();\n}\n";
    assert_eq!(scan_one(DEMO, other_line).violations.len(), 1);
}

#[test]
fn a_marker_covers_a_bind_a_tempfile_call_and_a_literal() {
    let text = "#[test]\nfn f() {\n    // hermetic-allowed: #363 port reuse is the subject\n    let p = TcpListener::bind(\"127.0.0.1:0\").unwrap().local_addr().unwrap().port();\n    // hermetic-allowed: #363 reason\n    let d = tempfile::tempdir().unwrap();\n    let s = \"/tmp\"; // hermetic-allowed: #363 reason\n}\n";
    let scan = scan_one(DEMO, text);
    assert!(scan.violations.is_empty(), "{:?}", scan.violations);
}

#[test]
fn a_malformed_marker_is_a_failure_and_does_not_exempt() {
    let forms = [
        "// hermetic-allowed\n",
        "// hermetic-allowed:\n",
        "// hermetic-allowed: #363\n",
        "// hermetic-allowed: #363   \n",
        "// hermetic-allowed: 363 reason\n",
        "// hermetic-allowed: # reason\n",
        "// hermetic-allowed: #abc reason\n",
        "// hermetic-allowed #363 reason\n",
        "// hermetic-allowed: #363reason\n",
        "// hermetic-allowed: reason without an issue\n",
    ];
    for marker in forms {
        let text =
            format!("#[test]\nfn f() {{\n    {marker}    let p = std::env::temp_dir();\n}}\n");
        let scan = scan_one(DEMO, &text);
        assert_eq!(scan.violations.len(), 1, "{marker}");
        let failures = evaluate(&scan);
        assert_eq!(failures.len(), 2, "{marker}: {failures:?}");
        assert!(failures[0].contains("malformed marker"), "{marker}");
        assert!(failures[0].contains("crates/demo/src/lib.rs:3"), "{marker}");
    }
}

#[test]
fn a_malformed_marker_outside_test_code_is_still_a_failure() {
    let scan = scan_one(DEMO, "fn f() {\n    // hermetic-allowed: no issue\n}\n");
    assert_eq!(
        scan.malformed,
        [Malformed {
            path: DEMO.to_owned(),
            line: 2
        }]
    );
}

#[test]
fn marker_text_in_docs_strings_and_mid_comment_is_not_a_marker() {
    let text = concat!(
        "/// // hermetic-allowed: broken\n",
        "//! hermetic-allowed: broken\n",
        "fn f() {\n",
        "    let s = \"// hermetic-allowed: broken\";\n",
        "    let r = r#\"\n// hermetic-allowed: broken\n\"#;\n",
        "    // see the hermetic-allowed: broken marker format\n",
        "}\n",
    );
    assert!(scan_one(DEMO, text).malformed.is_empty());
}

#[test]
fn the_timing_marker_does_not_exempt_a_hermetic_violation() {
    let text = in_test("    // timing-allowed: #362 reason\n    let p = std::env::temp_dir();");
    assert_eq!(scan_one(DEMO, &text).violations.len(), 1);
}

#[test]
fn failure_lines_name_the_location_and_rule_and_guidance_names_the_fix() {
    let scan = scan_one(DEMO, &in_test("let p = std::env::temp_dir();"));
    let failures = evaluate(&scan);
    assert_eq!(failures, ["crates/demo/src/lib.rs:3: std::env::temp_dir()"]);
    let advice = guidance(&scan);
    assert!(advice.contains("pohunek_test_support::tempdir()"));
    assert!(advice.contains("hermetic-allowed: #<issue> <reason>"));
    let scan = scan_one(
        DEMO,
        &in_test("let p = TcpListener::bind(\"127.0.0.1:0\").unwrap().local_addr();"),
    );
    assert!(guidance(&scan).contains("keep the bound listener"));
}

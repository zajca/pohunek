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
//!   the end of the literal, `/`, or a syntactic delimiter (a quote, whitespace,
//!   or one of ``: ; , ) & | ` \``); a character such as `+` or `@` extends the
//!   file name, so `/tmp+x` is another path. The scan cannot tell a path that is used from a fake path that only
//!   feeds a validator, and a shell script that mentions `/tmp` writes there
//!   for real, so every such literal needs a rewrite (the migrations use
//!   `/work`) or a marker;
//! - `tempfile::tempdir()`, `tempdir_in(..)`, `tempfile()`, `TempDir::new()`,
//!   `TempDir::with_prefix(..)`, `NamedTempFile::new()` (and `with_prefix`,
//!   `with_suffix`), and every `tempfile::Builder::new()` construction, whatever
//!   the builder is used for afterwards (a builder kept in a variable and
//!   finished later is still a host-temp-capable fixture). A chain that
//!   only ends in `.tempfile_in(dir)` names its directory and is allowed, and so
//!   are `NamedTempFile::new_in(dir)` and the type name `tempfile::TempDir`;
//! - a `TcpListener::bind(..)` of port 0 whose listener is not kept. Port 0
//!   in the bind argument is recognised as a string literal ending in `:0` or
//!   a trailing decimal zero operand, with `_` separators and an optional
//!   integer suffix (`("127.0.0.1", 0)`, `SocketAddr::new(ip, 0_u16)`). The
//!   qualifier is `TcpListener` or any alias of it (`use .. TcpListener as L`,
//!   `type L = ..TcpListener;`).
//!   Whether the listener is dropped cannot be decided from tokens alone, so
//!   the rule is conservative: the bind is accepted only when it initializes a
//!   named struct field, or when it is the whole initializer of a
//!   `let [mut] name = ..` (suffixes `?`, `.await`, `.unwrap()`, `.expect(..)`,
//!   `.map_err(..)` only). A field initializer keeps the listener only when
//!   the rest of the field expression also only unwraps. A `let` name is kept
//!   when it is never `drop(name)`d and the rest of its block uses it for
//!   something other than `name.local_addr()` (passed on, returned, moved into
//!   a value, `.accept()`, ..); a name that starts with `_` and is never used
//!   again is a scope guard and is kept. Anything else, such as
//!   `bind(..).unwrap().local_addr().unwrap().port()`, consumes or reduces the
//!   listener and is reported; hand the bound listener to the code under test
//!   (`from_std`, a socket path from `TestEnv::socket_path`) instead.
//!
//! - `std::env::set_var` and `std::env::remove_var`, also written
//!   `env::set_var`, a bare `set_var`/`remove_var` imported from `std::env`
//!   (by name, aliased with `as`, or through a glob), in test code outside
//!   `crates/test-support`. The process environment is shared by every test
//!   thread of a binary, so a test that must change it does so through
//!   `pohunek_test_support::process_env::ProcessEnv`, whose one binary-wide lock
//!   and restore-on-drop make the change safe against concurrent writers and
//!   readers; product code is not scanned. **No marker exempts this rule**: the
//!   shared mechanism is the one sanctioned place, so a test that wants a
//!   private lock or a bare mutation has to move onto `ProcessEnv` (or take the
//!   value as an argument) instead of carrying an exception. Out of scope: a
//!   mutation through `libc::setenv` or another FFI route, and a name that
//!   `use` re-exports from a user module.
//!
//! Except for that rule, an occurrence on a line that carries, or follows a line that carries, a
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

/// Characters that end a host temp path inside a literal: quotes, whitespace,
/// shell separators and list or escape delimiters. Any other character (letters,
/// digits, `_ . - + @ ..`) extends the file name, so `/tmp` followed by it
/// is another path.
const PATH_END_DELIMITERS: &str = "\"' \t\n\r:;,)&|`\\";

/// Integer type suffixes a port literal may carry (`0_u16`).
const INTEGER_SUFFIXES: [&str; 12] = [
    "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
];

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
    EnvMutation,
}

impl Rule {
    fn label(self) -> &'static str {
        match self {
            Self::TempDir => "std::env::temp_dir()",
            Self::TmpLiteral => "host /tmp path literal",
            Self::Tempfile => "tempfile crate fixture outside pohunek_test_support",
            Self::PortZeroBind => "TcpListener::bind(..:0) whose listener is not kept",
            Self::EnvMutation => "std::env::set_var/remove_var outside pohunek_test_support",
        }
    }

    /// Whether a `hermetic-allowed` marker may exempt an occurrence.
    fn markable(self) -> bool {
        self != Self::EnvMutation
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
            Self::EnvMutation => {
                "pass the value to the code under test, or change it through \
                 `pohunek_test_support::process_env::ProcessEnv` (`lock()`, `set`, \
                 `remove`); no marker exempts this rule"
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

/// `std::env` mutators.
const ENV_MUTATORS: [&str; 2] = ["set_var", "remove_var"];

/// Whether a bare `set_var`/`remove_var` can denote the `std::env` function:
/// imported by name, or through a glob of a path that ends in `env`.
fn env_mutators_imported(skeleton: &str) -> bool {
    if !imported_names(skeleton, "env", &ENV_MUTATORS).is_empty() {
        return true;
    }
    let bytes = skeleton.as_bytes();
    skeleton.match_indices("use ").any(|(at, _)| {
        if !starts_word(bytes, at) {
            return false;
        }
        let end = skeleton[at..].find(';').map_or(skeleton.len(), |n| at + n);
        let statement = &skeleton[at..end];
        contains_word(statement, "env") && statement.contains('*')
    })
}

/// `env::set_var` / `env::remove_var` uses, and an `as` alias of either in an
/// import from `env`.
fn env_mutation_candidates(skeleton: &str) -> Vec<usize> {
    let bytes = skeleton.as_bytes();
    let imported = env_mutators_imported(skeleton);
    let aliases = env_module_aliases(skeleton);
    let mut found = Vec::new();
    for name in ENV_MUTATORS {
        for (offset, qualifier) in named_uses(skeleton, name, false) {
            let hit = match qualifier {
                Qualifier::Path("env") => true,
                Qualifier::Path(module) if aliases.contains(&module) => true,
                Qualifier::Bare => imported && ident_before(bytes, offset) != "fn",
                Qualifier::Path(_) | Qualifier::Method => false,
            };
            if hit {
                found.push(offset);
            }
        }
    }
    // `use std::env::set_var as f;` hides every later call behind the alias.
    for (at, _) in skeleton.match_indices("use ") {
        if !starts_word(bytes, at) {
            continue;
        }
        let end = skeleton[at..].find(';').map_or(skeleton.len(), |n| at + n);
        let statement = &skeleton[at..end];
        if !contains_word(statement, "env") {
            continue;
        }
        for name in ENV_MUTATORS {
            for (offset, _) in statement.match_indices(name) {
                let offset = at + offset;
                let name_end = offset + name.len();
                if starts_word(bytes, offset)
                    && end_of_ident(bytes, offset) == name_end
                    && expect_token(bytes, name_end, "as").is_some()
                {
                    found.push(offset);
                }
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// Names that a `use` rooted at `std` binds to `std::env`: `use std::env as
/// e;`, `use std::{env as e, fs};` and `use std::env::{self as e};`.
///
/// An import rooted elsewhere (`use crate::fixture::env as e;`) names another
/// module and binds nothing here.
fn env_module_aliases(skeleton: &str) -> Vec<&str> {
    let bytes = skeleton.as_bytes();
    let mut aliases = Vec::new();
    for (at, _) in skeleton.match_indices("use ") {
        if !starts_word(bytes, at) {
            continue;
        }
        let end = skeleton[at..].find(';').map_or(skeleton.len(), |n| at + n);
        let squeezed: String = skeleton[at..end]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if !(squeezed.starts_with("usestd::") || squeezed.starts_with("use::std::")) {
            continue;
        }
        for (offset, _) in skeleton[at..end].match_indices("env") {
            let offset = at + offset;
            let name_end = offset + "env".len();
            if !starts_word(bytes, offset) || end_of_ident(bytes, offset) != name_end {
                continue;
            }
            if let Some(alias) = alias_after(skeleton, name_end, end) {
                aliases.push(alias);
                continue;
            }
            // `env::{self as e, ..}` binds `e` to the module itself.
            let Some(group) = expect_token(bytes, name_end, "::")
                .and_then(|after| expect_token(bytes, after, "{"))
            else {
                continue;
            };
            let close = skeleton[group..end].find('}').map_or(end, |n| group + n);
            for (self_at, _) in skeleton[group..close].match_indices("self") {
                let self_at = group + self_at;
                let self_end = self_at + "self".len();
                if starts_word(bytes, self_at) && end_of_ident(bytes, self_at) == self_end {
                    if let Some(alias) = alias_after(skeleton, self_end, close) {
                        aliases.push(alias);
                    }
                }
            }
        }
    }
    aliases
}

/// The identifier in `as <ident>` right after `from`, when it ends before
/// `limit`.
fn alias_after(skeleton: &str, from: usize, limit: usize) -> Option<&str> {
    let bytes = skeleton.as_bytes();
    let after_as = expect_token(bytes, from, "as")?;
    if bytes.get(after_as).copied().is_some_and(is_ident) {
        return None;
    }
    let alias_start = skip_whitespace(bytes, after_as);
    let alias_end = end_of_ident(bytes, alias_start);
    (alias_end > alias_start && alias_end <= limit).then(|| &skeleton[alias_start..alias_end])
}

/// Whether the host temp path at `at` in `text` stands alone.
fn is_host_temp_path(text: &str, at: usize, needle: &str) -> bool {
    let before = text[..at].chars().next_back();
    let after = text[at + needle.len()..].chars().next();
    before.is_none_or(|c| PATH_START_DELIMITERS.contains(c))
        && after.is_none_or(|c| c == '/' || PATH_END_DELIMITERS.contains(c))
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

/// Whether `statement` calls the builder method spelled `method` (leading dot).
fn builder_calls(statement: &str, method: &str) -> bool {
    let bytes = statement.as_bytes();
    statement.match_indices(method).any(|(at, _)| {
        let end = at + method.len();
        !bytes.get(end).copied().is_some_and(is_ident)
            && bytes.get(skip_whitespace(bytes, end)) == Some(&b'(')
    })
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
        let constructed = expect_token(bytes, offset + "Builder".len(), "::").is_some_and(|at| {
            let at = skip_whitespace(bytes, at);
            let end = end_of_ident(bytes, at);
            end > at && bytes.get(skip_whitespace(bytes, end)) == Some(&b'(')
        });
        if !is_tempfile || !constructed {
            continue;
        }
        let statement = &skeleton[offset..end_of_statement(bytes, offset)];
        let names_directory = builder_calls(statement, ".tempfile_in")
            && ![".tempdir", ".tempdir_in", ".tempfile"]
                .iter()
                .any(|method| builder_calls(statement, method));
        if !names_directory {
            found.push(offset);
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// Whether `token` is a decimal zero: digits and `_` separators with at least
/// one digit, all zero, and an optional integer type suffix.
fn is_zero_literal(token: &str) -> bool {
    let digits = INTEGER_SUFFIXES
        .iter()
        .find_map(|suffix| token.strip_suffix(suffix))
        .unwrap_or(token)
        .trim_end_matches('_');
    digits.starts_with('0') && digits.bytes().all(|b| b == b'0' || b == b'_')
}

/// Whether the bind argument (code view) names port 0.
fn binds_port_zero(argument_code: &str, argument_skeleton: &str) -> bool {
    if argument_code.contains(":0\"") {
        return true;
    }
    let trailing = argument_skeleton
        .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | ','));
    let token_start = trailing
        .bytes()
        .rposition(|b| !is_ident(b))
        .map_or(0, |at| at + 1);
    let operand = trailing[..token_start].trim_end();
    operand.ends_with(',') && is_zero_literal(&trailing[token_start..])
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

/// End of the field initializer that contains `from`: the next `,`, `;` or
/// unmatched closer outside nested groups.
fn end_of_field(bytes: &[u8], from: usize) -> usize {
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
            b',' | b';' if depth == 0 => return i,
            _ => {}
        }
    }
    bytes.len()
}

/// End of the block that contains `from`: its unmatched `}`, or the text end.
fn end_of_block(bytes: &[u8], from: usize) -> usize {
    let mut depth = 0_usize;
    for (i, &byte) in bytes.iter().enumerate().skip(from) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    bytes.len()
}

/// One entry per use of `name` in the rest of the block after `from`: `true`
/// when the use is more than `name.local_addr(..)` (passing, moving or returning
/// the listener, any other method call). A `name` reached through a field access
/// (`other.name`) is a different binding and is skipped.
fn later_uses(skeleton: &str, from: usize, name: &str) -> Vec<bool> {
    let bytes = skeleton.as_bytes();
    let block_end = end_of_block(bytes, from);
    named_uses(&skeleton[..block_end], name, false)
        .into_iter()
        .filter(|&(offset, _)| offset >= from)
        .filter(|(_, qualifier)| *qualifier != Qualifier::Method)
        .map(|(offset, _)| {
            let after = skip_whitespace(bytes, offset + name.len());
            let method = (bytes.get(after) == Some(&b'.')).then(|| {
                let start = skip_whitespace(bytes, after + 1);
                (&skeleton[start..end_of_ident(bytes, start)], start)
            });
            !method.is_some_and(|(method, start)| {
                method == "local_addr"
                    && bytes.get(skip_whitespace(bytes, start + method.len())) == Some(&b'(')
            })
        })
        .collect()
}

/// Whether `name` is dropped explicitly somewhere in the block that follows.
fn dropped_later(skeleton: &str, from: usize, name: &str) -> bool {
    let block_end = end_of_block(skeleton.as_bytes(), from);
    let block = &skeleton[from..block_end];
    named_uses(block, "drop", true)
        .into_iter()
        .any(|(offset, _)| {
            let open = skip_whitespace(block.as_bytes(), offset + "drop".len());
            let close = matching_close(block.as_bytes(), open);
            block[open + 1..close].trim() == name
        })
}

/// Names that denote `TcpListener` in `skeleton`: the type itself and every
/// alias introduced by `use .. TcpListener as L` (also inside braces) or
/// `type L = ..TcpListener;`.
fn listener_names(skeleton: &str) -> BTreeSet<String> {
    let bytes = skeleton.as_bytes();
    let mut names = BTreeSet::from(["TcpListener".to_owned()]);
    loop {
        let mut aliases = Vec::new();
        for (at, _) in skeleton.match_indices("use ") {
            if !starts_word(bytes, at) {
                continue;
            }
            let end = skeleton[at..].find(';').map_or(skeleton.len(), |n| at + n);
            for name in &names {
                for (offset, _) in skeleton[at..end].match_indices(name.as_str()) {
                    let offset = at + offset;
                    let name_end = offset + name.len();
                    if !starts_word(bytes, offset) || end_of_ident(bytes, offset) != name_end {
                        continue;
                    }
                    let Some(after_as) = expect_token(bytes, name_end, "as") else {
                        continue;
                    };
                    let alias_start = skip_whitespace(bytes, after_as);
                    if alias_start > after_as {
                        aliases.push(
                            skeleton[alias_start..end_of_ident(bytes, alias_start)].to_owned(),
                        );
                    }
                }
            }
        }
        for (at, _) in skeleton.match_indices("type ") {
            if !starts_word(bytes, at) {
                continue;
            }
            let alias_start = skip_whitespace(bytes, at + "type ".len());
            let alias_end = end_of_ident(bytes, alias_start);
            let end = skeleton[at..].find(';').map_or(skeleton.len(), |n| at + n);
            let Some(equals) = expect_token(bytes, alias_end, "=") else {
                continue;
            };
            if names
                .iter()
                .any(|name| contains_word(&skeleton[equals..end], name))
            {
                aliases.push(skeleton[alias_start..alias_end].to_owned());
            }
        }
        let before = names.len();
        names.extend(aliases.into_iter().filter(|alias| !alias.is_empty()));
        if names.len() == before {
            return names;
        }
    }
}

/// Port-0 `TcpListener::bind` calls whose listener is not demonstrably kept.
fn bind_candidates(file: &SourceFile) -> Vec<usize> {
    let views = file.views();
    let (skeleton, bytes) = (&views.skeleton, views.skeleton.as_bytes());
    let mut found = Vec::new();
    let listeners = listener_names(skeleton);
    for (offset, qualifier) in named_uses(skeleton, "bind", true) {
        let Qualifier::Path(listener) = qualifier else {
            continue;
        };
        if !listeners.contains(listener) {
            continue;
        }
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
            only_unwraps(bytes, close + 1, end_of_field(bytes, close + 1))
        } else {
            let_binding(skeleton, start).is_some_and(|name| {
                only_unwraps(bytes, close + 1, statement_end)
                    && !dropped_later(skeleton, statement_end, name)
                    && {
                        let uses = later_uses(skeleton, statement_end, name);
                        uses.contains(&true) || (name.starts_with('_') && uses.is_empty())
                    }
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
        candidates.extend(
            env_mutation_candidates(skeleton)
                .into_iter()
                .map(|at| (at, Rule::EnvMutation)),
        );
        for (offset, rule) in candidates {
            let line = file.line_of(offset);
            let allowed = rule.markable()
                && (valid.contains(&line) || valid.contains(&line.saturating_sub(1)));
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

/// Whether any violation is of a rule a marker can exempt.
fn rules_with_marker(violations: &[Violation]) -> bool {
    violations.iter().any(|violation| violation.rule.markable())
}

/// How to fix each rule that `scan` broke, with the marker escape hatch.
fn guidance(scan: &Scan) -> String {
    let rules: BTreeSet<Rule> = scan.violations.iter().map(|v| v.rule).collect();
    let mut text = String::new();
    for rule in rules {
        let _ = writeln!(text, "- {}: {}", rule.label(), rule.fix());
    }
    if rules_with_marker(&scan.violations) {
        let _ = write!(
            text,
            "A case where the host state is the subject of the test takes \
             `// {MARKER}: #<issue> <reason>` on or above the line."
        );
    }
    text.trim_end().to_owned()
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
        assert_eq!(found[0].0, 3, "{call}");
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
        "let mut listener = std::net::TcpListener::bind(\"127.0.0.1:0\").expect(\"bind\");\n    serve(&mut listener);",
        "let listener: TcpListener = TcpListener::bind(\"127.0.0.1:0\").await.unwrap();\n    tokio::spawn(run(listener));",
        "let _listener = TcpListener::bind(\"127.0.0.1:0\")?;",
        "let listener = tokio::net::TcpListener::bind((\"127.0.0.1\", 0)).await.map_err(fail).unwrap();\n    let (stream, _) = listener.accept().await.unwrap();",
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

/// Flagged lines of `text` as the content of a test module.
fn flagged_module(text: &str) -> Vec<(usize, Rule)> {
    rules_of(&scan_one(
        DEMO,
        &format!("#[cfg(test)]\nmod tests {{\n{text}\n}}\n"),
    ))
}

#[test]
fn a_split_tempfile_builder_is_flagged_at_its_construction() {
    for text in [
        "let builder = tempfile::Builder::new();\n    let d = builder.tempdir().unwrap();",
        "let mut b = tempfile::Builder::new();\n    b.prefix(\"x\");\n    let d = b.tempdir().unwrap();",
        "let b = tempfile::Builder::default();",
        "let b = ::tempfile::Builder :: new();",
    ] {
        let found = flagged(text);
        assert_eq!(found, [(3, Rule::Tempfile)], "{text}");
    }
    let imported = "use tempfile::Builder;\n#[test]\nfn f() {\n    let b = Builder::new();\n    let d = b.tempdir().unwrap();\n}\n";
    assert_eq!(rules_of(&scan_one(DEMO, imported)), [(4, Rule::Tempfile)]);
    // The import and a type position are not constructions.
    let type_only = "use tempfile::Builder;\n#[test]\nfn f(b: tempfile::Builder<'_, '_>) {}\n";
    assert!(scan_one(DEMO, type_only).violations.is_empty());
    assert!(flagged("let b = other::Builder::new();").is_empty());
}

#[test]
fn a_port_zero_operand_is_recognised_with_separators_and_suffixes() {
    for argument in [
        "(ip, 0)",
        "(ip, 0_u16)",
        "(ip, 0u16)",
        "(ip, 0_000)",
        "(ip, 00)",
        "(ip, 0usize)",
        "(ip,0)",
        "SocketAddr::new(ip, 0_u16)",
        "SocketAddr::from(([127, 0, 0, 1], 0_u16))",
        "(ip, 0,)",
        "\"127.0.0.1:0\"",
    ] {
        let call = format!("let port = TcpListener::bind({argument}).unwrap().local_addr();");
        assert_eq!(flagged(&call), [(3, Rule::PortZeroBind)], "{call}");
    }
    for argument in [
        "(ip, 80)",
        "(ip, 10)",
        "(ip, 0x10)",
        "(ip, PORT0)",
        "(ip, port_0)",
        "(ip, _0x)",
        "(ip, 10_u16)",
        "(0, ip)",
    ] {
        let call = format!("let port = TcpListener::bind({argument}).unwrap().local_addr();");
        assert!(flagged(&call).is_empty(), "{call}: {:?}", flagged(&call));
    }
}

#[test]
fn an_aliased_tcp_listener_is_still_a_listener() {
    let bind = "let port = Listener::bind(\"127.0.0.1:0\").unwrap().local_addr();";
    for header in [
        "use std::net::TcpListener as Listener;",
        "use tokio::net::TcpListener as Listener;",
        "use std::net::{TcpListener as Listener, UdpSocket};",
        "use std::net::{UdpSocket, TcpListener  as  Listener};",
        "type Listener = std::net::TcpListener;",
        "type Listener = tokio::net::TcpListener;",
        "use std::net::TcpListener as Base;\ntype Listener = Base;",
        "use std::net::TcpListener as Base;\nuse self::Base as Listener;",
    ] {
        let text = format!("{header}\n#[test]\nfn f() {{\n    {bind}\n}}\n");
        let lines = header.lines().count() + 3;
        assert_eq!(
            rules_of(&scan_one(DEMO, &text)),
            [(lines, Rule::PortZeroBind)],
            "{header}"
        );
    }
    for header in [
        "use other::Listener;",
        "use std::net::UdpSocket as Listener;",
        "type Listener = std::net::UdpSocket;",
        "use std::net::{TcpListener, UdpSocket as Listener};",
    ] {
        let text = format!("{header}\n#[test]\nfn f() {{\n    {bind}\n}}\n");
        assert!(scan_one(DEMO, &text).violations.is_empty(), "{header}");
    }
}

#[test]
fn a_field_initializer_only_keeps_a_listener_that_is_only_unwrapped() {
    for call in [
        "let s = Fixture { port: TcpListener::bind(\"127.0.0.1:0\").unwrap().local_addr().unwrap().port() };",
        "let s = Fixture {\n        port: TcpListener::bind(\"127.0.0.1:0\").unwrap().local_addr().unwrap().port(),\n        name: 1,\n    };",
        "let s = Fixture {\n        port: TcpListener::bind(\"127.0.0.1:0\").unwrap().into_std(),\n    };",
    ] {
        let found = flagged(call);
        assert_eq!(found.len(), 1, "{call}: {found:?}");
        assert_eq!(found[0].1, Rule::PortZeroBind, "{call}");
    }
    for call in [
        "let s = Fixture { listener: TcpListener::bind(\"127.0.0.1:0\").unwrap() };",
        "let s = Fixture {\n        listener: TcpListener::bind(\"127.0.0.1:0\").await.map_err(fail)?,\n        port: 1,\n    };",
        "let s = Fixture { a: 1, listener: TcpListener::bind(\"127.0.0.1:0\").expect(\"bind\") };",
    ] {
        assert!(flagged(call).is_empty(), "{call}: {:?}", flagged(call));
    }
}

#[test]
fn a_binding_that_only_reports_its_address_is_flagged() {
    let helper = "fn free_port() -> std::io::Result<u16> {\n    let listener = TcpListener::bind(\"127.0.0.1:0\")?;\n    Ok(listener.local_addr()?.port())\n}";
    assert_eq!(flagged_module(helper), [(4, Rule::PortZeroBind)]);
    let nested = "#[test]\nfn f() {\n    let port = {\n        let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n        l.local_addr().unwrap().port()\n    };\n    serve(port);\n}";
    assert_eq!(flagged_module(nested), [(6, Rule::PortZeroBind)]);
    let never_used =
        "#[test]\nfn f() {\n    let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n}";
    assert_eq!(flagged_module(never_used), [(5, Rule::PortZeroBind)]);
    let twice = "#[test]\nfn f() {\n    let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    let a = l.local_addr().unwrap();\n    let b = l . local_addr ().unwrap();\n}";
    assert_eq!(flagged_module(twice), [(5, Rule::PortZeroBind)]);
}

#[test]
fn a_binding_that_escapes_or_is_used_is_kept() {
    for body in [
        "let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    let port = l.local_addr().unwrap().port();\n    tokio::spawn(serve(l));\n    check(port);",
        "let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    let port = l.local_addr().unwrap().port();\n    (l, port)",
        "let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    let port = l.local_addr().unwrap().port();\n    let shared = Arc::new(l);\n    check(port);",
        "let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    let (s, _) = l.accept().unwrap();",
        "let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    for s in l.incoming() {}",
        "let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    let port = l.local_addr().unwrap().port();\n    let s = Server { l, port };",
        "let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    let port = l.local_addr().unwrap().port();\n    thread::spawn(move || run(&l));",
        "let _guard = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    connect_elsewhere();",
        "let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    let port = l.local_addr().unwrap().port();\n    let other = x.l.local_addr();\n    keep(l);",
    ] {
        let text = format!("#[test]\nfn f() {{\n    {body}\n}}");
        assert!(flagged_module(&text).is_empty(), "{body}: {:?}", flagged_module(&text));
    }
    // A field of another value with the binding's name is not a use of the binding.
    let other_field = "#[test]\nfn f() {\n    let l = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    let port = l.local_addr().unwrap().port();\n    serve(self.l, port);\n}";
    assert_eq!(flagged_module(other_field), [(5, Rule::PortZeroBind)]);
}

#[test]
fn tmp_followed_by_a_name_character_is_another_path() {
    for literal in [
        r#"let p = "/tmp+fixture";"#,
        r#"let p = "/tmp@fixture";"#,
        r#"let p = "/tmp~";"#,
        r#"let p = "/tmp%20x";"#,
        r#"let p = "/tmp#x";"#,
        r#"let p = "/var/tmp+x";"#,
    ] {
        assert!(
            flagged(literal).is_empty(),
            "{literal}: {:?}",
            flagged(literal)
        );
    }
    for literal in [
        r#"let p = "/tmp/x";"#,
        r#"let p = "/tmp";"#,
        r#"let p = "/tmp:/usr";"#,
        r#"let p = "/usr:/tmp:/bin";"#,
        r#"let p = "ls /tmp";"#,
        r#"let p = "ls /tmp && true";"#,
        r#"let p = "cd /tmp; ls";"#,
        r#"let p = "(cd /tmp)";"#,
        r#"let p = "'/tmp'";"#,
        r#"let p = "a /tmp\n";"#,
    ] {
        let found = flagged(literal);
        assert_eq!(found.len(), 1, "{literal}: {found:?}");
        assert_eq!(found[0].1, Rule::TmpLiteral, "{literal}");
    }
}

#[test]
fn every_env_mutation_call_form_is_flagged() {
    for call in [
        "std::env::set_var(\"K\", \"v\");",
        "std::env::remove_var(\"K\");",
        "env::set_var(\"K\", \"v\");",
        "env::remove_var(\"K\");",
        "::std::env::set_var(\"K\", \"v\");",
        "std::env :: set_var(\"K\", \"v\");",
        "unsafe { std::env::set_var(\"K\", \"v\") };",
        "let f = std::env::set_var::<&str, &str>;",
    ] {
        let found = flagged(call);
        assert_eq!(found.len(), 1, "{call}: {found:?}");
        assert_eq!(found[0].1, Rule::EnvMutation, "{call}");
    }
}

#[test]
fn an_imported_bare_env_mutator_is_flagged_at_the_call() {
    for (import, call) in [
        ("use std::env::set_var;", "set_var(\"K\", \"v\");"),
        ("use std::env::remove_var;", "remove_var(\"K\");"),
        ("use std::env::{self, set_var};", "set_var(\"K\", \"v\");"),
        ("use std::env::{remove_var, var};", "remove_var(\"K\");"),
        ("use std::env::*;", "set_var(\"K\", \"v\");"),
    ] {
        let text = format!("{import}\n#[test]\nfn f() {{\n    {call}\n}}\n");
        assert_eq!(
            rules_of(&scan_one(DEMO, &text)),
            [(4, Rule::EnvMutation)],
            "{import}"
        );
    }
}

#[test]
fn a_mutator_called_through_an_env_module_alias_is_flagged() {
    for (import, call) in [
        (
            "use std::env as process_env;",
            "process_env::set_var(\"K\", \"v\");",
        ),
        ("use std::env as e;", "e::remove_var(\"K\");"),
        ("use std::{env as e, fs};", "e::set_var(\"K\", \"v\");"),
        ("use std::env::{self as e};", "e::set_var(\"K\", \"v\");"),
        ("use std::env::{self as e, var};", "e::remove_var(\"K\");"),
        ("use ::std::env as e;", "e::set_var(\"K\", \"v\");"),
    ] {
        let text = format!("{import}\n#[test]\nfn f() {{\n    {call}\n}}\n");
        assert_eq!(
            rules_of(&scan_one(DEMO, &text)),
            [(4, Rule::EnvMutation)],
            "{import}"
        );
    }
    let unrelated = "use std::env as e;\n#[test]\nfn f() {\n    other::set_var(\"K\", \"v\");\n    let v = e::var(\"K\");\n}\n";
    assert!(rules_of(&scan_one(DEMO, unrelated)).is_empty());
    let custom =
        "use crate::fixture::env as e;\n#[test]\nfn f() {\n    e::set_var(\"K\", \"v\");\n}\n";
    assert!(rules_of(&scan_one(DEMO, custom)).is_empty());
}

#[test]
fn an_aliased_env_mutator_import_is_flagged_at_the_import() {
    let text = "#[cfg(test)]\nmod tests {\n    use std::env::set_var as put;\n    #[test]\n    fn f() {\n        put(\"K\", \"v\");\n    }\n}\n";
    assert_eq!(rules_of(&scan_one(DEMO, text)), [(3, Rule::EnvMutation)]);
}

#[test]
fn env_mutation_lookalikes_and_the_shared_override_are_not_flagged() {
    for call in [
        "my_set_var(\"K\", \"v\");",
        "set_var_checked(\"K\");",
        "registry.set_var(\"K\", \"v\");",
        "other::set_var(\"K\", \"v\");",
        "set_var(\"K\", \"v\");",
        "fn set_var() {}",
        "let remove_var = 1;",
        "let s = \"std::env::set_var(K, v)\";",
        "// std::env::remove_var(\"K\");",
        "let v = std::env::var(\"K\");",
        "let mut env = ProcessEnv::lock();",
        "let mut env = pohunek_test_support::process_env::ProcessEnv::lock();\n    env.set(\"K\", \"v\").remove(\"L\");",
    ] {
        assert!(flagged(call).is_empty(), "{call}: {:?}", flagged(call));
    }
    let local = "use other::set_var;\n#[test]\nfn f() {\n    set_var(\"K\", \"v\");\n}\n";
    assert!(scan_one(DEMO, local).violations.is_empty());
    let defined = "#[test]\nfn f() {\n    fn set_var(k: &str) {}\n    set_var(\"K\");\n}\n";
    assert!(scan_one(DEMO, defined).violations.is_empty());
}

#[test]
fn env_mutation_is_flagged_in_test_code_only_and_test_support_is_exempt() {
    let body = "fn f() {\n    std::env::set_var(\"K\", \"v\");\n}\n";
    assert!(scan_one(DEMO, body).violations.is_empty());
    assert_eq!(
        rules_of(&scan_one("crates/demo/tests/run.rs", body)),
        [(2, Rule::EnvMutation)]
    );
    let mixed = format!("#[cfg(test)]\nmod tests {{\n{body}}}\n{body}");
    assert_eq!(rules_of(&scan_one(DEMO, &mixed)), [(4, Rule::EnvMutation)]);
    assert!(scan_one("crates/test-support/src/process_env.rs", body)
        .violations
        .is_empty());
}

#[test]
fn no_marker_exempts_an_env_mutation() {
    for form in [
        "    std::env::set_var(\"K\", \"v\"); // hermetic-allowed: #405 reason\n",
        "    // hermetic-allowed: #405 reason\n    std::env::set_var(\"K\", \"v\");\n",
        "    // hermetic-allowed: #1 reason\n    std::env::remove_var(\"K\");\n",
    ] {
        let scan = scan_one(DEMO, &format!("#[test]\nfn f() {{\n{form}}}\n"));
        assert_eq!(
            rules_of(&scan),
            [(
                if form.starts_with("    //") { 4 } else { 3 },
                Rule::EnvMutation
            )],
            "{form}"
        );
        assert!(scan.malformed.is_empty(), "{form}");
    }
}

#[test]
fn env_mutation_guidance_names_the_shared_override_and_no_marker() {
    let scan = scan_one(DEMO, &in_test("std::env::set_var(\"K\", \"v\");"));
    assert_eq!(
        evaluate(&scan),
        [format!(
            "{DEMO}:3: std::env::set_var/remove_var outside pohunek_test_support"
        )]
    );
    let advice = guidance(&scan);
    assert!(advice.contains("ProcessEnv"));
    assert!(advice.contains("no marker exempts this rule"));
    assert!(!advice.contains("A case where the host state"));
    let mixed = scan_one(
        DEMO,
        &in_test("std::env::set_var(\"K\", \"v\");\n    let p = std::env::temp_dir();"),
    );
    assert!(guidance(&mixed).contains("hermetic-allowed: #<issue> <reason>"));
}

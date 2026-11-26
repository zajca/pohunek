//! Spec-derived assertions, the `*.expect.toml` sidecar of a trace.
//!
//! An assertion states a fact a terminal specification fixes, with its source.
//! The expected values are written from the specification, never copied from a
//! model's output.

// Rust guideline compliant 2026-06-26

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::ModelConfig;
use crate::model::GridDump;

/// Where an assertion's expected value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Basis {
    /// A neutral public specification; `source` is its URL.
    Spec,
    /// A contract Pohunek's own code or documentation states; `source` is a
    /// repository path. The report keeps these apart from specification facts.
    PohunekContract,
}

/// What an assertion checks.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "check", rename_all = "snake_case")]
pub enum Check {
    /// The text and width of one cell; a field left out is not checked.
    Cell {
        /// Row, zero based.
        row: u16,
        /// Column, zero based.
        col: u16,
        /// Expected grapheme text.
        text: Option<String>,
        /// Expected width, 1 or 2.
        width: Option<u8>,
    },
    /// Whether a cell is the trailing half of a wide cell.
    SpacerTail {
        /// Row, zero based.
        row: u16,
        /// Column, zero based.
        col: u16,
        /// Expected flag.
        value: bool,
    },
    /// Whether a row soft-wraps into the next one.
    RowWrapped {
        /// Row, zero based.
        row: u16,
        /// Expected flag.
        value: bool,
    },
    /// The screen size in rows and columns.
    Size {
        /// Expected rows.
        rows: u16,
        /// Expected columns.
        cols: u16,
    },
    /// The cursor position.
    Cursor {
        /// Row, zero based.
        row: u16,
        /// Column, zero based.
        col: u16,
    },
    /// Whether the alternate screen is active.
    AlternateScreen {
        /// Expected flag.
        value: bool,
    },
    /// The window title.
    Title {
        /// Expected title.
        value: String,
    },
    /// The payload of the latest OSC 9 sequence.
    Progress {
        /// Expected payload.
        value: String,
    },
    /// The number of retained scrollback rows.
    ScrollbackRows {
        /// Expected count.
        value: usize,
    },
    /// The OSC 8 target of a cell; empty means no link.
    Hyperlink {
        /// Row, zero based.
        row: u16,
        /// Column, zero based.
        col: u16,
        /// Expected target.
        uri: String,
    },
    /// The active Kitty keyboard protocol flags.
    KittyKeyboardFlags {
        /// Expected flag bits.
        value: u32,
    },
    /// An OSC 52 write of this many decoded bytes was observed.
    ClipboardWrite {
        /// Decoded byte length of the write.
        bytes: usize,
    },
}

/// One assertion.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Assertion {
    /// Unique name inside the file.
    pub id: String,
    /// Kind of source.
    pub basis: Basis,
    /// The source: a specification URL, or a repository path.
    pub source: String,
    /// One sentence stating the rule the assertion applies.
    pub rule: String,
    /// Evaluate after this many records instead of at the end.
    pub after_record: Option<usize>,
    /// Evaluate right after the n-th `Resize` record (1 based).
    pub after_resize: Option<usize>,
    /// Evaluate right before the n-th `Resize` record (1 based).
    pub before_resize: Option<usize>,
    /// How the width of an ambiguous sequence is decided. Set together with
    /// `mode_2027`.
    pub width_method: Option<String>,
    /// State of the grapheme-cluster mode 2027 the width assertion assumes.
    pub mode_2027: Option<bool>,
    /// The check.
    #[serde(flatten)]
    pub check: Check,
}

impl Assertion {
    /// Whether the assertion depends on a width method, so the report keeps it
    /// in its own group.
    #[must_use]
    pub fn is_width_method(&self) -> bool {
        self.width_method.is_some()
    }
}

/// The parsed sidecar of one trace.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Expect {
    /// The assertions in file order.
    pub assertion: Vec<Assertion>,
}

/// What is wrong with a sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectErrorKind {
    /// The TOML is malformed or has an unknown or missing field.
    Syntax {
        /// One-based line of the problem, when known.
        line: Option<usize>,
        /// The parser's message.
        message: String,
    },
    /// The file holds no assertion.
    Empty,
    /// Two assertions share an id.
    DuplicateId {
        /// The id.
        id: String,
    },
    /// A source is not allowed for its basis.
    DisallowedSource {
        /// The assertion.
        id: String,
        /// Why.
        reason: &'static str,
    },
    /// `width_method` and `mode_2027` are not set together.
    IncompleteWidthBasis {
        /// The assertion.
        id: String,
    },
    /// More than one moment is set, or a resize number is 0.
    InvalidMoment {
        /// The assertion.
        id: String,
    },
}

/// A sidecar could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path}: {kind:?}", path = path.display())]
pub struct ExpectError {
    /// The sidecar file.
    pub path: PathBuf,
    /// What is wrong.
    pub kind: ExpectErrorKind,
}

impl Expect {
    /// Parses and validates the sidecar `path` with content `text`.
    ///
    /// # Errors
    ///
    /// Returns an [`ExpectError`] naming `path` for malformed TOML, an empty
    /// file, a duplicate id, a source that is not allowed (outside the
    /// configured prefixes, or naming a forbidden word), a width method
    /// without its mode 2027 state, or a contradictory moment.
    pub fn parse(text: &str, path: &Path, config: &ModelConfig) -> Result<Self, ExpectError> {
        let fail = |kind| ExpectError {
            path: path.to_path_buf(),
            kind,
        };
        // `deny_unknown_fields` cannot be combined with the flattened check,
        // so the keys are validated on the raw document.
        let raw: toml::Value = toml::from_str(text).map_err(|error| {
            fail(ExpectErrorKind::Syntax {
                line: error
                    .span()
                    .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1),
                message: error.message().to_owned(),
            })
        })?;
        if let Some((key, line)) = unknown_key(&raw, text) {
            return Err(fail(ExpectErrorKind::Syntax {
                line,
                message: format!("unknown key `{key}`"),
            }));
        }
        let expect: Self = toml::from_str(text).map_err(|error| {
            let line = error
                .span()
                .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
            fail(ExpectErrorKind::Syntax {
                line,
                message: error.message().to_owned(),
            })
        })?;
        if expect.assertion.is_empty() {
            return Err(fail(ExpectErrorKind::Empty));
        }
        let mut seen = BTreeSet::new();
        for assertion in &expect.assertion {
            let id = assertion.id.clone();
            if !seen.insert(id.clone()) {
                return Err(fail(ExpectErrorKind::DuplicateId { id }));
            }
            if let Some(reason) = source_problem(assertion, config) {
                return Err(fail(ExpectErrorKind::DisallowedSource { id, reason }));
            }
            if assertion.width_method.is_some() != assertion.mode_2027.is_some() {
                return Err(fail(ExpectErrorKind::IncompleteWidthBasis { id }));
            }
            let moments = [
                assertion.after_record,
                assertion.after_resize,
                assertion.before_resize,
            ];
            if moments.iter().flatten().count() > 1
                || assertion.after_resize == Some(0)
                || assertion.before_resize == Some(0)
            {
                return Err(fail(ExpectErrorKind::InvalidMoment { id }));
            }
        }
        Ok(expect)
    }
}

/// Every key an assertion table may hold, across all checks.
const ASSERTION_KEYS: &[&str] = &[
    "id",
    "basis",
    "source",
    "rule",
    "after_record",
    "after_resize",
    "before_resize",
    "width_method",
    "mode_2027",
    "check",
    "row",
    "col",
    "text",
    "width",
    "value",
    "rows",
    "cols",
    "uri",
    "bytes",
];

/// The first key of the document that is not part of the sidecar format, and
/// the line it is on.
fn unknown_key(raw: &toml::Value, text: &str) -> Option<(String, Option<usize>)> {
    let line_of = |key: &str| {
        text.lines()
            .position(|line| line.trim_start().starts_with(key))
            .map(|index| index + 1)
    };
    let top = raw.as_table()?;
    if let Some(key) = top.keys().find(|key| key.as_str() != "assertion") {
        return Some((key.clone(), line_of(key)));
    }
    for assertion in top.get("assertion")?.as_array()? {
        let table = assertion.as_table()?;
        if let Some(key) = table
            .keys()
            .find(|key| !ASSERTION_KEYS.contains(&key.as_str()))
        {
            return Some((key.clone(), line_of(key)));
        }
    }
    None
}

fn source_problem(assertion: &Assertion, config: &ModelConfig) -> Option<&'static str> {
    let lower = assertion.source.to_lowercase();
    if config
        .forbidden_source_words
        .iter()
        .any(|word| lower.contains(&word.to_lowercase()))
    {
        return Some("the source names the model under test");
    }
    let (prefixes, reason) = match assertion.basis {
        Basis::Spec => (
            &config.allowed_source_prefixes,
            "the source is not a neutral specification URL",
        ),
        Basis::PohunekContract => (
            &config.contract_source_prefixes,
            "the source is not a repository path",
        ),
    };
    let allowed = prefixes
        .iter()
        .any(|prefix| assertion.source.starts_with(prefix.as_str()));
    (!allowed).then_some(reason)
}

/// Result of one assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The model's dump matches the expected value.
    Pass,
    /// It does not, or the model cannot observe the property.
    Fail(String),
}

impl Check {
    /// Compares the check with a dump.
    #[must_use]
    pub fn evaluate(&self, dump: &GridDump) -> Verdict {
        match self {
            Self::Cell {
                row,
                col,
                text,
                width,
            } => {
                let Some(cell) = cell_at(dump, *row, *col) else {
                    return out_of_range(*row, *col, dump);
                };
                if let Some(text) = text {
                    if &cell.text != text {
                        return mismatch("cell text", text, &cell.text);
                    }
                }
                match width {
                    Some(width) if *width != cell.width => {
                        mismatch("cell width", width, cell.width)
                    }
                    _ => Verdict::Pass,
                }
            }
            Self::SpacerTail { row, col, value } => match cell_at(dump, *row, *col) {
                Some(cell) if cell.spacer_tail == *value => Verdict::Pass,
                Some(cell) => mismatch("spacer tail", value, cell.spacer_tail),
                None => out_of_range(*row, *col, dump),
            },
            Self::RowWrapped { row, value } => match dump.row_wrapped.get(usize::from(*row)) {
                Some(wrapped) if wrapped == value => Verdict::Pass,
                Some(wrapped) => mismatch("row wrapped", value, wrapped),
                None => out_of_range(*row, 0, dump),
            },
            Self::Size { rows, cols } => {
                if (dump.rows, dump.cols) == (*rows, *cols) {
                    Verdict::Pass
                } else {
                    mismatch(
                        "size",
                        format!("{rows}x{cols}"),
                        format!("{}x{}", dump.rows, dump.cols),
                    )
                }
            }
            Self::Cursor { row, col } => {
                if (dump.cursor_row, dump.cursor_col) == (*row, *col) {
                    Verdict::Pass
                } else {
                    mismatch(
                        "cursor",
                        format!("{row},{col}"),
                        format!("{},{}", dump.cursor_row, dump.cursor_col),
                    )
                }
            }
            Self::AlternateScreen { value } => {
                compare("alternate screen", value, &dump.alternate_screen)
            }
            Self::Title { value } => match &dump.title {
                Some(title) if title == value => Verdict::Pass,
                other => mismatch("title", value, format!("{other:?}")),
            },
            Self::Progress { value } => match &dump.progress {
                Some(progress) if progress == value => Verdict::Pass,
                other => mismatch("progress", value, format!("{other:?}")),
            },
            Self::ScrollbackRows { value } => {
                compare("scrollback rows", value, &dump.scrollback_rows)
            }
            Self::Hyperlink { row, col, uri } => {
                let Some(links) = &dump.hyperlinks else {
                    return unobservable("OSC 8 hyperlinks");
                };
                match links
                    .get(usize::from(*row))
                    .and_then(|line| line.get(usize::from(*col)))
                {
                    Some(found) if found == uri => Verdict::Pass,
                    Some(found) => mismatch("hyperlink", uri, found),
                    None => out_of_range(*row, *col, dump),
                }
            }
            Self::KittyKeyboardFlags { value } => match dump.kitty_keyboard_flags {
                Some(flags) => compare("kitty keyboard flags", value, &flags),
                None => unobservable("the Kitty keyboard flags"),
            },
            Self::ClipboardWrite { bytes } => match &dump.clipboard_writes {
                Some(writes) if writes.contains(bytes) => Verdict::Pass,
                Some(writes) => mismatch(
                    "OSC 52 writes",
                    format!("one of {bytes} bytes"),
                    format!("{writes:?}"),
                ),
                None => unobservable("OSC 52 clipboard writes"),
            },
        }
    }
}

fn cell_at(dump: &GridDump, row: u16, col: u16) -> Option<&crate::model::CellDump> {
    dump.cells
        .get(usize::from(row))
        .and_then(|line| line.get(usize::from(col)))
}

fn out_of_range(row: u16, col: u16, dump: &GridDump) -> Verdict {
    Verdict::Fail(format!(
        "cell {row},{col} is outside the {}x{} screen",
        dump.rows, dump.cols
    ))
}

fn unobservable(what: &str) -> Verdict {
    Verdict::Fail(format!("the model cannot observe {what}"))
}

fn compare<T: PartialEq + std::fmt::Debug>(what: &str, expected: &T, found: &T) -> Verdict {
    if expected == found {
        Verdict::Pass
    } else {
        mismatch(what, format!("{expected:?}"), format!("{found:?}"))
    }
}

fn mismatch(what: &str, expected: impl std::fmt::Debug, found: impl std::fmt::Debug) -> Verdict {
    Verdict::Fail(format!("{what}: expected {expected:?}, found {found:?}"))
}

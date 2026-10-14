//! Knowledge log fragments: one file per unreleased change under `log.d/`.
//!
//! A change records its log entry as its own fragment instead of editing the
//! shared `log.md`, so independent changes never edit the same file. Every
//! consumer reads the fragments through this module:
//!
//! - [`assemble`] places the unreleased entries at the top of `log.md`, as
//!   `## Unreleased (...)` sections. The embedded bundle and `docs build`
//!   ship that assembled log, so readers see unreleased changes in place.
//! - [`fold`] writes the entries under a release heading; `scripts/release`
//!   runs it and removes the fragments in the release commit.
//! - [`check_log`] refuses a committed `log.md` that carries an unreleased
//!   section, which belongs in a fragment.
//!
//! A fragment is `log.d/<issue>-<slug>.md`: exactly one heading line
//! `## Unreleased (<YYYY-MM-DD>, <topic>, #<issue>[ #<issue>...])`, then a
//! non-empty body without `#` or `##` headings. The issue of the file name
//! must be one the heading cites. Links in the body are relative to the
//! bundle root, where the assembled `log.md` lives.
//!
//! Entries are ordered newest date first, as in the rest of `log.md`; entries
//! of one date by their file's issue number, then by file name, so the order
//! never depends on the directory listing.
//!
//! The module depends on `std` only, because the crate's build script includes
//! it to assemble the embedded bundle.

// Rust guideline compliant 2026-10-14

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// File name of the knowledge log at the bundle root.
pub const LOG_FILE: &str = "log.md";

/// Directory of the unreleased log fragments at the bundle root.
pub const FRAGMENT_DIR: &str = "log.d";

/// Opening of an unreleased entry's heading line.
const UNRELEASED_OPEN: &str = "## Unreleased (";

/// Start of any second-level heading, which opens a log section.
const SECTION_OPEN: &str = "## ";

/// Start of the log's title line.
const TITLE_OPEN: &str = "# ";

/// Heading level a folded entry takes below its release heading.
const RELEASE_ENTRY_OPEN: &str = "### ";

/// Length of an ISO 8601 calendar date, `YYYY-MM-DD`.
const DATE_LEN: usize = 10;

/// Indentation up to which a Markdown line is still a heading.
const MAX_HEADING_INDENT: usize = 3;

/// Extension of a fragment file.
const FRAGMENT_EXTENSION: &str = ".md";

/// Longest topic slug a migrated fragment's file name carries. Long enough to
/// tell the entries of one issue apart, short enough for a readable listing.
const MAX_SLUG_LEN: usize = 48;

/// One unreleased log entry, read from its fragment file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fragment {
    file_name: String,
    label: String,
    date: String,
    issue: u64,
    body: String,
}

impl Fragment {
    /// The fragment's file name inside [`FRAGMENT_DIR`].
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The heading text inside `## Unreleased (...)`.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    fn unreleased_section(&self) -> String {
        format!("{UNRELEASED_OPEN}{})\n\n{}\n", self.label, self.body)
    }

    fn release_entry(&self) -> String {
        format!("{RELEASE_ENTRY_OPEN}{}\n\n{}\n", self.label, self.body)
    }
}

/// Why a fragment or the log was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogError {
    path: PathBuf,
    reason: String,
}

impl LogError {
    fn new(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            reason: reason.into(),
        }
    }

    /// The fragment or log file the error is about.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What is wrong with it.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`: {}", self.path.display(), self.reason)
    }
}

impl std::error::Error for LogError {}

/// Parses the fragment `file_name` with `content`.
///
/// # Errors
///
/// Returns a [`LogError`] naming the first rule of the module documentation
/// the fragment breaks.
pub fn parse_fragment(file_name: &str, content: &str) -> Result<Fragment, LogError> {
    let content = unix_lines(content);
    let content = content.as_ref();
    let refuse = |reason: &str| LogError::new(file_name, reason);
    let issue = file_issue(file_name).ok_or_else(|| {
        refuse("the file name must be `<issue>-<slug>.md`, the slug in lower-case letters, digits and dashes")
    })?;
    let (heading, rest) = content.split_once('\n').unwrap_or((content, ""));
    let label = heading
        .strip_prefix(UNRELEASED_OPEN)
        .and_then(|label| label.strip_suffix(')'))
        .ok_or_else(|| {
            refuse("the first line must be `## Unreleased (<date>, <topic>, #<issue>)`")
        })?;
    let date = label_date(label).ok_or_else(|| {
        refuse("the heading must open with a `YYYY-MM-DD, ` date and name a topic")
    })?;
    if !label_issues(label).any(|cited| cited == issue) {
        return Err(LogError::new(
            file_name,
            format!("the heading must cite #{issue}, the issue of the file name"),
        ));
    }
    if rest.lines().any(opens_title_or_section) {
        return Err(refuse(
            "the body must not hold a `#` or `##` heading; use `###` or deeper",
        ));
    }
    let body = rest.trim_matches('\n');
    if body.trim().is_empty() {
        return Err(refuse("the body must describe the change"));
    }
    Ok(Fragment {
        file_name: file_name.to_owned(),
        label: label.to_owned(),
        date: date.to_owned(),
        issue,
        body: body.to_owned(),
    })
}

/// `text` with CRLF line ends turned into LF, so a file saved with either
/// parses the same.
fn unix_lines(text: &str) -> Cow<'_, str> {
    if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// `line` without the indentation a Markdown heading may carry.
fn unindented(line: &str) -> &str {
    let indent = line.bytes().take_while(|&byte| byte == b' ').count();
    if indent <= MAX_HEADING_INDENT {
        &line[indent..]
    } else {
        line
    }
}

/// Whether `line` is a first- or second-level ATX heading.
fn opens_title_or_section(line: &str) -> bool {
    let line = unindented(line);
    let hashes = line.bytes().take_while(|&byte| byte == b'#').count();
    (1..=2).contains(&hashes)
        && line[hashes..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
}

/// The issue number of a `<issue>-<slug>.md` file name.
fn file_issue(file_name: &str) -> Option<u64> {
    let stem = file_name.strip_suffix(FRAGMENT_EXTENSION)?;
    let (issue, slug) = stem.split_once('-')?;
    let slug_valid = !slug.is_empty()
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !slug_valid || issue.is_empty() || !issue.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    issue.parse().ok()
}

/// The `YYYY-MM-DD` date that opens `label`, when a topic follows it.
fn label_date(label: &str) -> Option<&str> {
    let date = label.get(..DATE_LEN)?;
    let topic = label.get(DATE_LEN..)?.strip_prefix(", ")?;
    let bytes = date.as_bytes();
    let digits = [0, 1, 2, 3, 5, 6, 8, 9]
        .iter()
        .all(|&index| bytes[index].is_ascii_digit());
    let year = date.get(..4)?.parse::<u32>().ok()?;
    let month = date.get(5..7)?.parse::<u8>().ok()?;
    let day = date.get(8..10)?.parse::<u8>().ok()?;
    let valid = digits
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && (1..=days_in_month(year, month)).contains(&day)
        && !topic.trim().is_empty()
        && !topic.trim_start().starts_with('#');
    valid.then_some(date)
}

/// Days of `month` in `year` of the Gregorian calendar; 0 for no such month.
fn days_in_month(year: u32, month: u8) -> u8 {
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    }
}

/// The `#<number>` issues `label` cites.
fn label_issues(label: &str) -> impl Iterator<Item = u64> + '_ {
    label
        .split(|character: char| !character.is_ascii_digit() && character != '#')
        .filter_map(|token| token.strip_prefix('#'))
        .filter_map(|number| number.parse().ok())
}

/// Reads and parses every fragment in `dir`, in log order.
///
/// An absent directory holds no fragments.
///
/// # Errors
///
/// Returns a [`LogError`] for an unreadable directory or file, an entry that
/// is not a regular `.md` file, or a fragment [`parse_fragment`] refuses.
pub fn read_fragments(dir: &Path) -> Result<Vec<Fragment>, LogError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(LogError::new(dir, error.to_string())),
    };
    let mut fragments = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| LogError::new(dir, error.to_string()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| LogError::new(&path, error.to_string()))?;
        let name = entry.file_name();
        let Some(name) = name
            .to_str()
            .filter(|name| name.ends_with(FRAGMENT_EXTENSION))
        else {
            return Err(LogError::new(
                &path,
                "only `.md` fragment files belong in log.d",
            ));
        };
        if !file_type.is_file() {
            return Err(LogError::new(&path, "a fragment must be a regular file"));
        }
        let content =
            fs::read_to_string(&path).map_err(|error| LogError::new(&path, error.to_string()))?;
        let fragment =
            parse_fragment(name, &content).map_err(|error| LogError::new(&path, error.reason))?;
        fragments.push(fragment);
    }
    fragments.sort_by(log_order);
    Ok(fragments)
}

/// Newest date first, then the file's issue, then the file name.
fn log_order(left: &Fragment, right: &Fragment) -> Ordering {
    right
        .date
        .cmp(&left.date)
        .then(left.issue.cmp(&right.issue))
        .then_with(|| left.file_name.cmp(&right.file_name))
}

/// Refuses a committed `log.md` that carries an unreleased section.
///
/// # Errors
///
/// Returns a [`LogError`] naming the first `## Unreleased` heading.
pub fn check_log(path: &Path, log: &str) -> Result<(), LogError> {
    match log
        .lines()
        .find(|line| unindented(line).starts_with("## Unreleased"))
    {
        Some(heading) => Err(LogError::new(
            path,
            format!("`{heading}` belongs in a fragment under {FRAGMENT_DIR}/"),
        )),
        None => Ok(()),
    }
}

/// `log` with every fragment as an `## Unreleased (...)` section above its
/// first section, in the order given.
///
/// # Errors
///
/// Returns a [`LogError`] when `log` does not open with a `# ` title line.
pub fn assemble(log: &str, fragments: &[Fragment]) -> Result<String, LogError> {
    let sections: Vec<String> = fragments.iter().map(Fragment::unreleased_section).collect();
    insert_sections(log, &sections.join("\n"))
}

/// `log` with every fragment as an entry of a new `## <version>` section above
/// its first section, in the order given. Without fragments the log is
/// returned unchanged.
///
/// A log that already has the `## <version>` section is a fold that wrote the
/// log but did not remove every fragment: when that section already holds
/// each fragment's entry, the log is returned unchanged, so removing the
/// fragments completes the fold instead of filing them twice.
///
/// # Errors
///
/// Returns a [`LogError`] when `log` does not open with a `# ` title line, or
/// when it already has the `## <version>` section without every fragment's
/// entry.
pub fn fold(log: &str, fragments: &[Fragment], version: &str) -> Result<String, LogError> {
    if fragments.is_empty() {
        return Ok(log.to_owned());
    }
    let log = unix_lines(log);
    let heading = format!("{SECTION_OPEN}{version}");
    let entries: Vec<String> = fragments.iter().map(Fragment::release_entry).collect();
    if let Some(section) = release_section(&log, &heading) {
        return match fragments
            .iter()
            .zip(&entries)
            .find(|(_, entry)| !section.contains(entry.as_str()))
        {
            None => Ok(log.into_owned()),
            Some((fragment, _)) => Err(LogError::new(
                LOG_FILE,
                format!(
                    "the log already has `{heading}`, and `{}` is not filed in it",
                    fragment.file_name
                ),
            )),
        };
    }
    insert_sections(&log, &format!("{heading}\n\n{}", entries.join("\n")))
}

/// The text of the section `heading` opens in `log`, up to the next `## `
/// section, with a trailing blank line so its last entry reads like the
/// others; `None` when `log` has no such section.
fn release_section(log: &str, heading: &str) -> Option<String> {
    let mut lines = log.lines().skip_while(|line| *line != heading);
    lines.next()?;
    let mut section = String::new();
    for line in lines.take_while(|line| !line.starts_with(SECTION_OPEN)) {
        section.push_str(line);
        section.push('\n');
    }
    section.push('\n');
    Some(section)
}

/// Inserts `sections` before the first section of `log`, after its title and
/// any introduction.
fn insert_sections(log: &str, sections: &str) -> Result<String, LogError> {
    let log = unix_lines(log);
    let log = log.as_ref();
    if !log.starts_with(TITLE_OPEN) {
        return Err(LogError::new(
            LOG_FILE,
            "the log must open with a `# ` title line",
        ));
    }
    if sections.is_empty() {
        return Ok(log.to_owned());
    }
    let mut offset = 0;
    let split = log
        .split_inclusive('\n')
        .find_map(|line| {
            let start = offset;
            offset += line.len();
            line.starts_with(SECTION_OPEN).then_some(start)
        })
        .unwrap_or(log.len());
    let (head, tail) = log.split_at(split);
    let head = head.trim_end_matches('\n');
    let mut assembled = format!("{head}\n\n{}", sections.trim_end_matches('\n'));
    if tail.is_empty() {
        assembled.push('\n');
    } else {
        assembled.push_str("\n\n");
        assembled.push_str(tail);
    }
    Ok(assembled)
}

/// One `## Unreleased (...)` section taken out of a log by [`split_unreleased`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Extracted {
    /// The fragment file name the section gets, unique within the split.
    pub file_name: String,
    /// The fragment's content.
    pub content: String,
}

/// Takes every `## Unreleased (...)` section out of `log` as a fragment.
///
/// A section is named after the first issue its heading cites and a slug of
/// its topic; `taken` lists names already in use, and a repeated name gets a
/// numeric suffix. Returns the log without those sections and the fragments.
///
/// # Errors
///
/// Returns a [`LogError`] for a section whose heading cites no issue or does
/// not open with a date, since its fragment could not be named, or whose
/// fragment would fail [`parse_fragment`].
pub fn split_unreleased(log: &str, taken: &[String]) -> Result<(String, Vec<Extracted>), LogError> {
    let log = unix_lines(log);
    let mut kept = String::new();
    let mut extracted: Vec<Extracted> = Vec::new();
    let mut current: Option<(String, String)> = None;
    let mut names: Vec<String> = taken.to_vec();
    let mut finish = |current: Option<(String, String)>,
                      extracted: &mut Vec<Extracted>|
     -> Result<(), LogError> {
        let Some((label, body)) = current else {
            return Ok(());
        };
        let issue = label_issues(&label).next().ok_or_else(|| {
            LogError::new(
                LOG_FILE,
                format!("`{UNRELEASED_OPEN}{label})` cites no issue"),
            )
        })?;
        let date = label_date(&label).ok_or_else(|| {
            LogError::new(
                LOG_FILE,
                format!("`{UNRELEASED_OPEN}{label})` has no leading date"),
            )
        })?;
        let topic = label[date.len() + 2..]
            .split(',')
            .next()
            .unwrap_or_default();
        let base = format!("{issue}-{}", slug(topic));
        let mut file_name = format!("{base}{FRAGMENT_EXTENSION}");
        let mut suffix = 2;
        while names.contains(&file_name) {
            file_name = format!("{base}-{suffix}{FRAGMENT_EXTENSION}");
            suffix += 1;
        }
        let content = format!("{UNRELEASED_OPEN}{label})\n\n{}\n", body.trim_matches('\n'));
        parse_fragment(&file_name, &content)?;
        names.push(file_name.clone());
        extracted.push(Extracted { file_name, content });
        Ok(())
    };
    for line in log.split_inclusive('\n') {
        if line.starts_with(SECTION_OPEN) || line.starts_with(TITLE_OPEN) {
            finish(current.take(), &mut extracted)?;
            if let Some(label) = line
                .trim_end_matches('\n')
                .strip_prefix(UNRELEASED_OPEN)
                .and_then(|label| label.strip_suffix(')'))
            {
                current = Some((label.to_owned(), String::new()));
                continue;
            }
        }
        match &mut current {
            Some((_, body)) => body.push_str(line),
            None => kept.push_str(line),
        }
    }
    finish(current.take(), &mut extracted)?;
    Ok((kept, extracted))
}

/// A file-name slug of `topic`: lower-case ASCII letters and digits joined by
/// single dashes.
fn slug(topic: &str) -> String {
    let mut slug = String::new();
    for character in topic.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    if slug.len() > MAX_SLUG_LEN {
        let cut = slug[..MAX_SLUG_LEN].rfind('-').unwrap_or(MAX_SLUG_LEN);
        slug.truncate(cut);
    }
    slug.trim_end_matches('-').to_owned()
}

//! The runtime feature table of the runtime package guide against the
//! committed registry snapshot.
//!
//! The snapshot holds one `<id> <since>` line per feature the daemon reports
//! in `runtime_features`. The guide's table (header `| Feature | Since |`)
//! must list the same ids with the same release, in any order: the table
//! orders by release, the registry by addition.

// Rust guideline compliant 2026-11-21

use std::collections::BTreeMap;
use std::path::Path;

use super::read_page;
use crate::XtaskError;

/// The guide that carries the feature table.
const GUIDE: &str = "docs/knowledge/guides/runtime-packages.md";

/// The committed snapshot of the daemon's feature registry.
const SNAPSHOT: &str = "crates/daemon/src/agent/host/fixtures/features.txt";

/// Start of the table's header row; the table ends at the first line that is
/// not a table row.
const TABLE_HEADER: &str = "| Feature | Since |";

/// The features of the guide's table as id to release, or `None` without a
/// table. A row whose first cell is not one backticked id is reported through
/// `problems`.
fn table_rows(content: &str, problems: &mut Vec<String>) -> Option<BTreeMap<String, String>> {
    let mut lines = content.lines();
    lines.find(|line| line.starts_with(TABLE_HEADER))?;
    let mut rows = BTreeMap::new();
    // The separator row follows the header.
    for line in lines.skip(1).take_while(|line| line.starts_with('|')) {
        let mut cells = line.split('|').skip(1).map(str::trim);
        let id = cells
            .next()
            .and_then(|cell| cell.strip_prefix('`')?.strip_suffix('`'));
        let since = cells.next();
        match (id, since) {
            (Some(id), Some(since)) if !id.is_empty() && !since.is_empty() => {
                if rows.insert(id.to_owned(), since.to_owned()).is_some() {
                    problems.push(format!("{GUIDE} lists `{id}` twice"));
                }
            }
            _ => problems.push(format!(
                "{GUIDE} has a feature row without id and release: {line}"
            )),
        }
    }
    Some(rows)
}

/// The snapshot as id to release; a malformed line is reported through
/// `problems`.
fn snapshot_rows(content: &str, problems: &mut Vec<String>) -> BTreeMap<String, String> {
    let mut rows = BTreeMap::new();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let mut words = line.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some(id), Some(since), None) => {
                rows.insert(id.to_owned(), since.to_owned());
            }
            _ => problems.push(format!(
                "{SNAPSHOT} has a line that is not `<id> <since>`: {line}"
            )),
        }
    }
    rows
}

/// The differences between the guide's table and the snapshot.
fn problems(guide: &str, snapshot: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let snapshot = snapshot_rows(snapshot, &mut problems);
    let Some(table) = table_rows(guide, &mut problems) else {
        problems.push(format!("{GUIDE} has no table starting `{TABLE_HEADER}`"));
        return problems;
    };
    for (id, since) in &snapshot {
        match table.get(id) {
            None => problems.push(format!(
                "{GUIDE} does not document the feature `{id}` of {SNAPSHOT}"
            )),
            Some(documented) if documented != since => problems.push(format!(
                "{GUIDE} states `{id}` since {documented}, but {SNAPSHOT} says {since}"
            )),
            Some(_) => {}
        }
    }
    for id in table.keys().filter(|id| !snapshot.contains_key(*id)) {
        problems.push(format!(
            "{GUIDE} documents the feature `{id}`, which {SNAPSHOT} does not list"
        ));
    }
    problems
}

/// Prints the outcome and returns whether the table matches the snapshot.
pub(super) fn check(repo: &Path) -> Result<bool, XtaskError> {
    let guide = read_page(repo, Path::new(GUIDE))?;
    let snapshot = read_page(repo, Path::new(SNAPSHOT))?;
    let problems = problems(&guide, &snapshot);
    if problems.is_empty() {
        println!("[PASS] feature-table: {GUIDE} lists the features of {SNAPSHOT}");
        return Ok(true);
    }
    println!("[FAIL] feature-table: {GUIDE} and {SNAPSHOT} differ:");
    for problem in &problems {
        println!("        {problem}");
    }
    Ok(false)
}

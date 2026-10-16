//! `cargo xtask affected sparse`: the sparse working tree of a package set.
//!
//! The tree holds only the tracked files whose change would select one of the
//! packages ([`sparse_paths`](super::sparse_paths)). Running those packages'
//! tests from a nextest archive in that tree turns a read of any other
//! repository path into a failure: the path is absent, so the read cannot pass
//! with a skipped test. The output is a `git sparse-checkout set --no-cone
//! --stdin` pattern list, one anchored and escaped pattern per file.

// Rust guideline compliant 2026-10-16

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::{
    command_output, ensure_git_toplevel, parse_nul_list, sparse_paths, workspace, Targets, GIT,
};
use crate::XtaskError;

/// Options for one `cargo xtask affected sparse` invocation.
#[derive(Debug, clap::Args)]
pub(crate) struct Options {
    /// Workspace package whose tests run in the tree; repeat for several.
    #[arg(long = "package", value_name = "NAME", required = true)]
    packages: Vec<String>,
}

/// Prints the sparse-checkout patterns of the packages' tree.
pub(crate) fn run(options: &Options) -> Result<(), XtaskError> {
    let cwd = std::env::current_dir().map_err(|source| XtaskError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    let (root, packages) = workspace(&cwd)?;
    ensure_git_toplevel(&root)?;
    let known: BTreeSet<&str> = packages
        .iter()
        .map(|package| package.name.as_str())
        .collect();
    if let Some(unknown) = options
        .packages
        .iter()
        .find(|name| !known.contains(name.as_str()))
    {
        return Err(XtaskError::Usage(format!(
            "affected sparse: `{unknown}` is not a workspace package"
        )));
    }
    let targets = Targets {
        whole: options
            .packages
            .iter()
            .map(|name| (name.clone(), BTreeSet::new()))
            .collect(),
        narrowed: BTreeMap::new(),
    };
    let listing = command_output(
        &root,
        GIT,
        &["ls-tree", "-r", "-z", "--name-only", "HEAD", "--"],
    )?;
    let tracked: BTreeSet<String> = parse_nul_list(&listing).collect();
    print!(
        "{}",
        patterns(&sparse_paths(&tracked, &packages, &targets))?
    );
    Ok(())
}

/// One `--no-cone` sparse-checkout pattern per path: anchored at the root,
/// with every wildcard character and a trailing space escaped, so each pattern
/// matches exactly its file.
pub(crate) fn patterns(paths: &BTreeSet<String>) -> Result<String, XtaskError> {
    let mut out = String::new();
    for path in paths {
        if path.contains(['\n', '\r']) {
            return Err(XtaskError::Usage(format!(
                "affected sparse: tracked path {path:?} has a line break, which a pattern list cannot hold"
            )));
        }
        out.push('/');
        let trailing = path.len() - path.trim_end_matches(' ').len();
        let (body, spaces) = path.split_at(path.len() - trailing);
        for c in body.chars() {
            if matches!(c, '\\' | '*' | '?' | '[') {
                out.push('\\');
            }
            out.push(c);
        }
        for _ in spaces.chars() {
            out.push_str("\\ ");
        }
        out.push('\n');
    }
    Ok(out)
}

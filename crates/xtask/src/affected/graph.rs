//! The Cargo dependency graph behind a selection: the closure over workspace
//! members and the dependency diff of a manifest or lockfile change.
//!
//! **Closure.** A changed member changes its library, and so does every member
//! that reaches it through normal or build dependencies, transitively. A member
//! whose dev-dependency is such a library has its tests selected too, but a
//! dev-dependency never enters the dependent's library, so it counts for one
//! hop only.
//!
//! **Dependency diff.** A change to `Cargo.lock`, to a member manifest or to the
//! root manifest cannot be mapped by its path: one lockfile line can change the
//! code of every member that resolves the touched crate. The diff resolves both
//! revisions with `cargo metadata --all-features`, which reports the
//! workspace-unified resolve the CI test archive is built with
//! (`cargo nextest archive --workspace --all-features`). A resolved node is
//! changed when it exists on one side only, or when its features or its
//! dependency edges differ. A node-level comparison of that resolve is a
//! superset of what a feature-graph analysis would select, which is the safe
//! direction. A root manifest change outside `[workspace.dependencies]`
//! (profiles, patches, lints, workspace package fields, members) cannot be
//! judged by the resolve and selects every test.

// Rust guideline compliant 2026-10-16

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{command_in, command_output, Package, GIT};
use crate::XtaskError;

/// How a dependency enters its dependent.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Kind {
    /// A normal or build dependency: part of the dependent's library or build.
    Library,
    /// A dev-dependency: part of the dependent's tests, benches and examples only.
    Dev,
}

/// Why a member is selected.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Reason {
    /// A file or a resolved dependency of the member itself changed.
    Changed(String),
    /// The member's library depends on a selected library.
    DependsOn(String),
    /// The member's tests dev-depend on a selected library.
    DevDependsOn(String),
}

impl Reason {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Changed(why) => why.clone(),
            Self::DependsOn(name) => format!("depends on {name}"),
            Self::DevDependsOn(name) => format!("dev-depends on {name}"),
        }
    }
}

/// Closes `seeds` over the reverse dependencies of `members`.
///
/// Returns every selected member with the reasons it is selected; a seed keeps
/// the reasons it was seeded with. Seeds that name no member are ignored, so
/// the caller validates names first.
pub(crate) fn close(
    seeds: &BTreeMap<String, BTreeSet<Reason>>,
    members: &[Package],
) -> BTreeMap<String, BTreeSet<Reason>> {
    let known: BTreeSet<&str> = members.iter().map(|member| member.name.as_str()).collect();
    let mut selected: BTreeMap<String, BTreeSet<Reason>> = seeds
        .iter()
        .filter(|(name, _)| known.contains(name.as_str()))
        .map(|(name, reasons)| (name.clone(), reasons.clone()))
        .collect();

    // Libraries: the seeds plus their reverse normal/build dependencies.
    let mut libraries: BTreeSet<String> = selected.keys().cloned().collect();
    let mut frontier: Vec<String> = libraries.iter().cloned().collect();
    while let Some(changed) = frontier.pop() {
        for member in members {
            if member.deps.contains(&(changed.clone(), Kind::Library)) {
                selected
                    .entry(member.name.clone())
                    .or_default()
                    .insert(Reason::DependsOn(changed.clone()));
                if libraries.insert(member.name.clone()) {
                    frontier.push(member.name.clone());
                }
            }
        }
    }

    // Tests: one dev-dependency hop from any changed library.
    for member in members {
        for (dep, kind) in &member.deps {
            if *kind == Kind::Dev && libraries.contains(dep) {
                selected
                    .entry(member.name.clone())
                    .or_default()
                    .insert(Reason::DevDependsOn(dep.clone()));
            }
        }
    }
    selected
}

#[derive(Deserialize)]
struct NoDepsMetadata {
    workspace_root: PathBuf,
    packages: Vec<NoDepsPackage>,
}

#[derive(Deserialize)]
struct NoDepsPackage {
    name: String,
    manifest_path: PathBuf,
    #[serde(default)]
    dependencies: Vec<NoDepsDependency>,
}

#[derive(Deserialize)]
struct NoDepsDependency {
    kind: Option<String>,
    path: Option<PathBuf>,
}

/// Parses `cargo metadata --no-deps` into the workspace root and its members.
///
/// A dependency is a member edge when its `path` is another member's manifest
/// directory; registry and git dependencies are not members.
pub(crate) fn parse_members(json: &[u8]) -> Result<(PathBuf, Vec<Package>), XtaskError> {
    let metadata: NoDepsMetadata = serde_json::from_slice(json).map_err(XtaskError::Json)?;
    let mut dirs: BTreeMap<PathBuf, String> = BTreeMap::new();
    for package in &metadata.packages {
        let dir = package
            .manifest_path
            .parent()
            .ok_or_else(|| XtaskError::InvalidPath(package.manifest_path.clone()))?;
        dirs.insert(dir.to_path_buf(), package.name.clone());
    }
    let mut members = Vec::with_capacity(metadata.packages.len());
    for package in &metadata.packages {
        let dir = package
            .manifest_path
            .parent()
            .and_then(|dir| dir.strip_prefix(&metadata.workspace_root).ok())
            .ok_or_else(|| XtaskError::InvalidPath(package.manifest_path.clone()))?;
        let deps = package
            .dependencies
            .iter()
            .filter_map(|dep| {
                let name = dirs.get(dep.path.as_ref()?)?;
                // A dev-dependency on the package itself enables features of
                // its own tests and is no edge between members.
                if *name == package.name {
                    return None;
                }
                let kind = if dep.kind.as_deref() == Some("dev") {
                    Kind::Dev
                } else {
                    Kind::Library
                };
                Some((name.clone(), kind))
            })
            .collect();
        members.push(Package {
            name: package.name.clone(),
            dir: crate::relative_path_string(dir)?,
            deps,
        });
    }
    Ok((metadata.workspace_root, members))
}

/// Outcome of the dependency diff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManifestDiff {
    /// The change cannot be judged by the resolve; the reason names why.
    Everything(String),
    /// Members whose own resolved node or library closure changed, with reasons.
    Members(BTreeMap<String, BTreeSet<Reason>>),
}

#[derive(Deserialize)]
struct FullMetadata {
    workspace_root: PathBuf,
    workspace_members: Vec<String>,
    packages: Vec<FullPackage>,
    resolve: Resolve,
}

#[derive(Deserialize)]
struct FullPackage {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    #[serde(default)]
    deps: Vec<NodeDep>,
    #[serde(default)]
    features: Vec<String>,
}

#[derive(Deserialize)]
struct NodeDep {
    name: String,
    pkg: String,
    #[serde(default)]
    dep_kinds: Vec<DepKind>,
}

#[derive(Deserialize)]
struct DepKind {
    kind: Option<String>,
    target: Option<String>,
}

/// A resolved node in revision-independent form.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Fingerprint {
    features: BTreeSet<String>,
    /// `(dependency key, extern name, kind, target)` per edge.
    edges: BTreeSet<(String, String, String, String)>,
}

/// One revision's resolve, keyed by revision-independent package keys.
struct Graph {
    /// Workspace member name per member key.
    members: BTreeMap<String, String>,
    nodes: BTreeMap<String, Fingerprint>,
}

/// Key of a package id that is equal across checkouts at different paths.
///
/// A member is keyed by name. A path package outside the workspace is keyed by
/// its path relative to the workspace root. Registry and git ids carry no
/// checkout path and are kept as they are.
fn package_key(id: &str, root: &Path, members: &BTreeMap<String, String>) -> String {
    if let Some(name) = members.get(id) {
        return format!("member:{name}");
    }
    if let Some(rest) = id.strip_prefix("path+file://") {
        let path = rest.split('#').next().unwrap_or(rest);
        let relative = Path::new(path)
            .strip_prefix(root)
            .map_or_else(|_| path.to_owned(), |p| p.display().to_string());
        let suffix = rest.split_once('#').map_or("", |(_, suffix)| suffix);
        return format!("path:{relative}#{suffix}");
    }
    id.to_owned()
}

/// Parses full `cargo metadata` output into a [`Graph`].
fn parse_graph(json: &[u8]) -> Result<Graph, XtaskError> {
    let metadata: FullMetadata = serde_json::from_slice(json).map_err(XtaskError::Json)?;
    let names: BTreeMap<&str, &str> = metadata
        .packages
        .iter()
        .map(|package| (package.id.as_str(), package.name.as_str()))
        .collect();
    let mut member_ids = BTreeMap::new();
    for id in &metadata.workspace_members {
        let name = names.get(id.as_str()).ok_or_else(|| {
            XtaskError::Usage(format!(
                "affected: cargo metadata lists member `{id}` without a package"
            ))
        })?;
        member_ids.insert(id.clone(), (*name).to_owned());
    }
    let root = &metadata.workspace_root;
    let mut nodes = BTreeMap::new();
    for node in metadata.resolve.nodes {
        let mut edges = BTreeSet::new();
        for dep in &node.deps {
            let key = package_key(&dep.pkg, root, &member_ids);
            for kind in &dep.dep_kinds {
                edges.insert((
                    key.clone(),
                    dep.name.clone(),
                    kind.kind.clone().unwrap_or_else(|| "normal".to_owned()),
                    kind.target.clone().unwrap_or_default(),
                ));
            }
        }
        nodes.insert(
            package_key(&node.id, root, &member_ids),
            Fingerprint {
                features: node.features.into_iter().collect(),
                edges,
            },
        );
    }
    let members = member_ids
        .into_values()
        .map(|name| (format!("member:{name}"), name))
        .collect();
    Ok(Graph { members, nodes })
}

/// Registry prefix dropped from package keys in descriptions.
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index#";

/// A package key as a reader would name it: `name@version` for crates.io.
fn short(key: &str) -> &str {
    key.strip_prefix(CRATES_IO)
        .or_else(|| key.strip_prefix("member:"))
        .unwrap_or(key)
}

/// Nodes that exist on one side only or differ between `base` and `head`,
/// each with a description of the difference.
fn changed_nodes(base: &Graph, head: &Graph) -> BTreeMap<String, String> {
    let keys: BTreeSet<&String> = base.nodes.keys().chain(head.nodes.keys()).collect();
    let mut changed = BTreeMap::new();
    for key in keys {
        let description = match (base.nodes.get(key), head.nodes.get(key)) {
            (Some(before), Some(after)) if before == after => continue,
            (None, _) => format!("{} is new", short(key)),
            (_, None) => format!("{} is gone", short(key)),
            (Some(before), Some(after)) => {
                let deps = |node: &Fingerprint| -> BTreeSet<String> {
                    node.edges
                        .iter()
                        .map(|(dep, ..)| short(dep).to_owned())
                        .collect()
                };
                let (was, now) = (deps(before), deps(after));
                let mut parts: Vec<String> =
                    now.difference(&was).map(|dep| format!("+{dep}")).collect();
                parts.extend(was.difference(&now).map(|dep| format!("-{dep}")));
                parts.extend(
                    after
                        .features
                        .difference(&before.features)
                        .map(|f| format!("+feature {f}")),
                );
                parts.extend(
                    before
                        .features
                        .difference(&after.features)
                        .map(|f| format!("-feature {f}")),
                );
                if parts.is_empty() {
                    parts.push("dependency kinds or targets changed".to_owned());
                }
                format!("{} resolves {}", short(key), parts.join(" "))
            }
        };
        changed.insert(key.clone(), description);
    }
    changed
}

/// Members of `head` whose library or tests resolve a changed node.
///
/// A node's library changes when the node changed or a normal or build
/// dependency's library changed, transitively; a member is selected when its
/// library changed or one of its dev-dependencies' libraries changed.
fn selected_members(
    head: &Graph,
    changed: &BTreeMap<String, String>,
) -> BTreeMap<String, BTreeSet<Reason>> {
    let mut libraries: BTreeMap<String, String> = changed
        .iter()
        .filter(|(key, _)| head.nodes.contains_key(*key))
        .map(|(key, description)| (key.clone(), description.clone()))
        .collect();
    loop {
        let mut grew = false;
        for (key, node) in &head.nodes {
            if libraries.contains_key(key) {
                continue;
            }
            let cause = node
                .edges
                .iter()
                .find(|(dep, _, kind, _)| kind != "dev" && libraries.contains_key(dep))
                .map(|(dep, ..)| libraries[dep].clone());
            if let Some(cause) = cause {
                libraries.insert(key.clone(), cause);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    let mut selected: BTreeMap<String, BTreeSet<Reason>> = BTreeMap::new();
    for (key, name) in &head.members {
        if let Some(cause) = libraries.get(key) {
            selected
                .entry(name.clone())
                .or_default()
                .insert(Reason::Changed(format!("dependency diff: {cause}")));
        }
        let Some(node) = head.nodes.get(key) else {
            continue;
        };
        for (dep, _, kind, _) in &node.edges {
            if kind == "dev" {
                if let Some(cause) = libraries.get(dep) {
                    selected
                        .entry(name.clone())
                        .or_default()
                        .insert(Reason::Changed(format!(
                            "dependency diff of a dev-dependency: {cause}"
                        )));
                }
            }
        }
    }
    selected
}

/// Table of the root manifest that the resolve fully reflects.
const RESOLVED_ROOT_TABLE: (&str, &str) = ("workspace", "dependencies");

/// The root manifest without [`RESOLVED_ROOT_TABLE`], or the parse error text.
fn root_manifest_rest(text: &str) -> Result<toml::Value, String> {
    let mut value: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
    if let Some(workspace) = value
        .get_mut(RESOLVED_ROOT_TABLE.0)
        .and_then(toml::Value::as_table_mut)
    {
        workspace.remove(RESOLVED_ROOT_TABLE.1);
    }
    Ok(value)
}

/// Runs the dependency diff between revision `base` and the checkout at `root`.
///
/// The base tree is exported with `git archive` into a private temporary
/// directory and resolved there, so the repository's worktree list and index
/// are never touched. Any failure is an error; the caller turns it into a
/// full selection.
pub(crate) fn manifest_diff(root: &Path, base: &str) -> Result<ManifestDiff, XtaskError> {
    let base_tree = export_tree(root, base)?;
    let base_root = base_tree.path();

    let read = |dir: &Path| {
        let path = dir.join("Cargo.toml");
        std::fs::read_to_string(&path).map_err(|source| XtaskError::Io { path, source })
    };
    let (before, after) = (read(base_root)?, read(root)?);
    match (root_manifest_rest(&before), root_manifest_rest(&after)) {
        (Ok(before), Ok(after)) if before == after => {}
        (Ok(_), Ok(_)) => {
            return Ok(ManifestDiff::Everything(
                "root manifest changed outside [workspace.dependencies]".to_owned(),
            ));
        }
        (Err(error), _) | (_, Err(error)) => {
            return Ok(ManifestDiff::Everything(format!(
                "root manifest does not parse: {error}"
            )));
        }
    }

    let resolve = |dir: &Path| {
        command_output(
            dir,
            "cargo",
            &[
                "metadata",
                "--format-version",
                "1",
                "--all-features",
                "--locked",
            ],
        )
        .and_then(|json| parse_graph(&json))
    };
    let base_graph = resolve(base_root)?;
    let head_graph = resolve(root)?;
    let changed = changed_nodes(&base_graph, &head_graph);
    Ok(ManifestDiff::Members(selected_members(
        &head_graph,
        &changed,
    )))
}

/// Exports the tree of revision `base` of the repository at `root`.
fn export_tree(root: &Path, base: &str) -> Result<tempfile::TempDir, XtaskError> {
    let dir = tempfile::Builder::new()
        .prefix("affected-base-")
        .tempdir()
        .map_err(|source| XtaskError::Io {
            path: std::env::temp_dir(),
            source,
        })?;
    let revision = format!("{base}^{{tree}}");
    let output = command_in(root, GIT)
        .args(["archive", "--format=tar", "--end-of-options", &revision])
        .output()
        .map_err(|source| XtaskError::Io {
            path: PathBuf::from(GIT),
            source,
        })?;
    if !output.status.success() {
        return Err(XtaskError::Usage(format!(
            "affected: `git archive {revision}` failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    tar::Archive::new(output.stdout.as_slice())
        .unpack(dir.path())
        .map_err(|source| XtaskError::Io {
            path: dir.path().to_path_buf(),
            source,
        })?;
    Ok(dir)
}

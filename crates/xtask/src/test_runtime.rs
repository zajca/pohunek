//! Test-only runtime trees for the one-step upgrade test.
//!
//! `release test-runtime` writes the layout `release assemble` gives a daemon
//! archive: `runtime-catalog-anchor.json` beside the daemon and
//! `runtime/runtime-catalog.json` with `runtime/packages/<id>.tar.zst` for each
//! requested runtime. The catalog is signed with a random key that is
//! discarded after signing, and the tree is written only into the directory
//! the caller names. `release assemble` never accepts it: the release signs
//! against the committed anchor (`scripts/release-workflow/assemble`), and
//! `unpack_daemon` in `release.rs` refuses a daemon archive that carries
//! `runtime/` or an anchor other than that one.

use std::fs;
use std::path::{Path, PathBuf};

use package::{read_archive, Limits};

use crate::catalog::{archive_identity, read_input, unix_now, verify_bytes};
use crate::release::{ANCHOR_FILE, BUNDLE_DIR, CATALOG_FILE, INVENTORY_FILE, PACKAGES_DIR};
use crate::release_policy::bundled_package_name;
use crate::runtime_package;
use crate::throwaway_trust::{self, core_release, write_public, CoreRelease};
use crate::XtaskError;

/// Directory of the checkout that holds one source directory per official
/// runtime package.
const RUNTIME_PACKAGES_DIR: &str = "runtime-packages";

/// Prefix of the staging directory the tree is assembled in before it is
/// renamed into the output directory.
const STAGING_PREFIX: &str = ".test-runtime-";

/// Entries whose presence marks a directory as a release tree or bundle: the
/// runtime tree, the anchor, and the bundle inventory.
const RELEASE_TREE_MARKERS: [&str; 3] = [BUNDLE_DIR, ANCHOR_FILE, INVENTORY_FILE];

/// What `release test-runtime` wrote.
#[derive(Debug)]
pub(crate) struct Summary {
    pub(crate) output: PathBuf,
    pub(crate) runtimes: Vec<String>,
}

/// Writes the test runtime tree of `runtimes` into the existing directory
/// `output`, building each package from `root/runtime-packages/<id>`.
///
/// The catalog declares this build's core release and carries the sequence
/// that release's catalog would carry, so a host that recorded the previous
/// release's catalog accepts it. Nothing is written when a runtime is refused.
pub(crate) fn write(
    root: &Path,
    output: &Path,
    runtimes: &[String],
) -> Result<Summary, XtaskError> {
    let release = core_release(env!("CARGO_PKG_VERSION"))?;
    write_tree(root, output, runtimes, &release)
}

/// [`write`] for the catalog of `release`.
fn write_tree(
    root: &Path,
    output: &Path,
    runtimes: &[String],
    release: &CoreRelease,
) -> Result<Summary, XtaskError> {
    check_runtimes(root, runtimes)?;
    check_output_location(root, output)?;
    let parent = match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if !output.is_dir() {
        return Err(XtaskError::Usage(format!(
            "the test runtime output `{}` is not an existing directory",
            output.display()
        )));
    }
    for taken in [output.join(BUNDLE_DIR), output.join(ANCHOR_FILE)] {
        if fs::symlink_metadata(&taken).is_ok() {
            return Err(XtaskError::Usage(format!(
                "the test runtime output already holds `{}`",
                taken.display()
            )));
        }
    }

    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(parent)
        .map_err(|source| XtaskError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    let bundle = staging.path().join(BUNDLE_DIR);
    let packages = bundle.join(PACKAGES_DIR);
    fs::create_dir_all(&packages).map_err(|source| XtaskError::Io {
        path: packages.clone(),
        source,
    })?;

    let mut entries = Vec::with_capacity(runtimes.len());
    for runtime in runtimes {
        let source = root.join(RUNTIME_PACKAGES_DIR).join(runtime);
        let archive = packages.join(bundled_package_name(runtime));
        runtime_package::build(&source, &archive)?;
        let bytes = read_input(&archive, Limits::DEFAULT.max_compressed_bytes)?;
        let verified = read_archive(&bytes, &Limits::DEFAULT).map_err(XtaskError::Package)?;
        let identity = archive_identity(&verified, &archive)?;
        if identity.runtime_id.as_str() != runtime {
            return Err(XtaskError::Usage(format!(
                "the package of `{}` declares runtime `{}`",
                runtime,
                identity.runtime_id.as_str()
            )));
        }
        let entry = throwaway_trust::package_entry(identity, verified.digest().clone())?;
        entries.push(entry);
    }

    let trust = throwaway_trust::sign(&entries, release)?;
    write_public(&staging.path().join(ANCHOR_FILE), &trust.anchor)?;
    write_public(&bundle.join(CATALOG_FILE), &trust.catalog)?;
    check_written(staging.path(), runtimes)?;

    fs::rename(&bundle, output.join(BUNDLE_DIR)).map_err(|source| XtaskError::Io {
        path: output.join(BUNDLE_DIR),
        source,
    })?;
    if let Err(source) = fs::rename(staging.path().join(ANCHOR_FILE), output.join(ANCHOR_FILE)) {
        // The output holds either the whole tree or none of it.
        let _ = fs::remove_dir_all(output.join(BUNDLE_DIR));
        return Err(XtaskError::Io {
            path: output.join(ANCHOR_FILE),
            source,
        });
    }
    Ok(Summary {
        output: output.to_path_buf(),
        runtimes: runtimes.to_vec(),
    })
}

/// Requires a non-empty list of distinct runtime ids, each with a source
/// directory of the checkout.
fn check_runtimes(root: &Path, runtimes: &[String]) -> Result<(), XtaskError> {
    if runtimes.is_empty() {
        return Err(XtaskError::Usage(
            "release test-runtime needs at least one --runtime".to_owned(),
        ));
    }
    for (index, runtime) in runtimes.iter().enumerate() {
        let well_formed = !runtime.is_empty()
            && runtime
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !well_formed {
            return Err(XtaskError::Usage(format!(
                "`{runtime}` is not a runtime id"
            )));
        }
        if runtimes[..index].contains(runtime) {
            return Err(XtaskError::Usage(format!(
                "runtime `{runtime}` is listed twice"
            )));
        }
        if !root.join(RUNTIME_PACKAGES_DIR).join(runtime).is_dir() {
            return Err(XtaskError::Usage(format!(
                "no package source directory for runtime `{runtime}`"
            )));
        }
    }
    Ok(())
}

/// Refuses an output that lies inside the checkout at `root`, or in a release
/// tree or bundle: the canonical output path, and every directory above it,
/// must not hold a [`RELEASE_TREE_MARKERS`] entry. Canonicalizing resolves
/// symlinks, so a link into the checkout is refused as well.
fn check_output_location(root: &Path, output: &Path) -> Result<(), XtaskError> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| XtaskError::Io { path, source }
    };
    let resolved = fs::canonicalize(output).map_err(io(output))?;
    let checkout = fs::canonicalize(root).map_err(io(root))?;
    if resolved.starts_with(&checkout) {
        return Err(XtaskError::Usage(format!(
            "the test runtime output `{}` lies inside the checkout",
            output.display()
        )));
    }
    for directory in resolved.ancestors() {
        for marker in RELEASE_TREE_MARKERS {
            if fs::symlink_metadata(directory.join(marker)).is_ok() {
                return Err(XtaskError::Usage(format!(
                    "the test runtime output `{}` lies inside a release tree (`{}` holds `{marker}`)",
                    output.display(),
                    directory.display()
                )));
            }
        }
    }
    Ok(())
}

/// Reads the written tree back: the anchor verifies the catalog on disk, and
/// each requested runtime has a package whose bytes match the catalog digest.
fn check_written(staging: &Path, runtimes: &[String]) -> Result<(), XtaskError> {
    let anchor_path = staging.join(ANCHOR_FILE);
    let anchor = read_input(
        &anchor_path,
        u64::try_from(package::MAX_ANCHOR_BYTES).unwrap_or(u64::MAX),
    )?;
    let bundle = staging.join(BUNDLE_DIR);
    let catalog = read_input(
        &bundle.join(CATALOG_FILE),
        u64::try_from(package::MAX_CATALOG_BYTES).unwrap_or(u64::MAX),
    )?;
    verify_bytes(&catalog, &anchor, None, unix_now()?)?;
    for runtime in runtimes {
        let path = bundle
            .join(PACKAGES_DIR)
            .join(bundled_package_name(runtime));
        let bytes = read_input(&path, Limits::DEFAULT.max_compressed_bytes)?;
        read_archive(&bytes, &Limits::DEFAULT).map_err(XtaskError::Package)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use package::CatalogError;

    use super::*;
    use crate::release::{parse_version, sequence_of};

    const RUNTIMES: [&str; 2] = ["claude", "codex"];

    fn runtimes() -> Vec<String> {
        RUNTIMES.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn the_tree_lists_each_requested_runtime_and_verifies_against_its_anchor() {
        let root = pohunek_test_support::workspace_root();
        let out = pohunek_test_support::tempdir().expect("tempdir");
        let output = out.path().join("head");
        fs::create_dir(&output).expect("create output");

        let summary = write(&root, &output, &runtimes()).expect("tree written");
        assert_eq!(summary.runtimes, runtimes());

        let anchor = fs::read(output.join(ANCHOR_FILE)).expect("anchor");
        let bundle = output.join(BUNDLE_DIR);
        let catalog = fs::read(bundle.join(CATALOG_FILE)).expect("catalog");
        verify_bytes(&catalog, &anchor, None, unix_now().expect("now")).expect("catalog verifies");

        let mut packages: Vec<String> = fs::read_dir(bundle.join(PACKAGES_DIR))
            .expect("packages dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        packages.sort();
        assert_eq!(packages, ["claude.tar.zst", "codex.tar.zst"]);

        let document: serde_json::Value = serde_json::from_slice(&catalog).expect("json");
        let mut listed: Vec<String> = document["catalog"]["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|entry| entry["runtime_id"].as_str().expect("runtime_id").to_owned())
            .collect();
        listed.sort();
        assert_eq!(listed, ["claude", "codex"]);
    }

    #[test]
    fn a_runtime_without_a_package_source_is_refused_and_writes_nothing() {
        let root = pohunek_test_support::workspace_root();
        let out = pohunek_test_support::tempdir().expect("tempdir");
        let output = out.path().join("head");
        fs::create_dir(&output).expect("create output");

        let missing = vec!["claude".to_owned(), "no-such-runtime".to_owned()];
        assert!(matches!(
            write(&root, &output, &missing),
            Err(XtaskError::Usage(message)) if message.contains("no-such-runtime")
        ));
        assert_eq!(fs::read_dir(&output).expect("read output").count(), 0);
    }

    #[test]
    fn an_output_inside_the_checkout_or_a_release_tree_is_refused_and_writes_nothing() {
        let root = pohunek_test_support::workspace_root();
        let inside_checkout = root.join("crates");
        assert!(matches!(
            write(&root, &inside_checkout, &runtimes()),
            Err(XtaskError::Usage(message)) if message.contains("inside the checkout")
        ));

        let out = pohunek_test_support::tempdir().expect("tempdir");
        let bundle = out.path().join("bundle");
        fs::create_dir_all(bundle.join(BUNDLE_DIR)).expect("release tree");
        let output = bundle.join("head");
        fs::create_dir(&output).expect("create output");
        assert!(matches!(
            write(&root, &output, &runtimes()),
            Err(XtaskError::Usage(message)) if message.contains("release tree")
        ));
        assert_eq!(fs::read_dir(&output).expect("read output").count(), 0);
    }

    #[test]
    fn the_catalog_clears_a_previous_release_high_water_mark_and_sequence_one_does_not() {
        let root = pohunek_test_support::workspace_root();
        let out = pohunek_test_support::tempdir().expect("tempdir");
        let previous = sequence_of(parse_version("0.34.2").expect("version"));
        let head = core_release(env!("CARGO_PKG_VERSION")).expect("head release");
        assert!(
            head.sequence >= previous,
            "the head version is at least the previous one"
        );

        let accepted = out.path().join("head");
        fs::create_dir(&accepted).expect("create output");
        write(&root, &accepted, &runtimes()).expect("tree written");
        let anchor = fs::read(accepted.join(ANCHOR_FILE)).expect("anchor");
        let catalog = fs::read(accepted.join(BUNDLE_DIR).join(CATALOG_FILE)).expect("catalog");
        let verified = verify_bytes(&catalog, &anchor, Some(previous), unix_now().expect("now"))
            .expect("the previous release's high-water mark accepts the head catalog");
        assert_eq!(verified.sequence(), head.sequence);

        let refused = out.path().join("sequence-one");
        fs::create_dir(&refused).expect("create output");
        let sequence_one = CoreRelease {
            version: head.version,
            sequence: 1,
        };
        write_tree(&root, &refused, &runtimes(), &sequence_one).expect("tree written");
        let anchor = fs::read(refused.join(ANCHOR_FILE)).expect("anchor");
        let catalog = fs::read(refused.join(BUNDLE_DIR).join(CATALOG_FILE)).expect("catalog");
        assert!(matches!(
            verify_bytes(&catalog, &anchor, Some(previous), unix_now().expect("now")),
            Err(XtaskError::Catalog(CatalogError::StaleSequence {
                sequence: 1,
                ..
            }))
        ));
    }

    #[test]
    fn a_stamped_head_writes_the_core_version_and_verifies() {
        let root = pohunek_test_support::workspace_root();
        let out = pohunek_test_support::tempdir().expect("tempdir");
        let output = out.path().join("head");
        fs::create_dir(&output).expect("create output");

        let stamped = core_release("0.34.2+upgrade.0123abcdef01").expect("stamped release");
        write_tree(&root, &output, &runtimes(), &stamped).expect("stamped tree written");

        let anchor = fs::read(output.join(ANCHOR_FILE)).expect("anchor");
        let catalog = fs::read(output.join(BUNDLE_DIR).join(CATALOG_FILE)).expect("catalog");
        let verified = verify_bytes(&catalog, &anchor, None, unix_now().expect("now"))
            .expect("the catalog verifies");
        assert_eq!(
            verified.sequence(),
            sequence_of(parse_version("0.34.2").expect("version"))
        );
        let document: serde_json::Value = serde_json::from_slice(&catalog).expect("json");
        assert_eq!(document["catalog"]["release"]["version"], "0.34.2");
    }
}

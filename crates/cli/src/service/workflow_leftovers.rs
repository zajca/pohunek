//! Detection of the units the removed workflow plugin feature installed.
//!
//! An earlier release installed one systemd unit or launchd agent per
//! workflow job and service, named after the installation namespace. Nothing
//! in this release runs or removes them, and the commands they invoke no
//! longer exist, so each would fail on every start. Install-time operations
//! name them and stop instead of leaving them behind.

// Rust guideline compliant 2026-11-09

use std::path::Path;

use super::context::Context;
use super::error::Error;

/// Refuses while workflow plugin units of this installation exist.
///
/// # Errors
///
/// Returns [`Error::WorkflowUnitsInstalled`] listing the units, or a
/// filesystem error when the unit directory cannot be read.
pub(super) fn ensure_none(context: &Context) -> Result<(), Error> {
    let namespace = context.namespace_without_creating()?;
    let units = leftover_units(context.supervisor_dir(), namespace.as_str())?;
    if units.is_empty() {
        Ok(())
    } else {
        Err(Error::WorkflowUnitsInstalled {
            directory: context.supervisor_dir().to_path_buf(),
            units,
        })
    }
}

/// File names in `directory` that belong to a workflow unit of `namespace`.
fn leftover_units(directory: &Path, namespace: &str) -> Result<Vec<String>, Error> {
    let systemd = format!("pohunek-{namespace}-wf-");
    let launchd = format!("io.github.zajca.pohunek.{namespace}.wf.");
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        // A directory that does not exist, or whose parent is not a directory,
        // holds no units; the environment checks name such a path.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(Vec::new())
        }
        Err(source) => {
            return Err(Error::Io {
                operation: "read the unit directory",
                path: directory.to_path_buf(),
                source,
            })
        }
    };
    let mut units = Vec::new();
    for entry in entries {
        let name = entry
            .map_err(|source| Error::Io {
                operation: "read the unit directory",
                path: directory.to_path_buf(),
                source,
            })?
            .file_name();
        let Some(name) = name.to_str() else { continue };
        let extension = Path::new(name)
            .extension()
            .and_then(|extension| extension.to_str());
        let systemd_unit = name.starts_with(&systemd)
            && matches!(extension, Some(e) if e.eq_ignore_ascii_case("service") || e.eq_ignore_ascii_case("timer"));
        let launchd_agent = name.starts_with(&launchd)
            && matches!(extension, Some(e) if e.eq_ignore_ascii_case("plist"));
        if systemd_unit || launchd_agent {
            units.push(name.to_owned());
        }
    }
    units.sort();
    Ok(units)
}

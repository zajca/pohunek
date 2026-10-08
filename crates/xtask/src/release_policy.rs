//! The release policy file and the exact input set it implies.
//!
//! `packaging/release-policy.json` is the single source of the inventory a
//! release must contain: which archives exist for which component and target,
//! which SDK tarballs ship, and how long a catalog stays valid. The assembler
//! derives the names of every input from it, the official runtime packages
//! and the compatibility matrix, then requires the input directory to hold
//! exactly those files.

// Rust guideline compliant 2026-10-08

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::attestation::Row;
use crate::catalog::{io_error, read_input};
use crate::release::{refuse, Fault, InputClass};
use crate::XtaskError;

/// Schema version of the policy file.
const POLICY_SCHEMA: u32 = 1;

/// Largest accepted policy file: a few dozen short lines.
const MAX_POLICY_BYTES: u64 = 64 * 1024;

/// Extension of a checksum file next to an archive or tarball.
const CHECKSUM_SUFFIX: &str = ".sha256";

/// Extension of a release archive.
const ARCHIVE_SUFFIX: &str = ".tar.gz";

/// Extension of an SDK tarball.
const SDK_SUFFIX: &str = ".tgz";

/// Extension of a runtime package archive.
const PACKAGE_SUFFIX: &str = ".tar.zst";

/// Extension of an attestation document.
const ATTESTATION_SUFFIX: &str = ".json";

/// A component that ships as a release archive.
///
/// The archive name prefix equals the one `packaging/stage-archive` uses.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Component {
    Cli,
    Daemon,
    Relay,
}

impl Component {
    fn prefix(self) -> &'static str {
        match self {
            Self::Cli => "pohunek-cli",
            Self::Daemon => "pohunek-daemon",
            Self::Relay => "pohunek-relay",
        }
    }
}

/// One release archive the release must contain.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArchiveSlot {
    pub(crate) component: Component,
    pub(crate) target: String,
}

impl ArchiveSlot {
    /// The archive name without extension: `pohunek-<component>-<version>-<target>`.
    pub(crate) fn stem(&self, version: &str) -> String {
        format!("{}-{version}-{}", self.component.prefix(), self.target)
    }

    /// The archive file name.
    pub(crate) fn archive_name(&self, version: &str) -> String {
        format!("{}{ARCHIVE_SUFFIX}", self.stem(version))
    }
}

/// The checked-in release policy.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    schema: u32,
    /// Days from the commit time until the catalog expires.
    pub(crate) catalog_validity_days: u64,
    /// Every archive a release contains.
    pub(crate) archives: Vec<ArchiveSlot>,
    /// SDK package names; each ships as `pohunek-ts-<name>-<version>.tgz`.
    pub(crate) sdk_packages: Vec<String>,
}

fn is_word(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
}

/// Reads and validates the policy file at `path`.
pub(crate) fn load_policy(path: &Path) -> Result<Policy, XtaskError> {
    let bytes = read_input(path, MAX_POLICY_BYTES)?;
    let policy: Policy = serde_json::from_slice(&bytes)
        .map_err(|_cause| refuse(InputClass::Policy, Fault::Malformed, "policy"))?;
    let malformed = |what: &str| refuse(InputClass::Policy, Fault::Malformed, what);
    if policy.schema != POLICY_SCHEMA {
        return Err(malformed("schema"));
    }
    if policy.catalog_validity_days == 0 {
        return Err(malformed("catalog_validity_days"));
    }
    if policy.archives.iter().any(|slot| !is_word(&slot.target)) {
        return Err(malformed("archives"));
    }
    let distinct: BTreeSet<&ArchiveSlot> = policy.archives.iter().collect();
    if distinct.len() != policy.archives.len() {
        return Err(refuse(InputClass::Policy, Fault::Duplicate, "archives"));
    }
    if !policy
        .archives
        .iter()
        .any(|slot| slot.component == Component::Daemon)
    {
        return Err(refuse(
            InputClass::Policy,
            Fault::Missing,
            "daemon archives",
        ));
    }
    if policy.sdk_packages.is_empty() || policy.sdk_packages.iter().any(|name| !is_word(name)) {
        return Err(malformed("sdk_packages"));
    }
    let distinct: BTreeSet<&String> = policy.sdk_packages.iter().collect();
    if distinct.len() != policy.sdk_packages.len() {
        return Err(refuse(InputClass::Policy, Fault::Duplicate, "sdk_packages"));
    }
    Ok(policy)
}

impl Policy {
    /// Targets that have a daemon archive, ascending.
    pub(crate) fn daemon_targets(&self) -> Vec<&str> {
        let mut targets: Vec<&str> = self
            .archives
            .iter()
            .filter(|slot| slot.component == Component::Daemon)
            .map(|slot| slot.target.as_str())
            .collect();
        targets.sort_unstable();
        targets
    }
}

/// File name of the SDK tarball of package `name`.
pub(crate) fn sdk_name(name: &str, version: &str) -> String {
    format!("pohunek-ts-{name}-{version}{SDK_SUFFIX}")
}

/// File name of the package archive of `runtime` in release `version`.
pub(crate) fn package_name(runtime: &str, version: &str) -> String {
    format!("pohunek-runtime-{runtime}-{version}{PACKAGE_SUFFIX}")
}

/// File name of the attestation of `runtime` on `target`.
pub(crate) fn attestation_name(runtime: &str, target: &str) -> String {
    format!("attestation-{runtime}-{target}{ATTESTATION_SUFFIX}")
}

/// File name of a package archive inside a daemon archive's `runtime/packages`.
pub(crate) fn bundled_package_name(runtime: &str) -> String {
    format!("{runtime}{PACKAGE_SUFFIX}")
}

/// File name of an attestation inside a daemon archive's `runtime/attestations`.
pub(crate) fn bundled_attestation_name(runtime: &str, target: &str) -> String {
    format!("{runtime}-{target}{ATTESTATION_SUFFIX}")
}

/// File name of the checksum file of `artifact`.
pub(crate) fn checksum_name(artifact: &str) -> String {
    format!("{artifact}{CHECKSUM_SUFFIX}")
}

/// The files the input directory must hold, by name and class, and the
/// name patterns whose other members are duplicates of an expected slot.
#[derive(Debug)]
pub(crate) struct Expected {
    names: BTreeMap<String, InputClass>,
    families: Vec<(String, String, InputClass)>,
}

impl Expected {
    /// The expected inputs of release `version`: the archives and SDK
    /// tarballs of the policy with their checksum files, one package archive
    /// per official runtime and one attestation per matrix row.
    pub(crate) fn new(policy: &Policy, version: &str, runtimes: &[String], rows: &[Row]) -> Self {
        let mut names = BTreeMap::new();
        let mut families = Vec::new();
        for slot in &policy.archives {
            let archive = slot.archive_name(version);
            names.insert(checksum_name(&archive), InputClass::Checksum);
            names.insert(archive, InputClass::Archive);
            let prefix = format!("{}-", slot.component.prefix());
            families.push((
                prefix,
                format!("-{}{ARCHIVE_SUFFIX}", slot.target),
                InputClass::Archive,
            ));
        }
        for package in &policy.sdk_packages {
            let tarball = sdk_name(package, version);
            names.insert(checksum_name(&tarball), InputClass::Checksum);
            names.insert(tarball, InputClass::SdkPackage);
            families.push((
                format!("pohunek-ts-{package}-"),
                SDK_SUFFIX.to_owned(),
                InputClass::SdkPackage,
            ));
        }
        for runtime in runtimes {
            names.insert(package_name(runtime, version), InputClass::PackageArchive);
            families.push((
                format!("pohunek-runtime-{runtime}-"),
                PACKAGE_SUFFIX.to_owned(),
                InputClass::PackageArchive,
            ));
        }
        for row in rows {
            names.insert(
                attestation_name(&row.runtime, &row.target),
                InputClass::Attestation,
            );
        }
        Self { names, families }
    }

    /// Requires the flat directory `inputs` to hold exactly the expected
    /// regular files.
    ///
    /// Refuses the first missing file, then the first file that repeats an
    /// expected slot under another name, then the first unexpected file, and
    /// anything that is not a regular file. Only names appear in the error.
    pub(crate) fn check_directory(&self, inputs: &Path) -> Result<(), XtaskError> {
        let mut present = BTreeSet::new();
        for entry in fs::read_dir(inputs).map_err(io_error(inputs))? {
            let entry = entry.map_err(io_error(inputs))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry.file_type().map_err(io_error(&entry.path()))?;
            if !kind.is_file() {
                return Err(refuse(InputClass::Unexpected, Fault::NotRegularFile, name));
            }
            present.insert(name);
        }
        for (name, class) in &self.names {
            if !present.contains(name) {
                return Err(refuse(*class, Fault::Missing, name));
            }
        }
        for name in &present {
            if self.names.contains_key(name) {
                continue;
            }
            let family = self.families.iter().find(|(prefix, suffix, _class)| {
                name.starts_with(prefix) && name.ends_with(suffix)
            });
            return Err(match family {
                Some((_prefix, _suffix, class)) => refuse(*class, Fault::Duplicate, name),
                None => refuse(InputClass::Unexpected, Fault::Unexpected, name),
            });
        }
        Ok(())
    }
}

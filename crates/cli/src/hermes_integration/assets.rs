//! Operator asset source and its explicit ownership contract.
//!
//! The managed plugin comes from exactly one immutable source: the selected,
//! enabled official Hermes runtime package. The CLI carries only the core hook
//! reporter; without an installed Hermes package no operator plugin can be
//! installed or updated.

// Rust guideline compliant 2026-10-13

#![expect(
    clippy::map_err_ignore,
    reason = "asset errors intentionally redact parser and path details from operator output"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use package::registry::{PackageSource, Registry, RegistryError};
use package::Limits;
use protocol::PackageId;
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};

use super::error::Error;

/// Version of the marker format written into one managed plugin root.
pub(crate) const MARKER_VERSION: u32 = 3;
/// The generated marker is deliberately not self-checksummed.
pub(crate) const MARKER_NAME: &str = ".pohunek-owned.json";
/// The hook reporter is compiled into the CLI and owned exclusively by it.
const CORE_OWNED_ASSETS: [(&str, &[u8]); 1] = [(
    "_pohunek_reporter.py",
    include_bytes!("assets/pohunek/_pohunek_reporter.py"),
)];
const POLICY_TOKEN: &str = "__POHUNEK_POLICY_PATH__";
/// Runtime package that supplies the operator sidecar and payload.
///
/// The marker records it for package-backed installs; parsing accepts no other id.
pub(crate) const PACKAGE_ID: &str = "pohunek.runtime.hermes";
/// Marker `source` of an install rendered from an operator payload that a CLI
/// binary carried. This CLI reads such markers and never writes them.
pub(crate) const EMBEDDED_SOURCE: &str = "embedded";
/// Marker `source` of an install rendered from the selected runtime package.
pub(crate) const PACKAGE_SOURCE: &str = "package";
/// Immutable core assets excluded from any future runtime-package overlay.
const CORE_OWNED_ASSET_PATHS: [&str; 1] = ["_pohunek_reporter.py"];
const EXPECTED_PAYLOAD_PATHS: [&str; 8] = [
    "plugin.yaml",
    "__init__.py",
    "cli.py",
    "hooks.py",
    "policy.py",
    "redact.py",
    "tools.py",
    "skills/pohunek/SKILL.md",
];
const EXPECTED_PATHS: [&str; 9] = [
    "plugin.yaml",
    "__init__.py",
    "cli.py",
    "hooks.py",
    "policy.py",
    "redact.py",
    "tools.py",
    "skills/pohunek/SKILL.md",
    "_pohunek_reporter.py",
];
/// Exact inventory written before the core reporter became a managed asset.
/// This inventory is accepted only for reading; every install writes the current set.
const PREVIOUS_PATHS: [&str; 8] = [
    "plugin.yaml",
    "__init__.py",
    "cli.py",
    "hooks.py",
    "policy.py",
    "redact.py",
    "tools.py",
    "skills/pohunek/SKILL.md",
];
#[cfg(test)]
pub(crate) const ASSET_COUNT: usize = EXPECTED_PATHS.len();

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperatorManifest {
    schema: u32,
    payload: Vec<OperatorAsset>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperatorAsset {
    path: String,
    sha256: String,
}

/// One rendered asset ready to write below the managed plugin root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asset {
    path: String,
    bytes: Vec<u8>,
}

/// Immutable bytes and package provenance selected before the filesystem transaction.
#[derive(Debug, Clone)]
pub(crate) struct Bundle {
    pub(crate) assets: Vec<Asset>,
    /// Digest of the package's closed operator sidecar.
    pub(crate) manifest_sha256: String,
    /// Runtime package id that supplied the operator payload.
    pub(crate) package_id: String,
    /// Package archive digest as lowercase hex, without the `sha256:` prefix.
    pub(crate) package_digest: String,
}

/// Selects and snapshots the enabled, selected official Hermes package.
///
/// # Errors
///
/// Returns [`Error::PackageNotInstalled`] when no Hermes package is installed,
/// [`Error::PackageRegistryUnreadable`] when the registry cannot be opened or
/// read safely, [`Error::PackageNotSelected`] when a Hermes package is
/// installed but no enabled official version is selected, and
/// [`Error::InvalidAsset`] when the selected package fails verification or
/// the closed sidecar contract.
pub(crate) fn selected_bundle(registry_path: &Path, policy_path: &Path) -> Result<Bundle, Error> {
    let Some(registry) = Registry::open_existing_at(registry_path, Limits::DEFAULT)
        .map_err(|_| Error::PackageRegistryUnreadable)?
    else {
        return Err(Error::PackageNotInstalled);
    };
    let state = registry
        .state()
        .map_err(|_| Error::PackageRegistryUnreadable)?;
    let id = PackageId::parse(PACKAGE_ID).expect("the Hermes package id constant is valid");
    if !state
        .packages()
        .iter()
        .any(|record| record.identity().id == id)
    {
        return Err(Error::PackageNotInstalled);
    }
    let selected = state.selected(&id).ok_or(Error::PackageNotSelected)?;
    // A validated registry state only selects installed records of the same id.
    let record = state
        .package(selected)
        .filter(|record| record.identity().id == id)
        .ok_or(Error::PackageRegistryUnreadable)?;
    if !record.enabled() || record.source() != PackageSource::Official {
        return Err(Error::PackageNotSelected);
    }
    let verified = registry.verify(selected).map_err(|error| match error {
        RegistryError::RootInvalid(_) => Error::InvalidAsset,
        _ => Error::PackageRegistryUnreadable,
    })?;
    let source = verified
        .read_file("operator.toml")
        .map_err(|_| Error::InvalidAsset)?;
    let mut payload = BTreeMap::new();
    let mut operator_paths = BTreeSet::new();
    for file in verified.files() {
        if file.path.starts_with("operator/") {
            operator_paths.insert(file.path.clone());
            let bytes = verified
                .read_file(&file.path)
                .map_err(|_| Error::InvalidAsset)?;
            payload.insert(file.path.clone(), bytes);
        }
    }
    let assets = render_source(policy_path, &source, |path| {
        payload.get(path).cloned().ok_or(Error::InvalidAsset)
    })?;
    let expected_operator_paths: BTreeSet<_> = EXPECTED_PAYLOAD_PATHS
        .iter()
        .map(|path| format!("operator/{path}"))
        .collect();
    if operator_paths != expected_operator_paths {
        return Err(Error::InvalidAsset);
    }
    Ok(Bundle {
        assets,
        manifest_sha256: checksum(&source),
        package_id: record.identity().id.to_string(),
        package_digest: record
            .digest()
            .to_string()
            .trim_start_matches("sha256:")
            .to_owned(),
    })
}

impl Asset {
    /// Returns the normalized relative asset path.
    #[must_use]
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    /// Returns the rendered bytes to write with an owner-private mode.
    #[must_use]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the SHA-256 checksum of exactly these rendered bytes.
    #[must_use]
    pub(crate) fn checksum(&self) -> String {
        checksum(&self.bytes)
    }
}

/// Versioned record proving which files this lifecycle owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ownership {
    /// Marker schema version.
    pub(crate) version: u32,
    /// Canonical Hermes home selected when the plugin was installed.
    pub(crate) hermes_home: String,
    /// Absolute external mutable policy selected by the installer.
    pub(crate) policy_path: String,
    /// Digest of the exact closed operator sidecar used for this install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) operator_sha256: Option<String>,
    /// Explicit immutable source used for this install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    /// Runtime package identity for package-backed assets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) package_id: Option<String>,
    /// Archive digest for package-backed assets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) package_digest: Option<String>,
    /// Rendered checksum for every immutable managed asset.
    #[serde(deserialize_with = "deserialize_checksums")]
    pub(crate) assets: BTreeMap<String, String>,
}

/// Renders a closed sidecar from an already snapshotted manifest and payload.
///
/// `read` returns the bytes of one `operator/<path>` payload file. The core
/// reporter is appended from the CLI binary.
pub(crate) fn render_source(
    policy_path: &Path,
    source: &[u8],
    mut read: impl FnMut(&str) -> Result<Vec<u8>, Error>,
) -> Result<Vec<Asset>, Error> {
    if !policy_path.is_absolute() || policy_path.to_str().is_none() {
        return Err(Error::UnsafePolicyPath);
    }
    let escaped = serde_json::to_string(policy_path.to_str().ok_or(Error::UnsafePolicyPath)?)
        .map_err(|_| Error::UnsafePolicyPath)?;
    let manifest = parse_operator_manifest(source, &mut read)?;
    let mut rendered = Vec::with_capacity(EXPECTED_PATHS.len());
    for record in manifest.payload {
        let bytes = read(&format!("operator/{}", record.path))?;
        if checksum(&bytes) != record.sha256 {
            return Err(Error::InvalidAsset);
        }
        let path = record.path;
        validate_relative_path(&path)?;
        let bytes = if path == "__init__.py" {
            replace_policy_token(&bytes, &escaped)?
        } else {
            bytes
        };
        rendered.push(Asset { path, bytes });
    }
    for (path, bytes) in CORE_OWNED_ASSETS {
        validate_relative_path(path)?;
        rendered.push(Asset {
            path: path.to_owned(),
            bytes: bytes.to_vec(),
        });
    }
    let rendered_paths: BTreeSet<_> = rendered.iter().map(|asset| asset.path.as_str()).collect();
    if rendered_paths.len() != EXPECTED_PATHS.len()
        || EXPECTED_PATHS
            .iter()
            .any(|path| !rendered_paths.contains(path))
    {
        return Err(Error::InvalidAsset);
    }
    Ok(rendered)
}

fn parse_operator_manifest(
    source: &[u8],
    read: &mut impl FnMut(&str) -> Result<Vec<u8>, Error>,
) -> Result<OperatorManifest, Error> {
    let text = std::str::from_utf8(source).map_err(|_| Error::InvalidAsset)?;
    let manifest: OperatorManifest =
        toml_edit::de::from_str(text).map_err(|_| Error::InvalidAsset)?;
    if manifest.schema != 1 || manifest.payload.len() != EXPECTED_PAYLOAD_PATHS.len() {
        return Err(Error::InvalidAsset);
    }
    let paths: Vec<_> = manifest
        .payload
        .iter()
        .map(|asset| asset.path.as_str())
        .collect();
    if paths != EXPECTED_PAYLOAD_PATHS
        || manifest
            .payload
            .iter()
            .any(|asset| !is_sha256(&asset.sha256))
    {
        return Err(Error::InvalidAsset);
    }
    for asset in &manifest.payload {
        let bytes = read(&format!("operator/{}", asset.path))?;
        if checksum(&bytes) != asset.sha256 {
            return Err(Error::InvalidAsset);
        }
    }
    Ok(manifest)
}

/// Builds a package-provenance marker for a snapshotted bundle and target.
pub(crate) fn ownership(
    hermes_home: &Path,
    policy_path: &Path,
    bundle: &Bundle,
) -> Result<Ownership, Error> {
    let hermes_home = hermes_home.to_str().ok_or(Error::UnsafeTarget)?;
    let policy_path = policy_path.to_str().ok_or(Error::UnsafePolicyPath)?;
    if !Path::new(hermes_home).is_absolute() || !Path::new(policy_path).is_absolute() {
        return Err(Error::UnsafeTarget);
    }
    let mut checksums = BTreeMap::new();
    for asset in &bundle.assets {
        validate_relative_path(asset.path())?;
        if checksums
            .insert(asset.path.clone(), asset.checksum())
            .is_some()
        {
            return Err(Error::InvalidAsset);
        }
    }
    if !has_current_paths(&checksums)
        || !is_sha256(&bundle.manifest_sha256)
        || bundle.package_id != PACKAGE_ID
        || !is_sha256(&bundle.package_digest)
    {
        return Err(Error::InvalidAsset);
    }
    Ok(Ownership {
        version: MARKER_VERSION,
        hermes_home: hermes_home.to_owned(),
        policy_path: policy_path.to_owned(),
        operator_sha256: Some(bundle.manifest_sha256.clone()),
        source: Some(PACKAGE_SOURCE.to_owned()),
        package_id: Some(bundle.package_id.clone()),
        package_digest: Some(bundle.package_digest.clone()),
        assets: checksums,
    })
}

/// Serializes the ownership marker deterministically.
pub(crate) fn marker_bytes(ownership: &Ownership) -> Result<Vec<u8>, Error> {
    serde_json::to_vec_pretty(ownership).map_err(|_| Error::InvalidMarker)
}

/// Parses an ownership marker without accepting unknown fields or versions.
pub(crate) fn parse_marker(bytes: &[u8]) -> Result<Ownership, Error> {
    let ownership: Ownership = serde_json::from_slice(bytes).map_err(|_| Error::InvalidMarker)?;
    let inventory_valid =
        has_current_paths(&ownership.assets) || has_previous_paths(&ownership.assets);
    let provenance_valid = match ownership.version {
        1 => {
            ownership.operator_sha256.is_none()
                && ownership.source.is_none()
                && ownership.package_id.is_none()
                && ownership.package_digest.is_none()
        }
        2 => {
            ownership.operator_sha256.as_deref().is_some_and(is_sha256)
                && ownership.source.is_none()
                && ownership.package_id.is_none()
                && ownership.package_digest.is_none()
                && has_current_paths(&ownership.assets)
        }
        MARKER_VERSION => {
            ownership.operator_sha256.as_deref().is_some_and(is_sha256)
                && has_current_paths(&ownership.assets)
                && match ownership.source.as_deref() {
                    Some(EMBEDDED_SOURCE) => {
                        ownership.package_id.is_none() && ownership.package_digest.is_none()
                    }
                    Some(PACKAGE_SOURCE) => {
                        ownership.package_id.as_deref() == Some(PACKAGE_ID)
                            && ownership.package_digest.as_deref().is_some_and(is_sha256)
                    }
                    _ => false,
                }
        }
        _ => false,
    };
    if !inventory_valid
        || !provenance_valid
        || !Path::new(&ownership.hermes_home).is_absolute()
        || !Path::new(&ownership.policy_path).is_absolute()
    {
        return Err(Error::InvalidMarker);
    }
    for (path, checksum) in &ownership.assets {
        validate_relative_path(path).map_err(|_| Error::InvalidMarker)?;
        if !is_sha256(checksum) {
            return Err(Error::InvalidMarker);
        }
    }
    Ok(ownership)
}

/// Computes a lowercase SHA-256 checksum without exposing asset contents.
#[must_use]
pub(crate) fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn replace_policy_token(template: &[u8], escaped_path: &str) -> Result<Vec<u8>, Error> {
    let template = std::str::from_utf8(template).map_err(|_| Error::InvalidAsset)?;
    let (prefix, suffix) = template
        .split_once(POLICY_TOKEN)
        .ok_or(Error::InvalidAsset)?;
    if suffix.contains(POLICY_TOKEN) {
        return Err(Error::InvalidAsset);
    }
    Ok(format!("{prefix}{escaped_path}{suffix}").into_bytes())
}

fn validate_relative_path(path: &str) -> Result<(), Error> {
    let path = Path::new(path);
    if path.is_absolute()
        || path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir
                    | std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(Error::InvalidAsset);
    }
    Ok(())
}

fn has_current_paths(checksums: &BTreeMap<String, String>) -> bool {
    checksums.len() == EXPECTED_PATHS.len()
        && EXPECTED_PATHS
            .iter()
            .all(|path| checksums.contains_key(*path))
        && CORE_OWNED_ASSET_PATHS
            .iter()
            .all(|path| checksums.contains_key(*path))
}

fn has_previous_paths(checksums: &BTreeMap<String, String>) -> bool {
    checksums.len() == PREVIOUS_PATHS.len()
        && PREVIOUS_PATHS
            .iter()
            .all(|path| checksums.contains_key(*path))
}

pub(crate) fn has_previous_inventory(ownership: &Ownership) -> bool {
    has_previous_paths(&ownership.assets)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| {
            byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte.is_ascii_hexdigit())
        })
}

fn deserialize_checksums<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Checksums;

    impl<'de> Visitor<'de> for Checksums {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a checksum object with unique paths")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut checksums = BTreeMap::new();
            while let Some((path, checksum)) = map.next_entry::<String, String>()? {
                if checksums.insert(path.clone(), checksum).is_some() {
                    return Err(A::Error::custom("duplicate asset path"));
                }
            }
            Ok(checksums)
        }
    }

    deserializer.deserialize_map(Checksums)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use pohunek_test_support::env::TestEnv;

    use super::*;
    use crate::hermes_integration::{test_package, test_python};

    #[test]
    fn renderer_replaces_exactly_one_policy_token_and_owns_every_asset() {
        let policy_path = Path::new("/work/policy\\\"quoted-á.json");
        let bundle = test_package::bundle(policy_path).expect("rendered package bundle");
        let init = bundle
            .assets
            .iter()
            .find(|asset| asset.path() == "__init__.py")
            .expect("init asset");
        let init = std::str::from_utf8(init.bytes()).expect("utf8 init");
        assert!(!init.contains(POLICY_TOKEN));
        let literal = serde_json::to_string(policy_path.to_str().expect("utf8 policy path"))
            .expect("policy literal");
        assert!(init.contains(&format!("POLICY_PATH = {literal}")));
        let ownership = ownership(
            Path::new("/work/hermes"),
            Path::new("/work/policy.json"),
            &bundle,
        )
        .expect("ownership");
        assert_eq!(ownership.assets.len(), 9);
        assert_eq!(
            ownership.operator_sha256.as_deref(),
            Some(bundle.manifest_sha256.as_str())
        );
        assert_eq!(ownership.source.as_deref(), Some(PACKAGE_SOURCE));
        assert!(!ownership.assets.contains_key(MARKER_NAME));
        assert_eq!(
            parse_marker(&marker_bytes(&ownership).expect("marker")).expect("parsed"),
            ownership
        );
    }

    #[test]
    fn rendered_init_is_valid_python_for_escaped_absolute_paths() {
        let env = TestEnv::new().expect("hermetic test environment");
        let bundle = test_package::bundle(Path::new("/work/policy\\\"quoted-á.json"))
            .expect("rendered package bundle");
        let init = bundle
            .assets
            .iter()
            .find(|asset| asset.path() == "__init__.py")
            .expect("init asset");
        let path = env.root().join("__init__.py");
        fs::write(&path, init.bytes()).expect("write isolated source");
        let status = env
            .command(test_python::interpreter())
            .args([
                "-I",
                "-c",
                "import ast, pathlib; source = pathlib.Path(__import__('sys').argv[1]).read_text(encoding='utf-8'); tree = ast.parse(source); compile(tree, '<asset>', 'exec')",
            ])
            .arg(&path)
            .status()
            .expect("start controlled local Python");
        assert!(status.success(), "rendered init must parse and compile");
    }

    /// Copies the packaged hook, the core reporter, and the start-identity
    /// suite into an isolated tree and runs them on the host interpreter, so
    /// the suite never imports from the source tree.
    #[test]
    fn start_identity_suite_passes_on_the_host_interpreter() {
        let env = TestEnv::new().expect("hermetic test environment");
        let bundle =
            test_package::bundle(Path::new("/work/policy.json")).expect("rendered package bundle");
        let asset = |path: &str| {
            bundle
                .assets
                .iter()
                .find(|asset| asset.path() == path)
                .unwrap_or_else(|| panic!("rendered asset {path}"))
                .bytes()
        };
        let hooks = asset("hooks.py");
        let reporter = asset("_pohunek_reporter.py");
        for (relative, bytes) in [
            ("pohunek/hooks.py", hooks),
            ("pohunek/_pohunek_reporter.py", reporter),
            (
                "tests/test_start_identity.py",
                include_bytes!("assets/tests/test_start_identity.py").as_slice(),
            ),
        ] {
            let path = env.root().join(relative);
            fs::create_dir_all(path.parent().expect("suite parent")).expect("suite directory");
            fs::write(&path, bytes).expect("write suite file");
        }
        let output = env
            .command(test_python::interpreter())
            .args(["-I", "-B"])
            .arg(env.root().join("tests/test_start_identity.py"))
            .output()
            .expect("start controlled host Python");
        assert!(
            output.status.success(),
            "start identity suite failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Production code compiles the core reporter in with `include_bytes!` and
    /// never resolves a source-tree location at run time.
    #[test]
    fn production_code_has_no_runtime_source_tree_lookup() {
        let sources = [
            ("assets.rs", include_str!("assets.rs")),
            ("doctor.rs", include_str!("doctor.rs")),
            ("error.rs", include_str!("error.rs")),
            ("lifecycle.rs", include_str!("lifecycle.rs")),
            ("mod.rs", include_str!("mod.rs")),
            ("policy.rs", include_str!("policy.rs")),
            ("runner.rs", include_str!("runner.rs")),
            ("skill.rs", include_str!("skill.rs")),
            ("target.rs", include_str!("target.rs")),
        ];
        for (name, source) in sources {
            let production = source
                .rfind("\n#[cfg(test)]\nmod tests")
                .map_or(source, |end| &source[..end]);
            for forbidden in ["CARGO_MANIFEST_DIR", "env!(\"CARGO", "option_env!"] {
                assert!(
                    !production.contains(forbidden),
                    "{name} resolves a build-tree location through {forbidden}"
                );
            }
        }
    }

    #[test]
    fn marker_rejects_unknown_missing_duplicate_and_malformed_checksums() {
        let bundle =
            test_package::bundle(Path::new("/work/policy.json")).expect("rendered package bundle");
        let ownership = ownership(
            Path::new("/work/hermes"),
            Path::new("/work/policy.json"),
            &bundle,
        )
        .expect("ownership");
        let document = String::from_utf8(marker_bytes(&ownership).expect("marker")).expect("utf8");
        let cases = [
            document.replacen("plugin.yaml", "unexpected.py", 1),
            document.replacen("plugin.yaml", "./plugin.yaml", 1),
            document.replacen("plugin.yaml", "../plugin.yaml", 1),
            document.replacen("plugin.yaml", "/plugin.yaml", 1),
            document.replacen("\"hermes_home\": \"/work/hermes\"", "\"hermes_home\": \"relative\"", 1),
            document.replacen("\"policy_path\": \"/work/policy.json\"", "\"policy_path\": \"relative\"", 1),
            document.replacen(&ownership.assets["plugin.yaml"], "not-a-checksum", 1),
            document.replacen("\"assets\": {", "\"assets\": {\n    \"plugin.yaml\": \"0000000000000000000000000000000000000000000000000000000000000000\",", 1),
        ];
        for case in cases {
            assert_eq!(parse_marker(case.as_bytes()), Err(Error::InvalidMarker));
        }
    }

    #[test]
    fn marker_accepts_absolute_trailing_slash_target_metadata() {
        for (hermes_home, policy_path) in [
            ("/work/hermes/", "/work/config/policy.json"),
            (
                "/work/pohunek-hermes-lifecycle-x/pohunek/hermes/",
                "/work/pohunek-hermes-lifecycle-x/config/policy.json",
            ),
        ] {
            let bundle =
                test_package::bundle(Path::new(policy_path)).expect("rendered package bundle");
            let ownership = ownership(Path::new(hermes_home), Path::new(policy_path), &bundle)
                .expect("ownership");
            let marker = marker_bytes(&ownership).expect("marker");
            assert_eq!(parse_marker(&marker).expect("parsed marker"), ownership);
        }
    }

    #[test]
    fn marker_accepts_only_the_previous_exact_asset_inventory() {
        let bundle =
            test_package::bundle(Path::new("/work/policy.json")).expect("rendered package bundle");
        let ownership = ownership(
            Path::new("/work/hermes"),
            Path::new("/work/policy.json"),
            &bundle,
        )
        .expect("ownership");
        let mut previous = ownership.clone();
        previous.version = 1;
        previous.operator_sha256 = None;
        previous.source = None;
        previous.package_id = None;
        previous.package_digest = None;
        previous.assets.remove("_pohunek_reporter.py");
        assert!(has_previous_inventory(&previous));
        assert_eq!(
            parse_marker(&marker_bytes(&previous).expect("previous marker"))
                .expect("previous marker remains readable"),
            previous
        );

        let mut reporter_release = ownership.clone();
        reporter_release.version = 1;
        reporter_release.operator_sha256 = None;
        reporter_release.source = None;
        reporter_release.package_id = None;
        reporter_release.package_digest = None;
        assert_eq!(
            parse_marker(&marker_bytes(&reporter_release).expect("v1 reporter marker"))
                .expect("previous S2 marker remains readable"),
            reporter_release
        );

        let mut missing_provenance = ownership.clone();
        missing_provenance.operator_sha256 = None;
        assert_eq!(
            parse_marker(&marker_bytes(&missing_provenance).expect("v2 marker without digest")),
            Err(Error::InvalidMarker)
        );
        let mut unexpected_provenance = reporter_release.clone();
        unexpected_provenance.operator_sha256 = Some("0".repeat(64));
        assert_eq!(
            parse_marker(&marker_bytes(&unexpected_provenance).expect("v1 marker with digest")),
            Err(Error::InvalidMarker)
        );

        let mut previous_v2 = ownership.clone();
        previous_v2.version = 2;
        previous_v2.source = None;
        previous_v2.package_id = None;
        previous_v2.package_digest = None;
        assert_eq!(
            parse_marker(&marker_bytes(&previous_v2).expect("v2 marker"))
                .expect("S3a marker remains readable"),
            previous_v2
        );

        previous
            .assets
            .insert("arbitrary.py".to_owned(), "0".repeat(64));
        assert!(!has_previous_inventory(&previous));
        assert_eq!(
            parse_marker(&marker_bytes(&previous).expect("arbitrary marker")),
            Err(Error::InvalidMarker)
        );
    }
}

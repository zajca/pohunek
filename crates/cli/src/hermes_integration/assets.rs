//! Embedded plugin assets and their explicit ownership contract.

// Rust guideline compliant 2026-09-29

#![expect(
    clippy::map_err_ignore,
    reason = "asset errors intentionally redact parser and path details from operator output"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};

use super::error::Error;

/// Version of the marker format written into one managed plugin root.
pub(crate) const MARKER_VERSION: u32 = 2;
/// The generated marker is deliberately not self-checksummed.
pub(crate) const MARKER_NAME: &str = ".pohunek-owned.json";
/// Closed operator sidecar embedded into the CLI for verification at install time.
const OPERATOR_MANIFEST: &[u8] = include_bytes!("../../../../compat/hermes/operator.toml");
/// Payload files available to the closed operator manifest.
const OPERATOR_PAYLOAD: [(&str, &[u8]); 8] = [
    (
        "plugin.yaml",
        include_bytes!("../../../../compat/hermes/operator/plugin.yaml"),
    ),
    (
        "__init__.py",
        include_bytes!("../../../../compat/hermes/operator/__init__.py"),
    ),
    (
        "cli.py",
        include_bytes!("../../../../compat/hermes/operator/cli.py"),
    ),
    (
        "hooks.py",
        include_bytes!("../../../../compat/hermes/operator/hooks.py"),
    ),
    (
        "policy.py",
        include_bytes!("../../../../compat/hermes/operator/policy.py"),
    ),
    (
        "redact.py",
        include_bytes!("../../../../compat/hermes/operator/redact.py"),
    ),
    (
        "tools.py",
        include_bytes!("../../../../compat/hermes/operator/tools.py"),
    ),
    (
        "skills/pohunek/SKILL.md",
        include_bytes!("../../../../compat/hermes/operator/skills/pohunek/SKILL.md"),
    ),
];
/// The reporter stays embedded and owned exclusively by the core CLI.
const CORE_OWNED_ASSETS: [(&str, &[u8]); 1] = [(
    "_pohunek_reporter.py",
    include_bytes!("assets/pohunek/_pohunek_reporter.py"),
)];
const POLICY_TOKEN: &str = "__POHUNEK_POLICY_PATH__";
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
    /// Rendered checksum for every immutable managed asset.
    #[serde(deserialize_with = "deserialize_checksums")]
    pub(crate) assets: BTreeMap<String, String>,
}

/// Renders immutable assets for exactly one validated absolute policy path.
pub(crate) fn render(policy_path: &Path) -> Result<Vec<Asset>, Error> {
    if !policy_path.is_absolute() || policy_path.to_str().is_none() {
        return Err(Error::UnsafePolicyPath);
    }
    let escaped = serde_json::to_string(policy_path.to_str().ok_or(Error::UnsafePolicyPath)?)
        .map_err(|_| Error::UnsafePolicyPath)?;
    let manifest = verified_operator_manifest()?;
    let mut rendered = Vec::with_capacity(EXPECTED_PATHS.len());
    for record in manifest.payload {
        let (path, bytes) = OPERATOR_PAYLOAD
            .iter()
            .find(|(path, _)| *path == record.path)
            .ok_or(Error::InvalidAsset)?;
        if checksum(bytes) != record.sha256 {
            return Err(Error::InvalidAsset);
        }
        let path = *path;
        validate_relative_path(path)?;
        let bytes = if path == "__init__.py" {
            replace_policy_token(bytes, &escaped)?
        } else {
            bytes.to_vec()
        };
        rendered.push(Asset {
            path: path.to_owned(),
            bytes,
        });
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

/// Returns a parsed sidecar only when its closed schema and exact payload set match.
fn verified_operator_manifest() -> Result<OperatorManifest, Error> {
    verified_operator_manifest_bytes(OPERATOR_MANIFEST)
}

fn verified_operator_manifest_bytes(source: &[u8]) -> Result<OperatorManifest, Error> {
    let source = std::str::from_utf8(source).map_err(|_| Error::InvalidAsset)?;
    let manifest: OperatorManifest =
        toml_edit::de::from_str(source).map_err(|_| Error::InvalidAsset)?;
    if manifest.schema != 1 || manifest.payload.len() != EXPECTED_PAYLOAD_PATHS.len() {
        return Err(Error::InvalidAsset);
    }
    let paths: Vec<_> = manifest
        .payload
        .iter()
        .map(|asset| asset.path.as_str())
        .collect();
    if paths != EXPECTED_PAYLOAD_PATHS {
        return Err(Error::InvalidAsset);
    }
    if manifest
        .payload
        .iter()
        .any(|asset| !is_sha256(&asset.sha256))
    {
        return Err(Error::InvalidAsset);
    }
    for record in &manifest.payload {
        let (_, bytes) = OPERATOR_PAYLOAD
            .iter()
            .find(|(path, _)| *path == record.path)
            .ok_or(Error::InvalidAsset)?;
        if checksum(bytes) != record.sha256 {
            return Err(Error::InvalidAsset);
        }
    }
    Ok(manifest)
}

pub(crate) fn operator_sha256() -> Result<String, Error> {
    verified_operator_manifest()?;
    Ok(checksum(OPERATOR_MANIFEST))
}

/// Builds a marker from rendered assets and selected target metadata.
pub(crate) fn ownership(
    hermes_home: &Path,
    policy_path: &Path,
    assets: &[Asset],
) -> Result<Ownership, Error> {
    let hermes_home = hermes_home.to_str().ok_or(Error::UnsafeTarget)?;
    let policy_path = policy_path.to_str().ok_or(Error::UnsafePolicyPath)?;
    if !Path::new(hermes_home).is_absolute() || !Path::new(policy_path).is_absolute() {
        return Err(Error::UnsafeTarget);
    }
    let mut checksums = BTreeMap::new();
    for asset in assets {
        validate_relative_path(asset.path())?;
        if checksums
            .insert(asset.path.clone(), asset.checksum())
            .is_some()
        {
            return Err(Error::InvalidAsset);
        }
    }
    if !has_current_paths(&checksums) {
        return Err(Error::InvalidAsset);
    }
    Ok(Ownership {
        version: MARKER_VERSION,
        hermes_home: hermes_home.to_owned(),
        policy_path: policy_path.to_owned(),
        operator_sha256: Some(operator_sha256()?),
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
        1 => ownership.operator_sha256.is_none(),
        MARKER_VERSION => {
            ownership.operator_sha256.as_deref().is_some_and(is_sha256)
                && has_current_paths(&ownership.assets)
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
    use crate::hermes_integration::test_python;

    #[test]
    fn renderer_replaces_exactly_one_policy_token_and_owns_every_asset() {
        let policy_path = Path::new("/work/policy\\\"quoted-á.json");
        let assets = render(policy_path).expect("rendered assets");
        let init = assets
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
            &assets,
        )
        .expect("ownership");
        assert_eq!(ownership.assets.len(), 9);
        assert_eq!(
            ownership.operator_sha256.as_deref(),
            Some(checksum(OPERATOR_MANIFEST).as_str())
        );
        assert!(!ownership.assets.contains_key(MARKER_NAME));
        assert_eq!(
            parse_marker(&marker_bytes(&ownership).expect("marker")).expect("parsed"),
            ownership
        );
    }

    #[test]
    fn rendered_init_is_valid_python_for_escaped_absolute_paths() {
        let env = TestEnv::new().expect("hermetic test environment");
        let assets = render(Path::new("/work/policy\\\"quoted-á.json")).expect("rendered assets");
        let init = assets
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

    /// Copies the embedded hook and the start-identity suite into an isolated
    /// tree and runs them on the host interpreter, so the suite never reads the
    /// source tree.
    #[test]
    fn start_identity_suite_passes_on_the_host_interpreter() {
        let env = TestEnv::new().expect("hermetic test environment");
        let hooks = OPERATOR_PAYLOAD
            .iter()
            .find(|(path, _)| *path == "hooks.py")
            .expect("embedded hooks asset")
            .1;
        let reporter = CORE_OWNED_ASSETS
            .iter()
            .find(|(path, _)| *path == "_pohunek_reporter.py")
            .expect("embedded core reporter asset")
            .1;
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

    /// Production code embeds assets with `include_bytes!` and never resolves a
    /// source-tree location at run time.
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
        let assets = render(Path::new("/work/policy.json")).expect("rendered assets");
        let ownership = ownership(
            Path::new("/work/hermes"),
            Path::new("/work/policy.json"),
            &assets,
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
            let assets = render(Path::new(policy_path)).expect("rendered assets");
            let ownership = ownership(Path::new(hermes_home), Path::new(policy_path), &assets)
                .expect("ownership");
            let marker = marker_bytes(&ownership).expect("marker");
            assert_eq!(parse_marker(&marker).expect("parsed marker"), ownership);
        }
    }

    #[test]
    fn marker_accepts_only_the_previous_exact_asset_inventory() {
        let assets = render(Path::new("/work/policy.json")).expect("rendered assets");
        let ownership = ownership(
            Path::new("/work/hermes"),
            Path::new("/work/policy.json"),
            &assets,
        )
        .expect("ownership");
        let mut previous = ownership.clone();
        previous.version = 1;
        previous.operator_sha256 = None;
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

        previous
            .assets
            .insert("arbitrary.py".to_owned(), "0".repeat(64));
        assert!(!has_previous_inventory(&previous));
        assert_eq!(
            parse_marker(&marker_bytes(&previous).expect("arbitrary marker")),
            Err(Error::InvalidMarker)
        );
    }

    #[test]
    fn operator_manifest_is_closed_and_hashes_only_allowlisted_payload() {
        let source = std::str::from_utf8(OPERATOR_MANIFEST).expect("manifest UTF-8");
        verified_operator_manifest_bytes(source.as_bytes()).expect("valid embedded manifest");

        let unknown_field = source.replacen("schema = 1\n", "schema = 1\nunexpected = true\n", 1);
        assert!(matches!(
            verified_operator_manifest_bytes(unknown_field.as_bytes()),
            Err(Error::InvalidAsset)
        ));
        let wrong_payload_hash = source.replacen(
            "9e666f85a2d9f99bc99cc0d997f146d52e28bd862cb6e387312551939a077625",
            &"0".repeat(64),
            1,
        );
        assert!(matches!(
            verified_operator_manifest_bytes(wrong_payload_hash.as_bytes()),
            Err(Error::InvalidAsset)
        ));
        let repeated_path = source.replacen("path = \"__init__.py\"", "path = \"plugin.yaml\"", 1);
        assert!(matches!(
            verified_operator_manifest_bytes(repeated_path.as_bytes()),
            Err(Error::InvalidAsset)
        ));
    }
}

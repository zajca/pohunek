//! Embedded plugin assets and their explicit ownership contract.

// Rust guideline compliant 2026-08-06

#![expect(
    clippy::map_err_ignore,
    reason = "asset errors intentionally redact parser and path details from operator output"
)]

use std::collections::BTreeMap;
use std::path::Path;

use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};

use super::error::Error;

/// Version of the marker format written into one managed plugin root.
pub(crate) const MARKER_VERSION: u32 = 1;
/// The generated marker is deliberately not self-checksummed.
pub(crate) const MARKER_NAME: &str = ".pohunek-owned.json";
/// Immutable embedded production assets, in normalized relative-path order.
const ASSETS: [(&str, &[u8]); 7] = [
    ("plugin.yaml", include_bytes!("assets/pohunek/plugin.yaml")),
    ("__init__.py", include_bytes!("assets/pohunek/__init__.py")),
    ("cli.py", include_bytes!("assets/pohunek/cli.py")),
    ("hooks.py", include_bytes!("assets/pohunek/hooks.py")),
    ("policy.py", include_bytes!("assets/pohunek/policy.py")),
    ("redact.py", include_bytes!("assets/pohunek/redact.py")),
    ("tools.py", include_bytes!("assets/pohunek/tools.py")),
];
const SKILL_PATH: &str = "skills/pohunek/SKILL.md";
const SKILL: &[u8] = include_bytes!("assets/pohunek/skills/pohunek/SKILL.md");
const POLICY_TOKEN: &str = "__POHUNEK_POLICY_PATH__";
const EXPECTED_PATHS: [&str; 8] = [
    "plugin.yaml",
    "__init__.py",
    "cli.py",
    "hooks.py",
    "policy.py",
    "redact.py",
    "tools.py",
    SKILL_PATH,
];

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
    let mut rendered = Vec::with_capacity(ASSETS.len() + 1);
    for (path, bytes) in ASSETS {
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
    rendered.push(Asset {
        path: SKILL_PATH.to_owned(),
        bytes: SKILL.to_vec(),
    });
    Ok(rendered)
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
    if !has_expected_paths(&checksums) {
        return Err(Error::InvalidAsset);
    }
    Ok(Ownership {
        version: MARKER_VERSION,
        hermes_home: hermes_home.to_owned(),
        policy_path: policy_path.to_owned(),
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
    if ownership.version != MARKER_VERSION
        || !has_expected_paths(&ownership.assets)
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

fn has_expected_paths(checksums: &BTreeMap<String, String>) -> bool {
    checksums.len() == EXPECTED_PATHS.len()
        && EXPECTED_PATHS
            .iter()
            .all(|path| checksums.contains_key(*path))
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
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir() -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "pohunek-hermes-assets-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create isolated test directory");
        TempDir(path)
    }

    #[test]
    fn renderer_replaces_exactly_one_policy_token_and_owns_every_asset() {
        let policy_path = Path::new("/tmp/policy\\\"quoted-á.json");
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
            Path::new("/tmp/hermes"),
            Path::new("/tmp/policy.json"),
            &assets,
        )
        .expect("ownership");
        assert_eq!(ownership.assets.len(), 8);
        assert!(!ownership.assets.contains_key(MARKER_NAME));
        assert_eq!(
            parse_marker(&marker_bytes(&ownership).expect("marker")).expect("parsed"),
            ownership
        );
    }

    #[test]
    fn rendered_init_is_valid_python_for_escaped_absolute_paths() {
        let temp = temp_dir();
        let assets = render(Path::new("/tmp/policy\\\"quoted-á.json")).expect("rendered assets");
        let init = assets
            .iter()
            .find(|asset| asset.path() == "__init__.py")
            .expect("init asset");
        let path = temp.0.join("__init__.py");
        fs::write(&path, init.bytes()).expect("write isolated source");
        let status = Command::new("/usr/bin/python3")
            .args([
                "-I",
                "-c",
                "import ast, pathlib; source = pathlib.Path(__import__('sys').argv[1]).read_text(encoding='utf-8'); tree = ast.parse(source); compile(tree, '<asset>', 'exec')",
            ])
            .arg(&path)
            .env_clear()
            .env("LANG", "C")
            .status()
            .expect("start controlled local Python");
        assert!(status.success(), "rendered init must parse and compile");
    }

    #[test]
    fn marker_rejects_unknown_missing_duplicate_and_malformed_checksums() {
        let assets = render(Path::new("/tmp/policy.json")).expect("rendered assets");
        let ownership = ownership(
            Path::new("/tmp/hermes"),
            Path::new("/tmp/policy.json"),
            &assets,
        )
        .expect("ownership");
        let document = String::from_utf8(marker_bytes(&ownership).expect("marker")).expect("utf8");
        let cases = [
            document.replacen("plugin.yaml", "unexpected.py", 1),
            document.replacen("plugin.yaml", "./plugin.yaml", 1),
            document.replacen("plugin.yaml", "../plugin.yaml", 1),
            document.replacen("plugin.yaml", "/plugin.yaml", 1),
            document.replacen("\"hermes_home\": \"/tmp/hermes\"", "\"hermes_home\": \"relative\"", 1),
            document.replacen("\"policy_path\": \"/tmp/policy.json\"", "\"policy_path\": \"relative\"", 1),
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
            ("/tmp/hermes/", "/tmp/config/policy.json"),
            (
                "/tmp/pohunek-hermes-lifecycle-x/pohunek/hermes/",
                "/tmp/pohunek-hermes-lifecycle-x/config/policy.json",
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
    fn embedded_skill_checksum_is_deterministic_and_owned_as_exact_bytes() {
        let first = render(Path::new("/tmp/policy.json")).expect("render first assets");
        let second = render(Path::new("/tmp/policy.json")).expect("render second assets");
        let first_skill = first
            .iter()
            .find(|asset| asset.path() == SKILL_PATH)
            .expect("first skill asset");
        let second_skill = second
            .iter()
            .find(|asset| asset.path() == SKILL_PATH)
            .expect("second skill asset");
        assert_eq!(first_skill.bytes(), SKILL);
        assert_eq!(first_skill.bytes(), second_skill.bytes());
        assert_eq!(first_skill.checksum(), second_skill.checksum());

        let ownership = ownership(
            Path::new("/tmp/hermes"),
            Path::new("/tmp/policy.json"),
            &first,
        )
        .expect("ownership");
        assert_eq!(ownership.assets[SKILL_PATH], checksum(SKILL));
    }
}

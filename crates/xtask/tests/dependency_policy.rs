//! Keeps the public-verification-only RSA exception within its reviewed graph.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Lockfile {
    package: Vec<Package>,
}

#[derive(Debug, Deserialize)]
struct Package {
    name: String,
    version: String,
    checksum: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[test]
fn rsa_exception_has_only_the_reviewed_oidc_dependency_parent() {
    let lock: Lockfile =
        toml::from_str(include_str!("../../../Cargo.lock")).expect("workspace lockfile");
    // These immutable registry checksums identify the exact source reviewed for
    // the exception. A version/source change must prompt another security review.
    for (name, version, checksum) in [
        (
            "rsa",
            "0.9.10",
            "b8573f03f5883dcaebdfcf4725caa1ecb9c15b2ef50c43a07b816e06799bb12d",
        ),
        (
            "openidconnect",
            "4.0.1",
            "0d8c6709ba2ea764bbed26bce1adf3c10517113ddea6f2d4196e4851757ef2b2",
        ),
    ] {
        let packages: Vec<_> = lock
            .package
            .iter()
            .filter(|package| package.name == name)
            .collect();
        assert_eq!(
            packages.len(),
            1,
            "RSA exception needs exactly one reviewed {name} version"
        );
        assert_eq!(
            packages[0].version, version,
            "reassess RSA exception after an upstream change"
        );
        assert_eq!(
            packages[0].checksum.as_deref(),
            Some(checksum),
            "reassess RSA exception after a source change"
        );
    }
    let parents: Vec<_> = lock
        .package
        .iter()
        .filter(|package| {
            package
                .dependencies
                .iter()
                .any(|dependency| dependency.split_whitespace().next() == Some("rsa"))
        })
        .map(|package| package.name.as_str())
        .collect();
    assert_eq!(
        parents,
        ["openidconnect"],
        "a new RSA consumer requires a new security assessment"
    );
    let consumers: Vec<_> = lock
        .package
        .iter()
        .filter(|package| {
            package
                .dependencies
                .iter()
                .any(|dependency| dependency.split_whitespace().next() == Some("openidconnect"))
        })
        .map(|package| package.name.as_str())
        .collect();
    assert_eq!(
        consumers,
        ["pohunek-relay"],
        "a new OIDC consumer invalidates the reviewed reverse dependency closure"
    );
}

#[test]
fn production_clippy_policy_rejects_private_rsa_even_through_aliases_and_local_allow() {
    use std::{fs, path::Path, process::Command};

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workspace: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("Cargo.toml")).expect("workspace manifest"))
            .expect("manifest TOML");
    let mut lints = workspace["workspace"]["lints"]["clippy"].clone();
    for lint in ["disallowed_types", "disallowed_methods"] {
        assert_eq!(
            lints[lint].as_str(),
            Some("deny"),
            "workspace must reject private RSA by default"
        );
    }
    for source in [
        include_str!("../../relay/src/lib.rs"),
        include_str!("../../relay/src/bin/pohunek-relayd.rs"),
    ] {
        assert!(
            source
                .lines()
                .any(|line| line
                    == "#![forbid(clippy::disallowed_types, clippy::disallowed_methods)]"),
            "every production RSA consumer root must forbid local overrides"
        );
    }
    // Mirror the production root's forbid while remaining compatible with
    // unrelated workspace Clap derives, which emit broad generated allows.
    for lint in ["disallowed_types", "disallowed_methods"] {
        lints[lint] = toml::Value::String("forbid".to_owned());
    }
    let fixture = tempfile::tempdir().expect("isolated policy fixture");
    fs::create_dir(fixture.path().join("src")).expect("fixture source");
    fs::copy(
        root.join(".clippy.toml"),
        fixture.path().join(".clippy.toml"),
    )
    .expect("production Clippy policy");
    let mut dependency = workspace["workspace"]["dependencies"]["openidconnect"].clone();
    assert_eq!(dependency["version"].as_str(), Some("4.0.1"));
    assert_eq!(dependency["default-features"].as_bool(), Some(false));
    assert_eq!(
        dependency["features"]
            .as_array()
            .expect("reviewed features"),
        &[toml::Value::String("reqwest".to_owned())]
    );
    assert_member_features(&root);
    dependency["version"] = toml::Value::String("=4.0.1".to_owned());
    let manifest = fixture_manifest(dependency, lints);
    fs::write(
        fixture.path().join("Cargo.toml"),
        toml::to_string(&manifest).expect("fixture manifest"),
    )
    .expect("write manifest");
    // Reuse the already downloaded reviewed dependency versions rather than
    // resolving newer cached versions while preparing the isolated root crate.
    fs::copy(root.join("Cargo.lock"), fixture.path().join("Cargo.lock"))
        .expect("reviewed dependency resolution");
    fs::write(fixture.path().join("src/main.rs"), "fn main() {}").expect("initial fixture source");
    let resolution = Command::new("cargo")
        .current_dir(fixture.path())
        .args(["tree", "--offline", "--prefix", "none"])
        .output()
        .expect("resolve isolated fixture");
    assert!(
        resolution.status.success(),
        "fixture resolution failed: {}",
        String::from_utf8_lossy(&resolution.stderr)
    );
    assert_reviewed_resolution(&fixture.path().join("Cargo.lock"));
    for (source, expected) in [
        (
            include_str!("fixtures/rsa_direct.rs"),
            &["clippy::disallowed_types", "clippy::disallowed_methods"][..],
        ),
        (
            include_str!("fixtures/rsa_alias.rs"),
            &["clippy::disallowed_types", "clippy::disallowed_methods"][..],
        ),
        (include_str!("fixtures/rsa_alias_allow.rs"), &["E0453"][..]),
    ] {
        fs::write(fixture.path().join("src/main.rs"), source).expect("write negative fixture");
        let output = Command::new("cargo")
            .current_dir(fixture.path())
            .args([
                "clippy",
                "--offline",
                "--locked",
                "--message-format=json",
                "--",
                "-D",
                "warnings",
            ])
            .env("CARGO_TARGET_DIR", root.join("target/rsa-policy"))
            .output()
            .expect("run production Clippy policy in isolated target");
        assert_diagnostics(&output, expected);
    }
}

fn assert_reviewed_resolution(path: &std::path::Path) {
    let reviewed: Lockfile =
        toml::from_str(include_str!("../../../Cargo.lock")).expect("reviewed resolution");
    let resolved: Lockfile =
        toml::from_str(&std::fs::read_to_string(path).expect("fixture lockfile"))
            .expect("fixture resolution");
    for package in resolved
        .package
        .iter()
        .filter(|package| package.name != "rsa-policy-probe")
    {
        assert!(
            reviewed
                .package
                .iter()
                .any(|known| known.name == package.name
                    && known.version == package.version
                    && known.checksum == package.checksum),
            "isolated fixture must retain reviewed versions: {package:?}"
        );
    }
}

fn assert_member_features(root: &std::path::Path) {
    // no-deps metadata enumerates every workspace member, including newly added
    // members/globs, without downloading irrelevant target dependencies.
    let output = std::process::Command::new("cargo")
        .current_dir(root)
        .args([
            "metadata",
            "--locked",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ])
        .output()
        .expect("workspace metadata");
    assert!(output.status.success(), "workspace metadata failed");
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("metadata JSON");
    let mut consumers = Vec::new();
    for package in metadata["packages"].as_array().expect("workspace packages") {
        for dependency in package["dependencies"]
            .as_array()
            .expect("all dependency declarations")
        {
            if dependency["name"] == "openidconnect" {
                assert_eq!(
                    dependency["features"],
                    serde_json::json!(["reqwest"]),
                    "reassess local, renamed, or target-specific OIDC features"
                );
                assert_eq!(dependency["uses_default_features"], false);
                consumers.push(package["name"].as_str().expect("package name").to_owned());
            }
        }
    }
    assert_eq!(consumers, ["pohunek-relay"]);
}

fn fixture_manifest(dependency: toml::Value, lints: toml::Value) -> toml::Value {
    toml::Value::Table(toml::map::Map::from_iter([
        (
            "package".to_owned(),
            toml::Value::Table(toml::map::Map::from_iter([
                (
                    "name".to_owned(),
                    toml::Value::String("rsa-policy-probe".to_owned()),
                ),
                (
                    "version".to_owned(),
                    toml::Value::String("0.0.0".to_owned()),
                ),
                ("edition".to_owned(), toml::Value::String("2021".to_owned())),
            ])),
        ),
        (
            "workspace".to_owned(),
            toml::Value::Table(toml::map::Map::new()),
        ),
        (
            "dependencies".to_owned(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "openidconnect".to_owned(),
                dependency,
            )])),
        ),
        (
            "lints".to_owned(),
            toml::Value::Table(toml::map::Map::from_iter([("clippy".to_owned(), lints)])),
        ),
    ]))
}

fn assert_diagnostics(output: &std::process::Output, expected: &[&str]) {
    assert!(
        !output.status.success(),
        "private RSA must not compile under production policy"
    );
    let codes: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value["reason"] == "compiler-message")
        .filter_map(|value| value["message"]["code"]["code"].as_str().map(str::to_owned))
        .collect();
    for code in expected {
        assert!(
            codes.iter().any(|actual| actual == code),
            "missing diagnostic {code}; codes={codes:?}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

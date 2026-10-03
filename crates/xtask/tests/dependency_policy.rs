//! Keeps the public-verification-only RSA exception within its reviewed graph.

use pohunek_test_support::env::TestEnv;
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
    use std::fs;

    let root = pohunek_test_support::workspace_root();
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
    let env = TestEnv::new().expect("hermetic test environment");
    // The environment's private working directory is the isolated fixture crate.
    let fixture = env.cwd();
    fs::create_dir(fixture.join("src")).expect("fixture source");
    fs::copy(root.join(".clippy.toml"), fixture.join(".clippy.toml"))
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
    assert_member_features(&env, &root);
    dependency["version"] = toml::Value::String("=4.0.1".to_owned());
    let manifest = fixture_manifest(dependency, lints);
    fs::write(
        fixture.join("Cargo.toml"),
        toml::to_string(&manifest).expect("fixture manifest"),
    )
    .expect("write manifest");
    // Reuse the already downloaded reviewed dependency versions rather than
    // resolving newer cached versions while preparing the isolated root crate.
    fs::copy(root.join("Cargo.lock"), fixture.join("Cargo.lock"))
        .expect("reviewed dependency resolution");
    fs::write(fixture.join("src/main.rs"), "fn main() {}").expect("initial fixture source");
    let resolution = host_cargo(&env)
        .current_dir(fixture)
        .args(["tree", "--offline", "--prefix", "none"])
        .output()
        .expect("resolve isolated fixture");
    assert!(
        resolution.status.success(),
        "fixture resolution failed: {}",
        String::from_utf8_lossy(&resolution.stderr)
    );
    assert_reviewed_resolution(&fixture.join("Cargo.lock"));
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
        fs::write(fixture.join("src/main.rs"), source).expect("write negative fixture");
        let output = host_cargo(&env)
            .current_dir(fixture)
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

/// A `cargo` command in the scrubbed environment of `env`.
///
/// The fixture resolves offline against the dependency versions already in the
/// developer's registry cache, so the cargo and rustup homes (and a pinned
/// toolchain) are passed on explicitly; everything else the environment drops.
/// The homes default to `~/.cargo` and `~/.rustup` of the test process.
fn host_cargo(env: &TestEnv) -> std::process::Command {
    let mut command = env.command("cargo");
    let host_home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    for (name, default_dir) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
        let value = std::env::var_os(name)
            .or_else(|| host_home.as_ref().map(|home| home.join(default_dir).into()));
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    if let Some(toolchain) = std::env::var_os("RUSTUP_TOOLCHAIN") {
        command.env("RUSTUP_TOOLCHAIN", toolchain);
    }
    command
}

fn assert_member_features(env: &TestEnv, root: &std::path::Path) {
    // no-deps metadata enumerates every workspace member, including newly added
    // members/globs, without downloading irrelevant target dependencies.
    let output = host_cargo(env)
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

/// zbus must stay on its runtime-agnostic backend. Cargo unifies features across
/// the graph, so a `tokio` feature on any member makes zbus require a Tokio
/// reactor for every consumer; the GUI's theme detection drives zbus from a
/// non-Tokio thread and panics at startup with "there is no reactor running".
#[test]
fn zbus_is_never_built_with_its_tokio_backend() {
    let env = TestEnv::new().expect("hermetic test environment");
    let root = pohunek_test_support::workspace_root();
    let output = host_cargo(&env)
        .current_dir(&root)
        .args(["metadata", "--locked", "--offline", "--format-version", "1"])
        .output()
        .expect("workspace metadata");
    assert!(
        output.status.success(),
        "workspace metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("metadata JSON");
    // Third-party crates may bring their own zbus major (the CLI's keyring
    // pulls zbus 4); only the zbus that workspace members link against matters.
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolve nodes");
    let members: Vec<&str> = metadata["workspace_members"]
        .as_array()
        .expect("workspace members")
        .iter()
        .map(|member| member.as_str().expect("member id"))
        .collect();
    let zbus_ids: Vec<&str> = nodes
        .iter()
        .filter(|node| node["id"].as_str().is_some_and(|id| members.contains(&id)))
        .flat_map(|node| node["deps"].as_array().expect("node deps"))
        .filter(|dependency| dependency["name"] == "zbus")
        .map(|dependency| dependency["pkg"].as_str().expect("dependency id"))
        .collect();
    assert!(!zbus_ids.is_empty(), "the systemd backend depends on zbus");
    for node in nodes
        .iter()
        .filter(|node| node["id"].as_str().is_some_and(|id| zbus_ids.contains(&id)))
    {
        let features = node["features"].as_array().expect("resolved features");
        assert!(
            !features.iter().any(|feature| feature == "tokio"),
            "zbus resolved with its `tokio` feature; that makes the GUI panic at startup \
             (\"there is no reactor running\") because its theme detection uses zbus off \
             a Tokio runtime. Keep the workspace dependency on `async-io`."
        );
        assert!(
            features.iter().any(|feature| feature == "async-io"),
            "zbus needs the runtime-agnostic `async-io` backend"
        );
    }
}

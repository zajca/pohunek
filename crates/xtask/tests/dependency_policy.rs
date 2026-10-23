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

/// Offline `cargo metadata` of the whole workspace, covering every target and,
/// when `all_features` is set, every optional dependency edge.
fn workspace_metadata(all_features: bool) -> serde_json::Value {
    let env = TestEnv::new().expect("hermetic test environment");
    let root = pohunek_test_support::workspace_root();
    let mut command = host_cargo(&env);
    command
        .current_dir(&root)
        .args(["metadata", "--locked", "--offline", "--format-version", "1"]);
    if all_features {
        command.arg("--all-features");
    }
    let output = command.output().expect("workspace metadata");
    assert!(
        output.status.success(),
        "workspace metadata failed; this offline resolution covers the crates of every \
         target, so it needs the sources of every locked crate in the cargo registry. \
         Prerequisite: run `cargo fetch --locked` once (the CI heavy job does). \
         cargo said: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("metadata JSON")
}

/// Without `default-rt` (and without `tokio`), `zbus::connection::Builder::build`
/// reports `Error::Unsupported` unless a runtime is passed to it; the systemd
/// backend passes none, so every zbus a workspace member depends on directly
/// needs `default-rt`.
#[test]
fn zbus_keeps_its_default_runtime() {
    let metadata = workspace_metadata(false);
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
            features.iter().any(|feature| feature == "default-rt"),
            "zbus needs its `default-rt` backend: the systemd backend builds connections \
             without passing a runtime"
        );
    }
}

/// `smartstring` (RUSTSEC-2026-0249, ignored in `.cargo/audit.toml`) is locked
/// only through the optional `ts` feature chain; no default-feature build may
/// link it.
#[test]
fn smartstring_is_absent_from_the_default_feature_graph() {
    let metadata = workspace_metadata(false);
    let names: std::collections::BTreeMap<&str, &str> = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|package| {
            (
                package["id"].as_str().expect("package id"),
                package["name"].as_str().expect("package name"),
            )
        })
        .collect();
    let linked: Vec<&str> = metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolve nodes")
        .iter()
        .map(|node| names[node["id"].as_str().expect("node id")])
        .collect();
    assert!(
        !linked.contains(&"smartstring"),
        "a default-feature dependency now reaches smartstring; the RUSTSEC-2026-0249 ignore \
         in .cargo/audit.toml is only valid for the optional `ts` feature chain"
    );
}

/// Violations of the legacy zbus policy in `metadata` (`cargo metadata` JSON
/// of the whole workspace with every feature).
///
/// zbus below 6 and its split-out `zvariant`/`zbus_names` crates are
/// unmaintained (RUSTSEC-2026-0370, -0371, -0372). Workspace members link zbus
/// 6. The only remaining chain is `pohunek-cli` -> `keyring 3` (its own
/// `zbus 4` dependency and `secret-service 4` behind it), which
/// `.cargo/audit.toml` ignores for exactly that chain:
/// - only `keyring 3` and `secret-service 4` may depend on the legacy family;
/// - only `keyring 3` may depend on `secret-service 4`;
/// - only the workspace crate `pohunek-cli` may depend on `keyring 3`.
fn legacy_zbus_violations(metadata: &serde_json::Value) -> Vec<String> {
    use std::collections::BTreeMap;

    // package id -> (name, major version)
    let packages: BTreeMap<&str, (&str, u64)> = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|package| {
            let version = package["version"].as_str().expect("package version");
            let major = version
                .split('.')
                .next()
                .and_then(|major| major.parse().ok())
                .expect("numeric major version");
            (
                package["id"].as_str().expect("package id"),
                (package["name"].as_str().expect("package name"), major),
            )
        })
        .collect();
    let is_legacy = |id: &str| match packages[id] {
        ("zbus" | "zbus_macros", major) => major < 6,
        ("zvariant" | "zvariant_derive" | "zvariant_utils" | "zbus_names", _) => true,
        _ => false,
    };
    let mut violations = Vec::new();
    let mut legacy_found = false;
    for node in metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolve nodes")
    {
        let parent = node["id"].as_str().expect("node id");
        let parent_package = packages[parent];
        for dependency in node["deps"].as_array().expect("node deps") {
            let child = dependency["pkg"].as_str().expect("dependency id");
            let child_package = packages[child];
            if is_legacy(child) && !is_legacy(parent) {
                legacy_found = true;
                if !matches!(parent_package, ("secret-service", 4) | ("keyring", 3)) {
                    violations.push(format!(
                        "`{}` {}.x depends on `{}`, part of the unmaintained zbus < 6 family",
                        parent_package.0, parent_package.1, child_package.0
                    ));
                }
            }
            if child_package == ("secret-service", 4) && parent_package != ("keyring", 3) {
                violations.push(format!(
                    "`{}` {}.x depends on `secret-service` 4, which only `keyring` 3 may",
                    parent_package.0, parent_package.1
                ));
            }
            if child_package == ("keyring", 3) && parent_package.0 != "pohunek-cli" {
                violations.push(format!(
                    "`{}` {}.x depends on `keyring` 3, which only `pohunek-cli` may",
                    parent_package.0, parent_package.1
                ));
            }
        }
    }
    if !legacy_found {
        violations.push(
            "no legacy zbus consumer remains: remove the RUSTSEC-2026-0370/0371/0372 ignores \
             from .cargo/audit.toml and this test"
                .to_owned(),
        );
    }
    violations
}

#[test]
fn legacy_zbus_family_is_reachable_only_through_the_keyring_secret_service_chain() {
    let violations = legacy_zbus_violations(&workspace_metadata(true));
    assert!(
        violations.is_empty(),
        "{}; the family must otherwise be zbus 6 (workspace dependency `zbus`)",
        violations.join("; ")
    );
}

/// Synthetic `cargo metadata` of the permitted chain plus `extra` edges
/// (`parent -> child`, each named `name@major`).
fn synthetic_metadata(extra: &[(&str, &str)]) -> serde_json::Value {
    let mut edges = vec![
        ("pohunek-cli@0", "keyring@3"),
        ("keyring@3", "zbus@4"),
        ("keyring@3", "secret-service@4"),
        ("secret-service@4", "zbus@4"),
        ("zbus@4", "zvariant@4"),
        ("pohunek-platform@0", "zbus@6"),
    ];
    edges.extend_from_slice(extra);
    let mut ids: Vec<&str> = edges.iter().flat_map(|(a, b)| [*a, *b]).collect();
    ids.sort_unstable();
    ids.dedup();
    let packages: Vec<_> = ids
        .iter()
        .map(|id| {
            let (name, major) = id.split_once('@').expect("name@major");
            serde_json::json!({"id": id, "name": name, "version": format!("{major}.0.0")})
        })
        .collect();
    let nodes: Vec<_> = ids
        .iter()
        .map(|id| {
            let deps: Vec<_> = edges
                .iter()
                .filter(|(parent, _)| parent == id)
                .map(|(_, child)| serde_json::json!({"pkg": child}))
                .collect();
            serde_json::json!({"id": id, "deps": deps})
        })
        .collect();
    serde_json::json!({"packages": packages, "resolve": {"nodes": nodes}})
}

#[test]
fn legacy_zbus_policy_accepts_the_reviewed_chain() {
    assert_eq!(
        legacy_zbus_violations(&synthetic_metadata(&[])),
        Vec::<String>::new()
    );
}

#[test]
fn legacy_zbus_policy_rejects_any_other_consumer_of_the_chain() {
    for (extra, expected) in [
        (
            ("other@1", "secret-service@4"),
            "`other` 1.x depends on `secret-service` 4",
        ),
        (
            ("other@1", "keyring@3"),
            "`other` 1.x depends on `keyring` 3",
        ),
        (
            ("pohunek-relay@0", "keyring@3"),
            "`pohunek-relay` 0.x depends on `keyring` 3",
        ),
        (("other@1", "zbus@4"), "`other` 1.x depends on `zbus`"),
        (("other@1", "zbus@5"), "`other` 1.x depends on `zbus`"),
        (
            ("other@1", "zvariant@4"),
            "`other` 1.x depends on `zvariant`",
        ),
        (
            ("pohunek-cli@0", "secret-service@4"),
            "`pohunek-cli` 0.x depends on `secret-service` 4",
        ),
    ] {
        let violations = legacy_zbus_violations(&synthetic_metadata(&[extra]));
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(expected)),
            "{extra:?} must be rejected as `{expected}`; got {violations:?}"
        );
    }
}

#[test]
fn legacy_zbus_policy_asks_for_the_ignores_to_go_once_the_chain_is_gone() {
    let mut metadata = synthetic_metadata(&[]);
    for node in metadata["resolve"]["nodes"].as_array_mut().expect("nodes") {
        node["deps"] = serde_json::json!([]);
    }
    let violations = legacy_zbus_violations(&metadata);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("no legacy zbus consumer remains")),
        "{violations:?}"
    );
}

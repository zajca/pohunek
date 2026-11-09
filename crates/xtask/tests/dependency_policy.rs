//! Dependency-graph policy of the workspace: the unmaintained zbus family stays out of the
//! lockfile, zbus keeps its runtime, and the optional `ts` chain stays out of default builds.

use pohunek_test_support::env::TestEnv;

/// A `cargo` command in the scrubbed environment of `env`.
///
/// Used for offline `cargo metadata` of the workspace: it resolves against the
/// registry cache of the host's cargo, so the cargo and rustup homes (and a
/// pinned toolchain) are passed on explicitly; everything else the environment
/// drops. The homes default to `~/.cargo` and `~/.rustup` of the test process.
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

/// zbus below 6 and its split-out `zvariant`/`zbus_names` crates are unmaintained
/// (RUSTSEC-2026-0370, -0371, -0372); every workspace dependency must be on zbus 6.
#[test]
fn the_unmaintained_zbus_family_is_absent_from_the_lock() {
    let metadata = workspace_metadata(true);
    let legacy: Vec<String> = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .filter_map(|package| {
            let name = package["name"].as_str().expect("package name");
            let version = package["version"].as_str().expect("package version");
            let major: u64 = version.split('.').next()?.parse().ok()?;
            let legacy = match name {
                "zbus" | "zbus_macros" => major < 6,
                "zvariant" | "zvariant_derive" | "zvariant_utils" | "zbus_names" => true,
                _ => false,
            };
            legacy.then(|| format!("{name} {version}"))
        })
        .collect();
    assert!(
        legacy.is_empty(),
        "the legacy zbus family is locked again: {legacy:?}"
    );
}

//! Tests of host profiles bound to an installed package at the session
//! registry: fresh launches, resume, relay approval and package retention,
//! over a real plugin root and real session registries.

// Rust guideline compliant 2026-10-04

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use package::registry::{Registry, RegistryError};
use package::{Limits, PackageDigest};
use pohunek_test_support::wait::HANG_GUARD;
use protocol::{PackageErrorKind, PackageUninstallParams, SessionNewParams};

use super::packages::PackageUninstallError;
use super::tests::{assigned_agent_script, hermetic_shell, params, temp_dir, temp_store_path};
use super::{SessionRegistry, SessionRegistryConfig};
use crate::agent::host::fixture::{installed_pi_host, PI_SHAPED_PACKAGE_ID};
use crate::agent::host::{LaunchSource, RuntimeHost};

/// The package fixture, its host and an agents directory holding a profile
/// pinned to it.
struct Pinned {
    plugins: PathBuf,
    agents: PathBuf,
    state: PathBuf,
    store_path: PathBuf,
    dir: PathBuf,
    digest: PackageDigest,
    host: RuntimeHost,
}

impl Pinned {
    /// Installs the package launching `script` and writes the profile
    /// `pinned` bound to it.
    fn new(tag: &str, script: &Path, dir: &Path) -> Self {
        let plugins = temp_dir(&format!("{tag}-plugins")).join("plugins");
        let (host, digest) = installed_pi_host(&plugins, script);
        let agents = temp_dir(&format!("{tag}-agents"));
        let state = temp_dir(&format!("{tag}-state"));
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("private state");
        fs::write(
            agents.join("pinned.toml"),
            format!("base = \"pi\"\npackage = \"{PI_SHAPED_PACKAGE_ID}\"\ndigest = \"{digest}\"\n"),
        )
        .expect("write profile");
        Self {
            plugins,
            agents,
            state,
            store_path: temp_store_path(tag),
            dir: dir.to_path_buf(),
            digest,
            host,
        }
    }

    fn registry(&self) -> SessionRegistry {
        SessionRegistry::new_with_runtimes(
            SessionRegistryConfig {
                shell_command: hermetic_shell(),
                stop_grace: Duration::from_millis(50),
                store_path: Some(self.store_path.clone()),
                agents_dir: Some(self.agents.clone()),
                host_state_dir: Some(self.state.clone()),
                ..SessionRegistryConfig::default()
            },
            self.host.clone(),
        )
    }

    fn package_registry(&self) -> Registry {
        Registry::open_at(&self.plugins, Limits::DEFAULT).expect("registry")
    }

    fn new_params(&self) -> SessionNewParams {
        SessionNewParams {
            agent: "pinned".to_owned(),
            cwd: Some(self.dir.clone()),
            ..params()
        }
    }

    fn disable(&self) {
        self.package_registry()
            .set_enabled(&self.digest, false)
            .expect("disable");
        self.host.reload().expect("reload");
    }
}

#[tokio::test]
async fn an_owner_local_pinned_profile_launches_and_records_the_pinned_digest() {
    let dir = temp_dir("pin-launch");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let pinned = Pinned::new("pin-launch", &script, &dir);
    let registry = pinned.registry();

    let created = registry
        .create(pinned.new_params())
        .await
        .expect("a pinned profile launches");

    let retained = registry.retained_package_digests().await.expect("retained");
    assert!(retained.contains(&pinned.digest));
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn a_disabled_package_refuses_a_fresh_launch_through_an_owner_local_pinned_profile() {
    let dir = temp_dir("pin-disabled-local");
    let marker = dir.join("argv.txt");
    let (script, mut gate) = assigned_agent_script(&dir, &marker);
    let pinned = Pinned::new("pin-disabled-local", &script, &dir);
    let registry = pinned.registry();
    let created = registry
        .create(pinned.new_params())
        .await
        .expect("create a pinned session");
    gate.write_all(b"go\n").expect("release the exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");

    pinned.disable();

    let fresh = registry
        .create(pinned.new_params())
        .await
        .expect_err("a disabled package serves no fresh launch");
    assert_eq!(fresh.code, "runtime_not_installed");
    registry
        .resume(&created.id)
        .await
        .expect("the session pinned to the digest still resumes");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn a_disabled_package_refuses_a_fresh_launch_through_a_relay_approved_pinned_profile() {
    let dir = temp_dir("pin-disabled-relay");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let pinned = Pinned::new("pin-disabled-relay", &script, &dir);
    let registry = pinned.registry();
    let profiles = &registry.inner.profiles;
    let agent = profiles.resolve_agent("pinned").expect("resolves");
    let approved = profiles
        .revision_of(&agent)
        .expect("revision key")
        .expect("a profile has a revision");
    let relay = LaunchSource::RelayProfile {
        profile: "pinned".to_owned(),
        revision: approved,
    };

    let resolved = relay
        .resolve(profiles)
        .expect("the approved revision resolves");
    drop(
        registry
            .guard_package_launch(&resolved.definition)
            .await
            .expect("an enabled package may launch"),
    );

    pinned.disable();

    let resolved = relay
        .resolve(profiles)
        .expect("disabling does not change the approved revision");
    let refused = registry
        .guard_package_launch(&resolved.definition)
        .await
        .expect_err("a disabled package refuses a relay launch");
    assert_eq!(refused.code, "runtime_not_installed");
}

#[tokio::test]
async fn a_profile_pin_alone_retains_the_package_and_refuses_uninstall() {
    let dir = temp_dir("pin-retained");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let pinned = Pinned::new("pin-retained", &script, &dir);
    let registry = pinned.registry();

    let retained = registry.retained_package_digests().await.expect("retained");
    assert!(retained.contains(&pinned.digest));

    let refused = registry
        .uninstall_package(&pinned.digest)
        .await
        .expect_err("a profile pins the package");
    assert!(matches!(
        refused,
        PackageUninstallError::Registry(RegistryError::StillReferenced)
    ));

    let listed = registry.package_list().await.expect("list");
    assert!(listed.packages[0].referenced);
    assert_eq!(
        registry
            .package_uninstall(PackageUninstallParams {
                digest: pinned.digest.clone(),
                remove_modified: false,
            })
            .await
            .expect_err("referenced"),
        PackageErrorKind::Referenced
    );

    fs::remove_file(pinned.agents.join("pinned.toml")).expect("remove the profile");
    registry
        .uninstall_package(&pinned.digest)
        .await
        .expect("an unpinned package uninstalls");
}

#[tokio::test]
async fn a_profile_pin_refuses_removal_of_a_modified_root() {
    let dir = temp_dir("pin-modified");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let pinned = Pinned::new("pin-modified", &script, &dir);
    let registry = pinned.registry();
    let hex = pinned
        .digest
        .as_str()
        .strip_prefix("sha256:")
        .expect("digest prefix");
    let descriptor = pinned
        .plugins
        .join("packages")
        .join(hex)
        .join("files")
        .join("runtime.toml");
    let mut bytes = fs::read(&descriptor).expect("read descriptor");
    let at = bytes
        .windows(b"fast".len())
        .position(|window| window == b"fast")
        .expect("model argument");
    bytes[at..at + 4].copy_from_slice(b"slow");
    fs::write(&descriptor, bytes).expect("modify the root");

    assert_eq!(
        registry
            .package_uninstall(PackageUninstallParams {
                digest: pinned.digest.clone(),
                remove_modified: true,
            })
            .await
            .expect_err("a pinned modified root is retained"),
        PackageErrorKind::Referenced
    );
    assert!(descriptor.exists());

    fs::remove_file(pinned.agents.join("pinned.toml")).expect("remove the profile");
    registry
        .package_uninstall(PackageUninstallParams {
            digest: pinned.digest.clone(),
            remove_modified: true,
        })
        .await
        .expect("unpinned, the modified root is removed");
}

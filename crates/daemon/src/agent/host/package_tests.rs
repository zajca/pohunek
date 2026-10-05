//! Tests for package-backed runtimes: loading, reload, pin resolution and the
//! verification before launch, over real archives installed into a real
//! owner-private package store.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use package::registry::{InstallRequest, PackageSource as InstallSource, Registry, RegistryError};
use package::verify::VerifyError;
use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
use protocol::{
    BindingProvenance, LaunchBinding, PackageId, PackageIdentity, PackageVersion, RuntimeId,
    RuntimeRef,
};

use super::{
    BuiltinSource, LaunchPin, PackageRejection, PackageSource, PackageStore, ReloadError,
    RuntimeHost, RuntimeRegistry,
};

/// Install time recorded for every fixture package; the registry never reads
/// the clock.
const INSTALLED_AT: u64 = 1_700_000_000;

/// Package-relative path of the detection manifest the fixture descriptor
/// names.
const DETECT_PATH: &str = "detect/default.toml";

const DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

/// A temporary plugin root the daemon-side store opens.
struct Plugins {
    dir: tempfile::TempDir,
}

impl Plugins {
    fn new() -> Self {
        Self {
            dir: pohunek_test_support::tempdir().expect("private test directory"),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("plugins")
    }

    fn store(&self) -> PackageStore {
        PackageStore::open(&self.path()).expect("open the package store")
    }

    fn registry(&self) -> Registry {
        Registry::open_at(&self.path(), Limits::DEFAULT).expect("open the registry")
    }

    fn host(&self) -> RuntimeHost {
        RuntimeHost::with_packages(
            BuiltinSource::new("/bin/sh"),
            PackageSource::new(self.store()),
        )
        .expect("host builds")
    }

    /// Root directory of an installed package.
    fn root(&self, digest: &PackageDigest) -> PathBuf {
        let hex = digest
            .as_str()
            .strip_prefix("sha256:")
            .expect("digest prefix");
        self.path().join("packages").join(hex)
    }
}

/// An archive and the identity it is installed under.
struct Built {
    bytes: Vec<u8>,
    digest: PackageDigest,
    identity: PackageIdentity,
}

fn identity(id: &str, version: &str) -> PackageIdentity {
    PackageIdentity {
        id: PackageId::parse(id).expect("package id"),
        version: PackageVersion::parse(version).expect("package version"),
    }
}

/// The descriptor of a package that exports runtime `runtime`.
fn descriptor(package_id: &str, version: &str, runtime: &str) -> String {
    format!(
        r#"schema = 1
id = "{package_id}"
version = "{version}"
runtime_api = 1

[runtime]
id = "{runtime}"
name = "Acme"
program = "acme-agent"
args = ["--model", "fast"]
detect_manifest = "{DETECT_PATH}"
prompt_arg = true

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["--session", "{{reference}}"]

[fork]
supported = false

[native_reference]
strategy = "hook"
"#
    )
}

fn entries(descriptor: &str, extra: &str) -> Vec<ArchiveEntry> {
    vec![
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: descriptor.as_bytes().to_vec(),
            executable: false,
        },
        ArchiveEntry {
            path: DETECT_PATH.to_owned(),
            contents: DETECT_MANIFEST.as_bytes().to_vec(),
            executable: false,
        },
        ArchiveEntry {
            path: "notes.txt".to_owned(),
            contents: extra.as_bytes().to_vec(),
            executable: false,
        },
    ]
}

fn build(descriptor: &str, extra: &str, identity: PackageIdentity) -> Built {
    let bytes = build_archive(&entries(descriptor, extra), &Limits::DEFAULT).expect("archive");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("archive reads")
        .digest()
        .clone();
    Built {
        bytes,
        digest,
        identity,
    }
}

/// The package `acme.agent` at `version` exporting runtime `acme`.
fn acme(version: &str) -> Built {
    build(
        &descriptor("acme.agent", version, "acme"),
        version,
        identity("acme.agent", version),
    )
}

fn install(registry: &Registry, built: &Built, enabled: bool, select: bool) {
    registry
        .install(&InstallRequest {
            archive: &built.bytes,
            expected: &built.digest,
            identity: built.identity.clone(),
            source: InstallSource::ExplicitDigest,
            enabled,
            select,
            installed_at_unix_seconds: INSTALLED_AT,
        })
        .expect("install");
}

fn runtime(value: &str) -> RuntimeId {
    RuntimeId::parse(value).expect("runtime id")
}

/// The launch pin a session of `built` freezes.
fn pin_of(built: &Built, runtime_id: &str) -> LaunchPin {
    LaunchPin::Pinned(Box::new(LaunchBinding {
        runtime_id: runtime(runtime_id),
        provenance: BindingProvenance::Package {
            package: built.identity.clone(),
            package_digest: built.digest.clone(),
        },
    }))
}

fn code(error: &protocol::ProtocolError) -> &str {
    &error.code
}

#[test]
fn an_enabled_selected_package_serves_a_package_origin_runtime_next_to_the_builtins() {
    let plugins = Plugins::new();
    let built = acme("1.0.0");
    install(&plugins.registry(), &built, true, true);

    let host = plugins.host();

    let definition = host.resolve_id(&runtime("acme")).expect("package runtime");
    assert_eq!(
        definition.binding().provenance,
        BindingProvenance::Package {
            package: built.identity.clone(),
            package_digest: built.digest.clone(),
        }
    );
    assert_eq!(definition.program().as_str(), "acme-agent");
    for builtin in ["shell", "codex", "claude", "hermes"] {
        host.resolve_id(&runtime(builtin)).expect("built-in");
    }
    assert_eq!(*host.package_report(), super::PackageReport::default());
    host.verify_launchable(&definition).expect("launchable");
}

#[test]
fn the_package_detection_manifest_is_read_from_the_verified_package() {
    let plugins = Plugins::new();
    install(&plugins.registry(), &acme("1.0.0"), true, true);

    let host = plugins.host();

    let definition = host.resolve_id(&runtime("acme")).expect("package runtime");
    assert!(
        !definition.manifest().required_regions().is_empty(),
        "the package manifest's rules are in effect"
    );
}

#[test]
fn an_unselected_or_disabled_package_is_not_served_to_fresh_launches() {
    let plugins = Plugins::new();
    let registry = plugins.registry();
    let unselected = acme("1.0.0");
    install(&registry, &unselected, true, false);
    let host = plugins.host();
    assert_eq!(
        code(&host.resolve_id(&runtime("acme")).expect_err("unselected")),
        "runtime_not_installed"
    );

    registry.select(&unselected.digest).expect("select");
    host.reload().expect("reload");
    host.resolve_id(&runtime("acme")).expect("selected");

    registry
        .set_enabled(&unselected.digest, false)
        .expect("disable");
    host.reload().expect("reload");
    assert_eq!(
        code(&host.resolve_id(&runtime("acme")).expect_err("disabled")),
        "runtime_not_installed"
    );
}

#[test]
fn a_disabled_package_still_serves_the_sessions_pinned_to_it() {
    let plugins = Plugins::new();
    let registry = plugins.registry();
    let built = acme("1.0.0");
    install(&registry, &built, true, true);
    let host = plugins.host();
    let definition = host.resolve_id(&runtime("acme")).expect("package runtime");
    registry.set_enabled(&built.digest, false).expect("disable");
    host.reload().expect("reload");

    let resolved = host
        .resolve_pinned(&RuntimeRef::from_wire("acme"), &pin_of(&built, "acme"))
        .expect("the pin resolves from its own digest");

    assert_eq!(resolved.binding(), definition.binding());
    // A fresh launch of the still-held definition is refused.
    assert_eq!(
        code(&host.verify_launchable(&definition).expect_err("disabled")),
        "runtime_not_installed"
    );
}

#[test]
fn reload_picks_up_installs_and_uninstalls_and_keeps_the_builtins() {
    let plugins = Plugins::new();
    let host = plugins.host();
    host.resolve_id(&runtime("acme"))
        .expect_err("the runtime is not served");

    let built = acme("1.0.0");
    install(&plugins.registry(), &built, true, true);
    assert!(
        host.resolve_id(&runtime("acme")).is_err(),
        "nothing is reloaded implicitly"
    );
    host.reload().expect("reload");
    host.resolve_id(&runtime("acme")).expect("installed");

    plugins
        .registry()
        .uninstall(&built.digest, &package::registry::RetainedDigests::new())
        .expect("uninstall");
    host.reload().expect("reload");
    host.resolve_id(&runtime("acme"))
        .expect_err("the runtime is not served");
    host.resolve_id(&runtime("codex")).expect("built-in stays");
}

#[test]
fn a_clone_of_the_host_observes_a_reload() {
    let plugins = Plugins::new();
    let host = plugins.host();
    let clone = host.clone();
    install(&plugins.registry(), &acme("1.0.0"), true, true);

    host.reload().expect("reload");

    clone.resolve_id(&runtime("acme")).expect("shared snapshot");
}

#[test]
fn a_host_without_a_package_store_cannot_reload() {
    let host = RuntimeHost::new(
        RuntimeRegistry::from_sources(&[&BuiltinSource::new("/bin/sh")]).expect("registry"),
    );
    assert_eq!(
        host.reload().expect_err("no store"),
        ReloadError::NotReloadable
    );
}

#[test]
fn a_pin_resolves_from_its_digest_after_a_newer_version_is_selected() {
    let plugins = Plugins::new();
    let registry = plugins.registry();
    let first = acme("1.0.0");
    let second = acme("2.0.0");
    install(&registry, &first, true, true);
    let host = plugins.host();
    install(&registry, &second, true, false);
    registry.select(&second.digest).expect("select v2");
    host.reload().expect("reload");

    let fresh = host.resolve_id(&runtime("acme")).expect("selected");
    let pinned = host
        .resolve_pinned(&RuntimeRef::from_wire("acme"), &pin_of(&first, "acme"))
        .expect("the v1 pin keeps resolving");

    assert_eq!(
        fresh.binding().provenance,
        BindingProvenance::Package {
            package: second.identity.clone(),
            package_digest: second.digest.clone(),
        }
    );
    assert_eq!(
        pinned.binding().provenance,
        BindingProvenance::Package {
            package: first.identity.clone(),
            package_digest: first.digest.clone(),
        }
    );
}

#[test]
fn a_pin_to_a_package_that_is_not_installed_is_not_installed() {
    let plugins = Plugins::new();
    let host = plugins.host();
    let ghost = acme("1.0.0");

    let error = host
        .resolve_pinned(&RuntimeRef::from_wire("acme"), &pin_of(&ghost, "acme"))
        .expect_err("not installed");

    assert_eq!(code(&error), "runtime_not_installed");
}

#[test]
fn a_package_pin_never_falls_back_to_a_builtin_or_another_package() {
    let plugins = Plugins::new();
    let registry = plugins.registry();
    let other = acme("1.0.0");
    install(&registry, &other, true, true);
    let host = plugins.host();
    let ghost = build(
        &descriptor("ghost.agent", "1.0.0", "claude"),
        "ghost",
        identity("ghost.agent", "1.0.0"),
    );

    // A built-in id pinned to a package that is not installed.
    let builtin_id = host
        .resolve_pinned(&RuntimeRef::claude(), &pin_of(&ghost, "claude"))
        .expect_err("the built-in does not stand in");
    assert_eq!(code(&builtin_id), "runtime_not_installed");

    // The pin names an installed digest but another runtime than that
    // package exports: installed content that contradicts the pin.
    let wrong_runtime = host
        .resolve_pinned(&RuntimeRef::from_wire("other"), &pin_of(&other, "other"))
        .expect_err("the package does not export that runtime");
    assert_eq!(code(&wrong_runtime), "runtime_incompatible");

    // The pin's runtime id and the reference disagree.
    let mismatched = host
        .resolve_pinned(&RuntimeRef::from_wire("acme"), &pin_of(&other, "claude"))
        .expect_err("reference and pin disagree");
    assert_eq!(code(&mismatched), "runtime_not_installed");
}

#[test]
fn a_pin_with_a_fabricated_identity_is_refused() {
    let plugins = Plugins::new();
    let built = acme("1.0.0");
    install(&plugins.registry(), &built, true, true);
    let host = plugins.host();
    let forged = Built {
        bytes: Vec::new(),
        digest: built.digest.clone(),
        identity: identity("acme.agent", "9.9.9"),
    };

    let error = host
        .resolve_pinned(&RuntimeRef::from_wire("acme"), &pin_of(&forged, "acme"))
        .expect_err("identity differs from the record");

    assert_eq!(code(&error), "runtime_incompatible");
}

/// How a test damages an installed root.
enum Tamper {
    /// Rewrites the descriptor with other bytes of the same length.
    SameSizeContent,
    /// Appends to the descriptor.
    Append,
    /// Adds a file the manifest does not list.
    AddFile,
    /// Removes the notes file.
    RemoveFile,
    /// Changes only the mode of the descriptor.
    Chmod,
    /// Deletes the whole root.
    RemoveRoot,
}

fn tamper(plugins: &Plugins, digest: &PackageDigest, how: &Tamper) {
    let root = plugins.root(digest);
    let descriptor = root.join("files").join("runtime.toml");
    match how {
        Tamper::SameSizeContent => {
            let mut bytes = fs::read(&descriptor).expect("read");
            let at = bytes
                .windows(b"fast".len())
                .position(|window| window == b"fast")
                .expect("model argument");
            bytes[at..at + 4].copy_from_slice(b"slow");
            fs::write(&descriptor, bytes).expect("write");
        }
        Tamper::Append => {
            let mut bytes = fs::read(&descriptor).expect("read");
            bytes.extend_from_slice(b"\n# appended\n");
            fs::write(&descriptor, bytes).expect("write");
        }
        Tamper::AddFile => fs::write(root.join("files").join("extra"), b"x").expect("write"),
        Tamper::RemoveFile => fs::remove_file(root.join("files").join("notes.txt")).expect("rm"),
        Tamper::Chmod => {
            fs::set_permissions(&descriptor, fs::Permissions::from_mode(0o644)).expect("chmod");
        }
        Tamper::RemoveRoot => fs::remove_dir_all(&root).expect("remove root"),
    }
}

#[test]
fn a_package_modified_after_install_never_launches_pins_or_loads() {
    let cases = [
        (Tamper::SameSizeContent, "runtime_incompatible"),
        (Tamper::Append, "runtime_incompatible"),
        (Tamper::AddFile, "runtime_incompatible"),
        (Tamper::RemoveFile, "runtime_incompatible"),
        (Tamper::Chmod, "runtime_incompatible"),
        (Tamper::RemoveRoot, "runtime_incompatible"),
    ];
    for (how, expected) in cases {
        let plugins = Plugins::new();
        let built = acme("1.0.0");
        install(&plugins.registry(), &built, true, true);
        let host = plugins.host();
        let definition = host.resolve_id(&runtime("acme")).expect("loaded");

        tamper(&plugins, &built.digest, &how);

        // The definition built before the tamper no longer passes the launch
        // check.
        let launch = host.verify_launchable(&definition).expect_err("launch");
        assert_eq!(code(&launch), expected);
        // The pin refuses to resolve; nothing else stands in.
        let pinned = host
            .resolve_pinned(&RuntimeRef::from_wire("acme"), &pin_of(&built, "acme"))
            .expect_err("pin");
        assert_eq!(code(&pinned), expected);
        // A reload leaves the package out, reports why, and keeps built-ins.
        let report = host.reload().expect("reload still succeeds");
        assert_eq!(report.rejected.len(), 1);
        assert_eq!(report.rejected[0].digest, built.digest);
        assert!(matches!(
            report.rejected[0].reason,
            PackageRejection::Root(_)
        ));
        host.resolve_id(&runtime("acme"))
            .expect_err("the runtime is not served");
        host.resolve_id(&runtime("codex")).expect("built-in stays");
    }
}

#[test]
fn tampering_is_reported_with_the_typed_cause_and_without_paths() {
    let plugins = Plugins::new();
    let built = acme("1.0.0");
    install(&plugins.registry(), &built, true, true);
    let host = plugins.host();
    tamper(&plugins, &built.digest, &Tamper::SameSizeContent);

    let report = host.reload().expect("reload");

    assert!(matches!(
        report.rejected[0].reason,
        PackageRejection::Root(VerifyError::Modified { .. })
    ));
    let text = report.rejected[0].reason.to_string();
    assert!(!text.contains(plugins.path().to_str().expect("utf-8 path")));
}

#[test]
fn an_uninstalled_package_fails_the_launch_check_as_not_installed() {
    let plugins = Plugins::new();
    let built = acme("1.0.0");
    install(&plugins.registry(), &built, true, true);
    let host = plugins.host();
    let definition = host.resolve_id(&runtime("acme")).expect("loaded");
    plugins
        .registry()
        .uninstall(&built.digest, &package::registry::RetainedDigests::new())
        .expect("uninstall");

    let error = host
        .verify_launchable(&definition)
        .expect_err("uninstalled");

    assert_eq!(code(&error), "runtime_not_installed");
}

#[test]
fn builtin_definitions_always_pass_the_launch_check() {
    let plugins = Plugins::new();
    let host = plugins.host();
    for id in ["shell", "codex", "claude", "hermes"] {
        let definition = host.resolve_id(&runtime(id)).expect("built-in");
        host.verify_launchable(&definition).expect("launchable");
    }
}

#[test]
fn a_broken_package_does_not_take_down_the_others_or_the_builtins() {
    let plugins = Plugins::new();
    let registry = plugins.registry();
    let good = acme("1.0.0");
    install(&registry, &good, true, true);
    let broken = build(
        &descriptor("broken.agent", "1.0.0", "broken").replace("schema = 1", "schema = 9"),
        "broken",
        identity("broken.agent", "1.0.0"),
    );
    install(&registry, &broken, true, true);

    let host = plugins.host();

    host.resolve_id(&runtime("acme")).expect("good package");
    host.resolve_id(&runtime("claude")).expect("built-in");
    let report = host.package_report();
    assert_eq!(report.rejected.len(), 1);
    assert_eq!(report.rejected[0].digest, broken.digest);
    assert!(matches!(
        report.rejected[0].reason,
        PackageRejection::Descriptor(_)
    ));
}

#[test]
fn packages_claiming_the_same_runtime_are_both_refused() {
    let plugins = Plugins::new();
    let registry = plugins.registry();
    let first = build(
        &descriptor("one.agent", "1.0.0", "shared"),
        "one",
        identity("one.agent", "1.0.0"),
    );
    let second = build(
        &descriptor("two.agent", "1.0.0", "shared"),
        "two",
        identity("two.agent", "1.0.0"),
    );
    install(&registry, &first, true, true);
    install(&registry, &second, true, true);

    let host = plugins.host();

    host.resolve_id(&runtime("shared"))
        .expect_err("the runtime is not served");
    let report = host.package_report();
    assert_eq!(report.rejected.len(), 2);
    assert!(report
        .rejected
        .iter()
        .all(|rejected| rejected.reason == PackageRejection::RuntimeIdConflict));
    host.resolve_id(&runtime("codex")).expect("built-in");
}

#[test]
fn a_package_cannot_claim_a_reserved_runtime_id() {
    for reserved in ["shell", "codex", "claude", "hermes"] {
        let plugins = Plugins::new();
        let built = build(
            &descriptor("evil.agent", "1.0.0", reserved),
            "evil",
            identity("evil.agent", "1.0.0"),
        );
        install(&plugins.registry(), &built, true, true);

        let host = plugins.host();

        let report = host.package_report();
        assert_eq!(report.rejected.len(), 1, "{reserved}");
        if reserved == "shell" {
            // The shell descriptor shape is refused before the id check runs.
            assert!(matches!(
                report.rejected[0].reason,
                PackageRejection::Descriptor(_)
            ));
        } else {
            assert_eq!(
                report.rejected[0].reason,
                PackageRejection::ReservedRuntimeId,
                "{reserved}"
            );
        }
        // The built-in keeps its own provenance.
        let definition = host.resolve_id(&runtime(reserved)).expect("built-in");
        assert!(matches!(
            definition.binding().provenance,
            BindingProvenance::Builtin { .. }
        ));
        // And a pin to the package cannot resolve either.
        let error = host
            .resolve_pinned(&RuntimeRef::from_wire(reserved), &pin_of(&built, reserved))
            .expect_err("reserved id");
        assert_eq!(code(&error), "runtime_incompatible");
    }
}

#[test]
fn a_descriptor_that_disagrees_with_the_recorded_identity_is_refused() {
    let plugins = Plugins::new();
    let built = build(
        &descriptor("acme.agent", "2.0.0", "acme"),
        "mismatch",
        identity("acme.agent", "1.0.0"),
    );
    install(&plugins.registry(), &built, true, true);

    let host = plugins.host();

    assert_eq!(
        host.package_report().rejected[0].reason,
        PackageRejection::IdentityMismatch
    );
    host.resolve_id(&runtime("acme"))
        .expect_err("the runtime is not served");
}

#[test]
fn a_package_without_a_descriptor_or_with_a_bad_detection_manifest_is_refused() {
    let plugins = Plugins::new();
    let registry = plugins.registry();
    let no_descriptor = {
        let archive = build_archive(
            &[ArchiveEntry {
                path: "other.txt".to_owned(),
                contents: b"x".to_vec(),
                executable: false,
            }],
            &Limits::DEFAULT,
        )
        .expect("archive");
        let digest = read_archive(&archive, &Limits::DEFAULT)
            .expect("reads")
            .digest()
            .clone();
        Built {
            bytes: archive,
            digest,
            identity: identity("empty.agent", "1.0.0"),
        }
    };
    install(&registry, &no_descriptor, true, true);
    let unknown_manifest = build(
        &descriptor("nomanifest.agent", "1.0.0", "nomanifest")
            .replace(DETECT_PATH, "detect/missing.toml"),
        "x",
        identity("nomanifest.agent", "1.0.0"),
    );
    install(&registry, &unknown_manifest, true, true);

    let host = plugins.host();

    let reasons: Vec<_> = host
        .package_report()
        .rejected
        .iter()
        .map(|rejected| (rejected.digest.clone(), rejected.reason.clone()))
        .collect();
    assert!(reasons.contains(&(no_descriptor.digest, PackageRejection::DescriptorMissing)));
    assert!(reasons.contains(&(
        unknown_manifest.digest,
        PackageRejection::Descriptor(super::DefinitionError::UnknownManifest)
    )));
}

#[test]
fn an_unreadable_registry_record_degrades_to_the_builtins_and_blocks_reload() {
    let plugins = Plugins::new();
    let built = acme("1.0.0");
    install(&plugins.registry(), &built, true, true);
    let host = plugins.host();
    host.resolve_id(&runtime("acme")).expect("loaded");
    fs::write(plugins.path().join("registry.json"), b"{ not json").expect("corrupt the record");

    // The running host keeps its registry; the reload reports the fault.
    let error = host.reload().expect_err("corrupt record");
    assert_eq!(error, ReloadError::Packages(RegistryError::Corrupt));
    host.resolve_id(&runtime("acme"))
        .expect("previous snapshot");

    // A host started on the corrupt record serves only the built-ins and
    // reports the fault; the package pin is incompatible, never a fallback.
    let restarted = plugins.host();
    assert_eq!(
        restarted.package_report().fault,
        Some(RegistryError::Corrupt)
    );
    restarted
        .resolve_id(&runtime("acme"))
        .expect_err("the runtime is not served");
    restarted.resolve_id(&runtime("codex")).expect("built-in");
    let pinned = restarted
        .resolve_pinned(&RuntimeRef::from_wire("acme"), &pin_of(&built, "acme"))
        .expect_err("registry unreadable");
    assert_eq!(code(&pinned), "runtime_incompatible");
}

#[test]
fn the_plugin_root_must_be_owner_private() {
    let plugins = Plugins::new();
    let _ = plugins.store();
    fs::set_permissions(plugins.path(), fs::Permissions::from_mode(0o755)).expect("chmod");

    PackageStore::open(&plugins.path()).expect_err("a group-readable plugin root is refused");
}

#[test]
fn a_running_session_stays_controllable_while_its_package_is_installed() {
    let plugins = Plugins::new();
    let registry = plugins.registry();
    let built = acme("1.0.0");
    install(&registry, &built, true, true);
    let host = plugins.host();
    let reference = RuntimeRef::from_wire("acme");
    let pin = pin_of(&built, "acme");
    host.ensure_session_runtime(&reference, &pin)
        .expect("served");

    registry.set_enabled(&built.digest, false).expect("disable");
    host.reload().expect("reload");
    host.ensure_session_runtime(&reference, &pin)
        .expect("a disabled package keeps its sessions controllable");
    assert_eq!(
        code(
            &host
                .ensure_session_runtime(&reference, &LaunchPin::Unpinned)
                .expect_err("an unpinned session has nothing that keeps it")
        ),
        "runtime_not_installed"
    );

    registry
        .uninstall(&built.digest, &package::registry::RetainedDigests::new())
        .expect("uninstall");
    assert_eq!(
        code(
            &host
                .ensure_session_runtime(&reference, &pin)
                .expect_err("uninstalled")
        ),
        "runtime_not_installed"
    );
}

#[test]
fn observation_follows_the_pinned_package_after_disable_or_a_newer_selection() {
    let plugins = Plugins::new();
    let registry = plugins.registry();
    let first = acme("1.0.0");
    install(&registry, &first, true, true);
    let host = plugins.host();
    let reference = RuntimeRef::from_wire("acme");
    let pin = pin_of(&first, "acme");
    let regions = |config: &crate::detect::DetectorConfig| {
        format!(
            "{:?}",
            config
                .manifest
                .as_ref()
                .expect("manifest")
                .required_regions()
        )
    };
    let expected = regions(&crate::detect::DetectorConfig::for_definition(
        &host.resolve_id(&runtime("acme")).expect("loaded"),
    ));

    registry.set_enabled(&first.digest, false).expect("disable");
    host.reload().expect("reload");

    let pinned = crate::detect::DetectorConfig::for_pinned(&host, &reference, &pin, None);
    assert_eq!(regions(&pinned), expected);
    let generic =
        crate::detect::DetectorConfig::for_pinned(&host, &reference, &LaunchPin::Unpinned, None);
    assert_ne!(regions(&generic), expected, "no pin, no package rules");
    // A tampered pinned package falls back to generic observation; launching
    // it is refused elsewhere.
    tamper(&plugins, &first.digest, &Tamper::Append);
    let degraded = crate::detect::DetectorConfig::for_pinned(&host, &reference, &pin, None);
    assert_eq!(regions(&degraded), regions(&generic));
}

#[test]
fn a_recovered_safe_text_runtime_keeps_its_input_safety_when_its_package_fails_verification() {
    let plugins = Plugins::new();
    let built = build(
        &descriptor("acme.agent", "1.0.0", "acme").replace(
            "text_policy = \"unrestricted\"",
            "text_policy = \"hermes_safe_text\"",
        ),
        "safe",
        identity("acme.agent", "1.0.0"),
    );
    install(&plugins.registry(), &built, true, true);
    let host = plugins.host();
    let reference = RuntimeRef::from_wire("acme");
    let pin = pin_of(&built, "acme");
    let launched = host
        .resolve_id(&runtime("acme"))
        .expect("loaded")
        .input_rules();
    assert!(launched.is_restricted());
    let stored = crate::store::StoredInputRules::from(launched);
    let check = |rules: crate::agent::InputRules| {
        (
            rules.validate_text("bad\u{1b}[31m").is_err(),
            rules
                .validate_activity(Some(protocol::AgentActivity::Blocked))
                .is_err(),
        )
    };

    // Verified: the pinned definition supplies the contract.
    let verified = crate::agent::recovered_input_rules(&host, &reference, &pin, stored);
    assert_eq!(check(verified), (true, true));

    // Verification fails: the contract persisted at launch still applies.
    tamper(&plugins, &built.digest, &Tamper::Append);
    host.definition_for_pin(&reference, &pin)
        .expect_err("the pinned package no longer verifies");
    let degraded = crate::agent::recovered_input_rules(&host, &reference, &pin, stored);
    assert_eq!(check(degraded), (true, true));
    assert_eq!(degraded.submit_delay, launched.submit_delay);

    // An unrestricted runtime stays unrestricted.
    let open = crate::store::StoredInputRules::from(crate::agent::InputRules::unrestricted(
        true,
        std::time::Duration::from_millis(5),
    ));
    assert!(!open.to_standalone_rules().is_restricted());
}

/// The fixture descriptor of runtime `acme` with an `[integration]` table.
fn integration_descriptor(integration: &str) -> String {
    format!(
        "{}\n[integration]\n{integration}\n",
        descriptor("acme.agent", "1.0.0", "acme")
    )
}

/// The rejection installing `descriptor` as an archive reports.
fn install_rejection(descriptor: &str) -> Option<PackageRejection> {
    let built = build(descriptor, "x", identity("acme.agent", "1.0.0"));
    super::package::definition_from_archive(&entries(descriptor, "x"), &built.digest).err()
}

#[test]
fn a_package_with_a_supported_handler_and_hook_schema_serves_its_schema() {
    let plugins = Plugins::new();
    let built = build(
        &integration_descriptor(
            "handler = \"claude-hook-v1\"\nhook_schema = \"identity-subagent-v1\"",
        ),
        "x",
        identity("acme.agent", "1.0.0"),
    );
    install(&plugins.registry(), &built, true, true);

    let definition = plugins
        .host()
        .resolve_id(&runtime("acme"))
        .expect("the package runtime is served");

    assert_eq!(
        definition.hook_schema().map(|schema| schema.id),
        Some("identity-subagent-v1")
    );
}

#[test]
fn installing_a_package_with_an_unknown_or_mismatched_schema_is_refused() {
    for (integration, field) in [
        (
            "handler = \"claude-hook-v1\"\nhook_schema = \"identity-v9\"",
            "integration.hook_schema",
        ),
        (
            "handler = \"claude-hook-v1\"\nhook_schema = \"identity-v1\"",
            "integration.hook_schema",
        ),
        (
            "handler = \"acme-hook-v1\"\nhook_schema = \"identity-v1\"",
            "integration.handler",
        ),
    ] {
        match install_rejection(&integration_descriptor(integration)) {
            Some(PackageRejection::Descriptor(super::DefinitionError::Field {
                field: refused,
                ..
            })) => assert_eq!(refused, field, "{integration}"),
            other => panic!("expected a refused descriptor for {integration}, got {other:?}"),
        }
    }
    assert!(install_rejection(&descriptor("acme.agent", "1.0.0", "acme")).is_none());
}

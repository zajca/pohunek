//! A package-provided, hook-less runtime for tests of the assigned native
//! reference strategy.
//!
//! The runtime is shaped like Pi: it starts with a caller-chosen id, resumes
//! with `--session <id>` and forks with `--fork <id>`, and its conversations
//! live as `<stamp>_<id>.jsonl` files below a config home.

use std::path::Path;
use std::sync::Arc;

use protocol::PackageDigest;

use super::builtin::BuiltinSource;
use super::definition::{DefinitionError, DefinitionOrigin, RuntimeDefinition};
use super::handle::RuntimeHost;
use super::registry::{RuntimeRegistry, RuntimeSource, SourceTrust};
use crate::detect::generic_shell_manifest;

/// Runtime id of the fixture.
pub(crate) const PI_SHAPED_ID: &str = "pi";

/// Environment variable that names the fixture runtime's config home.
pub(crate) const PI_SHAPED_HOME_ENV: &str = "PI_SHAPED_HOME";

/// Package id of the fixture.
pub(crate) const PI_SHAPED_PACKAGE_ID: &str = "acme.runtime.pi";

/// Archive digest [`pi_shaped_definition`] reports as its package digest.
pub(crate) const PACKAGE_DIGEST: &str =
    "sha256:2222222222222222222222222222222222222222222222222222222222222222";

/// The profile keys that bind a host profile to [`pi_shaped_host`]'s package.
pub(crate) fn pi_shaped_profile_pin() -> String {
    format!("package = \"{PI_SHAPED_PACKAGE_ID}\"\ndigest = \"{PACKAGE_DIGEST}\"\n")
}

/// `[native_reference.existence]` body that looks for the session file below
/// [`PI_SHAPED_HOME_ENV`].
pub(crate) const PI_SHAPED_FILE_CHECK: &str = r#"check = "file"
root_env = "PI_SHAPED_HOME"
dir = "sessions"
file_name = "_{reference}.jsonl"
name_match = "ends_with"
max_depth = 1"#;

/// Like [`PI_SHAPED_FILE_CHECK`], rooted at `XDG_CONFIG_HOME` with a
/// home-relative fallback.
pub(crate) const PI_SHAPED_XDG_CHECK: &str = r#"check = "file"
root_env = "XDG_CONFIG_HOME"
root_home = ".config/pi"
dir = "sessions"
file_name = "_{reference}.jsonl"
name_match = "ends_with"
max_depth = 1"#;

/// `[native_reference.existence]` body that skips verification.
pub(crate) const PI_SHAPED_NO_CHECK: &str = r#"check = "none""#;

/// The runtime document of the fixture: launches `program` with the fixed
/// arguments `--model fast`, and takes the initial prompt as a trailing
/// argument.
pub(crate) fn pi_shaped_document(program: &Path, existence: &str) -> String {
    format!(
        r#"schema = 1
id = "acme.runtime.pi"
version = "1.0.0"
runtime_api = 1

[runtime]
id = "{PI_SHAPED_ID}"
name = "Pi"
program = {program:?}
args = ["--model", "fast"]
detect_manifest = "any"
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
supported = true
args = ["--fork", "{{reference}}"]

[native_reference]
strategy = "assigned"
launch_args = ["--session-id", "{{reference}}"]

[native_reference.existence]
{existence}
"#
    )
}

/// Parses the fixture document into a package-origin definition.
///
/// # Panics
///
/// Panics when the document is invalid; tests pass documents they expect to
/// parse.
pub(crate) fn pi_shaped_definition(program: &Path, existence: &str) -> RuntimeDefinition {
    RuntimeDefinition::from_toml(
        &pi_shaped_document(program, existence),
        |package| DefinitionOrigin::Package {
            package,
            digest: PackageDigest::parse(PACKAGE_DIGEST).expect("valid digest"),
        },
        |_name| Ok(Arc::new(generic_shell_manifest().clone())),
    )
    .expect("the Pi-shaped fixture document is valid")
}

/// `version_probe` value of the data-driven probe, accepting `[1.0.0, 1.1.0)`.
pub(crate) const PI_SHAPED_PROBE: &str =
    r#"{ parser = "semver-v1", args = ["--version"], min = "1.0.0", below = "1.1.0" }"#;

/// Like [`pi_shaped_host`], with the fixture runtime declaring `probe` as its
/// `version_probe`.
///
/// # Panics
///
/// Panics when the document or the registry is invalid, which would be a
/// defect of the fixture.
pub(crate) fn pi_shaped_probed_host(program: &Path, existence: &str, probe: &str) -> RuntimeHost {
    let document = pi_shaped_document(program, existence).replace(
        "prompt_arg = true\n",
        &format!("prompt_arg = true\nversion_probe = {probe}\n"),
    );
    let definition = RuntimeDefinition::from_toml(
        &document,
        |package| DefinitionOrigin::Package {
            package,
            digest: PackageDigest::parse(PACKAGE_DIGEST).expect("valid digest"),
        },
        |_name| Ok(Arc::new(generic_shell_manifest().clone())),
    )
    .expect("the probed fixture document is valid");
    let builtin = BuiltinSource::new("/bin/sh");
    let fixture = FixtureSource(vec![definition]);
    let registry =
        RuntimeRegistry::from_sources(&[&builtin, &fixture]).expect("the fixture registry builds");
    RuntimeHost::new(registry)
}

#[derive(Debug)]
struct FixtureSource(Vec<RuntimeDefinition>);

impl RuntimeSource for FixtureSource {
    fn trust(&self) -> SourceTrust {
        SourceTrust::External
    }

    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
        Ok(self.0.clone())
    }
}

/// [`pi_shaped_host`] with the fixture runtime declaring an integration, so
/// its sessions admit hook reports.
///
/// # Panics
///
/// Panics when the registry cannot be built, which would be a defect of the
/// fixture.
pub(crate) fn pi_shaped_hooked_host(program: &Path, existence: &str) -> RuntimeHost {
    let document = with_integration(&pi_shaped_document(program, existence));
    let definition = RuntimeDefinition::from_toml(
        &document,
        |package| DefinitionOrigin::Package {
            package,
            digest: PackageDigest::parse(PACKAGE_DIGEST).expect("valid digest"),
        },
        |_name| Ok(Arc::new(generic_shell_manifest().clone())),
    )
    .expect("the hooked fixture document is valid");
    let builtin = BuiltinSource::new("/bin/sh");
    let fixture = FixtureSource(vec![definition]);
    let registry =
        RuntimeRegistry::from_sources(&[&builtin, &fixture]).expect("the fixture registry builds");
    RuntimeHost::new(registry)
}

/// A host serving the built-in runtimes plus the Pi-shaped fixture.
///
/// # Panics
///
/// Panics when the registry cannot be built, which would be a defect of the
/// fixture.
pub(crate) fn pi_shaped_host(program: &Path, existence: &str) -> RuntimeHost {
    let builtin = BuiltinSource::new("/bin/sh");
    let fixture = FixtureSource(vec![pi_shaped_definition(program, existence)]);
    let registry =
        RuntimeRegistry::from_sources(&[&builtin, &fixture]).expect("the fixture registry builds");
    RuntimeHost::new(registry)
}

/// Detection manifest shipped by [`installed_pi_host`] packages.
const PACKAGED_DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

/// Detection manifest of the second fixture version: the first manifest plus a
/// rule over another region, so the two versions are told apart by their
/// required regions.
pub(crate) const PACKAGED_DETECT_MANIFEST_V2: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]

[[rules]]
id = "title_blocked"
state = "blocked"
priority = 300
region = "osc_title"
any = [{ contains = "blocked" }]
"#;

/// Installs the Pi-shaped fixture at `version` with the detection manifest
/// `detect` as an enabled package of `plugins_dir`, selected when `select`,
/// and returns its archive digest.
///
/// # Panics
///
/// Panics when the fixture cannot be installed, which would be a defect of the
/// fixture.
pub(crate) fn install_pi_version(
    plugins_dir: &Path,
    program: &Path,
    version: &str,
    detect: &str,
    select: bool,
) -> PackageDigest {
    let document = pi_shaped_document(program, PI_SHAPED_NO_CHECK);
    install_document(plugins_dir, &document, version, detect, select)
}

/// Appends the integration of the hooked fixture variant: a handler and the
/// hook schema its reports follow.
fn with_integration(document: &str) -> String {
    format!(
        "{document}\n[integration]\nhandler = \"codex-hook-v1\"\nhook_schema = \"identity-subagent-v1\"\n"
    )
}

/// Installs `document` (a Pi-shaped descriptor) as a package of
/// `plugins_dir` and returns its archive digest.
fn install_document(
    plugins_dir: &Path,
    document: &str,
    version: &str,
    detect: &str,
    select: bool,
) -> PackageDigest {
    use package::registry::{InstallRequest, PackageSource as InstallSource, Registry};
    use package::{build_archive, read_archive, ArchiveEntry, Limits};

    let document = document
        .replace(
            "detect_manifest = \"any\"",
            "detect_manifest = \"detect.toml\"",
        )
        .replace("version = \"1.0.0\"", &format!("version = \"{version}\""));
    let entries = [
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: document.into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "detect.toml".to_owned(),
            contents: detect.as_bytes().to_vec(),
            executable: false,
        },
    ];
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the fixture archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the fixture archive reads")
        .digest()
        .clone();
    Registry::open_at(plugins_dir, Limits::DEFAULT)
        .expect("the plugin root opens")
        .install(&InstallRequest {
            archive: &bytes,
            expected: &digest,
            identity: protocol::PackageIdentity {
                id: protocol::PackageId::parse(PI_SHAPED_PACKAGE_ID).expect("package id"),
                version: protocol::PackageVersion::parse(version).expect("package version"),
            },
            source: InstallSource::ExplicitDigest,
            enabled: true,
            select,
            installed_at_unix_seconds: 1_700_000_000,
        })
        .expect("the fixture package installs");
    digest
}

/// Installs the Pi-shaped fixture at `version` with the default detection
/// manifest, selecting it when `select` is set, and returns its archive digest.
///
/// # Panics
///
/// Panics when the fixture cannot be installed, which would be a defect of
/// the fixture.
pub(crate) fn install_pi_package(
    plugins_dir: &Path,
    program: &Path,
    version: &str,
    select: bool,
) -> PackageDigest {
    install_pi_version(
        plugins_dir,
        program,
        version,
        PACKAGED_DETECT_MANIFEST,
        select,
    )
}

/// [`installed_pi_host`] for the fixture variant that declares an
/// integration, so sessions of the package admit hook reports.
///
/// # Panics
///
/// Panics when the fixture cannot be installed or loaded, which would be a
/// defect of the fixture.
pub(crate) fn installed_hooked_pi_host(
    plugins_dir: &Path,
    program: &Path,
) -> (RuntimeHost, PackageDigest) {
    use super::package::{PackageSource, PackageStore};

    let document = with_integration(&pi_shaped_document(program, PI_SHAPED_NO_CHECK));
    let digest = install_document(
        plugins_dir,
        &document,
        "1.0.0",
        PACKAGED_DETECT_MANIFEST,
        true,
    );
    let host = RuntimeHost::with_packages(
        BuiltinSource::new("/bin/sh"),
        PackageSource::new(PackageStore::open(plugins_dir).expect("the store opens")),
    )
    .expect("the host builds");
    (host, digest)
}

/// Installs the Pi-shaped fixture as an enabled, selected package into the
/// plugin root `plugins_dir` and returns a host that serves it with its
/// archive digest.
///
/// # Panics
///
/// Panics when the fixture cannot be installed or loaded, which would be a
/// defect of the fixture.
pub(crate) fn installed_pi_host(
    plugins_dir: &Path,
    program: &Path,
) -> (RuntimeHost, PackageDigest) {
    use super::package::{PackageSource, PackageStore};

    let digest = install_pi_version(
        plugins_dir,
        program,
        "1.0.0",
        PACKAGED_DETECT_MANIFEST,
        true,
    );
    let host = RuntimeHost::with_packages(
        BuiltinSource::new("/bin/sh"),
        PackageSource::new(PackageStore::open(plugins_dir).expect("the store opens")),
    )
    .expect("the host builds");
    (host, digest)
}

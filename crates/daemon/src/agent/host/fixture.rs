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

const PACKAGE_DIGEST: &str =
    "sha256:2222222222222222222222222222222222222222222222222222222222222222";

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
    use package::registry::{InstallRequest, PackageSource as InstallSource, Registry};
    use package::{build_archive, read_archive, ArchiveEntry, Limits};

    use super::package::{PackageSource, PackageStore};

    let document = pi_shaped_document(program, PI_SHAPED_NO_CHECK).replace(
        "detect_manifest = \"any\"",
        "detect_manifest = \"detect.toml\"",
    );
    let entries = [
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: document.into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "detect.toml".to_owned(),
            contents: PACKAGED_DETECT_MANIFEST.as_bytes().to_vec(),
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
                id: protocol::PackageId::parse("acme.runtime.pi").expect("package id"),
                version: protocol::PackageVersion::parse("1.0.0").expect("package version"),
            },
            source: InstallSource::ExplicitDigest,
            enabled: true,
            select: true,
            installed_at_unix_seconds: 1_700_000_000,
        })
        .expect("the fixture package installs");
    let host = RuntimeHost::with_packages(
        BuiltinSource::new("/bin/sh"),
        PackageSource::new(PackageStore::open(plugins_dir).expect("the store opens")),
    )
    .expect("the host builds");
    (host, digest)
}

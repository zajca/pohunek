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

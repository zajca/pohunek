//! Runtime fixtures for in-crate tests.
//!
//! [`official_host`] serves the official Codex, Claude and Hermes packages
//! from `runtime-packages/`, so tests of those runtimes exercise the package
//! descriptors rather than definitions compiled into the daemon;
//! [`install_official_packages`] installs the same packages into a plugin
//! root for hosts with a package store.
//!
//! The Pi-shaped runtime is a package-provided runtime for tests of the
//! assigned native reference strategy; it is hook-less unless a test declares
//! an integration for it. It starts with a caller-chosen id, resumes with
//! `--session <id>` and forks with `--fork <id>`, and its conversations live as
//! `<stamp>_<id>.jsonl` files below a config home.

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

/// Package digest [`OfficialFixtureSource`] reports for the official Codex
/// package.
///
/// It names no real archive: a host built from the fixture has no package
/// store, so nothing verifies a package root against it. Profiles pin it
/// through [`official_profile_pin`].
pub(crate) const OFFICIAL_CODEX_DIGEST: &str =
    "sha256:3333333333333333333333333333333333333333333333333333333333333333";

/// Package digest [`OfficialFixtureSource`] reports for the official Claude
/// package; see [`OFFICIAL_CODEX_DIGEST`].
pub(crate) const OFFICIAL_CLAUDE_DIGEST: &str =
    "sha256:4444444444444444444444444444444444444444444444444444444444444444";

/// Package digest [`OfficialFixtureSource`] reports for the official Hermes
/// package; see [`OFFICIAL_CODEX_DIGEST`].
pub(crate) const OFFICIAL_HERMES_DIGEST: &str =
    "sha256:6666666666666666666666666666666666666666666666666666666666666666";

/// One official runtime package as `runtime-packages/` ships it.
struct OfficialPackage {
    /// Runtime id the descriptor declares.
    runtime: &'static str,
    /// Package id the descriptor declares.
    package: &'static str,
    /// The `runtime.toml` of the package.
    descriptor: &'static str,
    /// The `detect.toml` of the package.
    detect: &'static str,
    /// Digest the fixture reports for the package.
    digest: &'static str,
    /// Whether the fixture keeps the package's `version_probe`. In-crate
    /// Hermes stand-ins answer `--version` with a banner the probe reads, so
    /// launch-policy scenarios exercise the package's probe; Codex and Claude
    /// stand-ins do not answer it.
    keeps_probe: bool,
}

/// The official Codex, Claude and Hermes packages, read from the workspace's
/// `runtime-packages/` tree at build time.
const OFFICIAL_PACKAGES: [OfficialPackage; 3] = [
    OfficialPackage {
        runtime: "codex",
        package: "pohunek.runtime.codex",
        descriptor: include_str!("../../../../../runtime-packages/codex/runtime.toml"),
        detect: include_str!("../../../../../runtime-packages/codex/detect.toml"),
        digest: OFFICIAL_CODEX_DIGEST,
        keeps_probe: false,
    },
    OfficialPackage {
        runtime: "claude",
        package: "pohunek.runtime.claude",
        descriptor: include_str!("../../../../../runtime-packages/claude/runtime.toml"),
        detect: include_str!("../../../../../runtime-packages/claude/detect.toml"),
        digest: OFFICIAL_CLAUDE_DIGEST,
        keeps_probe: false,
    },
    OfficialPackage {
        runtime: "hermes",
        package: "pohunek.runtime.hermes",
        descriptor: include_str!("../../../../../runtime-packages/hermes/runtime.toml"),
        detect: include_str!("../../../../../runtime-packages/hermes/detect.toml"),
        digest: OFFICIAL_HERMES_DIGEST,
        keeps_probe: true,
    },
];

/// Install time the fixture records for every package it installs; a fixed
/// value keeps the registry records independent of the test host's clock.
const INSTALLED_AT_UNIX_SECONDS: u64 = 1_700_000_000;

/// Key of the descriptor line [`OfficialFixtureSource`] drops.
const VERSION_PROBE_KEY: &str = "version_probe =";

/// The official Codex, Claude and Hermes packages as a catalog-authorized
/// source.
///
/// Each definition is parsed by [`super::definition_from_archive`] from the
/// package's own `runtime.toml` and `detect.toml`, so it carries package
/// provenance and the package's detection manifest. The Codex and Claude
/// `version_probe` lines are dropped: the probe would run the real
/// `codex`/`claude` binary, while in-crate tests launch stand-in programs that
/// do not answer `--version`. The Hermes package keeps its probe (see
/// [`OfficialPackage::keeps_probe`]); out-of-process tests keep every probe
/// and use fakes that answer `--version`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OfficialFixtureSource;

impl RuntimeSource for OfficialFixtureSource {
    fn trust(&self) -> SourceTrust {
        SourceTrust::Official
    }

    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
        Ok(OFFICIAL_PACKAGES.iter().map(official_definition).collect())
    }
}

/// The `runtime.toml` of `official` with its version probe dropped.
///
/// # Panics
///
/// Panics unless the descriptor declares exactly one `version_probe` line,
/// which would be a defect of the fixture or of the package.
fn probeless_descriptor(official: &OfficialPackage) -> String {
    let (probes, kept): (Vec<&str>, Vec<&str>) = official
        .descriptor
        .lines()
        .partition(|line| line.trim_start().starts_with(VERSION_PROBE_KEY));
    assert_eq!(
        probes.len(),
        1,
        "the {} package declares exactly one version_probe line",
        official.runtime
    );
    kept.join("\n")
}

/// The archive entries of `official`, with its version probe dropped unless
/// the fixture keeps it ([`OfficialPackage::keeps_probe`]).
///
/// # Panics
///
/// Panics unless a descriptor whose probe is dropped declares exactly one
/// `version_probe` line, which would be a defect of the fixture or of the
/// package.
fn official_entries(official: &OfficialPackage) -> [package::ArchiveEntry; 2] {
    let descriptor = if official.keeps_probe {
        official.descriptor.to_owned()
    } else {
        probeless_descriptor(official)
    };
    [
        package::ArchiveEntry {
            path: super::RUNTIME_DESCRIPTOR_PATH.to_owned(),
            contents: descriptor.into_bytes(),
            executable: false,
        },
        package::ArchiveEntry {
            path: "detect.toml".to_owned(),
            contents: official.detect.as_bytes().to_vec(),
            executable: false,
        },
    ]
}

/// The archive entries of the official package of `runtime` as
/// [`OfficialFixtureSource`] serves it: the package's own descriptor and
/// detection manifest, with the version probe dropped for Codex and Claude.
///
/// # Panics
///
/// Panics when `runtime` is not `codex`, `claude` or `hermes`.
pub(crate) fn official_package_entries(runtime: &str) -> [package::ArchiveEntry; 2] {
    let official = OFFICIAL_PACKAGES
        .iter()
        .find(|official| official.runtime == runtime)
        .unwrap_or_else(|| panic!("{runtime} is not an official fixture runtime"));
    official_entries(official)
}

/// Parses `official` with its version probe dropped unless the fixture keeps
/// it.
///
/// # Panics
///
/// Panics unless a descriptor whose probe is dropped declares exactly one
/// `version_probe` line, or when the package does not parse, which would be a
/// defect of the fixture or of the package.
fn official_definition(official: &OfficialPackage) -> RuntimeDefinition {
    let digest = PackageDigest::parse(official.digest).expect("valid digest");
    super::definition_from_archive(&official_entries(official), &digest).unwrap_or_else(|error| {
        panic!(
            "the official {} package parses: {error:?}",
            official.runtime
        )
    })
}

/// Installs the official Codex, Claude and Hermes packages, with the Codex and
/// Claude version probes dropped and the Hermes probe kept as
/// [`OfficialFixtureSource`] does, into the plugin root
/// `plugins_dir` as enabled, selected packages recorded with official
/// provenance, as a catalog-authorized install records them (no catalog is
/// checked), so a host over the root serves `codex`, `claude` and `hermes`
/// from them.
///
/// # Panics
///
/// Panics when a package cannot be built or installed, which would be a
/// defect of the fixture.
pub(crate) fn install_official_packages(plugins_dir: &Path) {
    use package::registry::{InstallRequest, PackageSource as InstallSource, Registry};
    use package::{build_archive, read_archive, Limits};

    let registry = Registry::open_at(plugins_dir, Limits::DEFAULT).expect("the plugin root opens");
    for official in &OFFICIAL_PACKAGES {
        let entries = official_entries(official);
        let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the official archive builds");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the official archive reads")
            .digest()
            .clone();
        let protocol::BindingProvenance::Package { package, .. } =
            super::definition_from_archive(&entries, &digest)
                .expect("the official package parses")
                .binding()
                .provenance
                .clone()
        else {
            panic!("the {} package has package provenance", official.runtime);
        };
        registry
            .install(&InstallRequest {
                archive: &bytes,
                expected: &digest,
                identity: package,
                source: InstallSource::Official,
                enabled: true,
                select: true,
                installed_at_unix_seconds: INSTALLED_AT_UNIX_SECONDS,
            })
            .expect("the official package installs");
    }
}

/// A host serving the built-in shell, with `/bin/sh` as the shell, and the
/// official Codex, Claude and Hermes packages through
/// [`OfficialFixtureSource`].
///
/// # Panics
///
/// Panics when the registry cannot be built, which would be a defect of the
/// fixture.
pub(crate) fn official_host() -> RuntimeHost {
    official_host_over(&BuiltinSource::new("/bin/sh"))
}

/// Like [`official_host`], with the built-in runtimes of `builtin`.
///
/// # Panics
///
/// Panics when the registry cannot be built, which would be a defect of the
/// fixture.
pub(crate) fn official_host_over(builtin: &BuiltinSource) -> RuntimeHost {
    let registry = RuntimeRegistry::from_sources(&[builtin, &OfficialFixtureSource])
        .expect("the official fixture registry builds");
    RuntimeHost::new(registry)
}

/// The launch pin of a session launched from the official package of
/// `runtime` in [`official_host`].
///
/// # Panics
///
/// Panics when `runtime` is not `codex`, `claude` or `hermes`.
pub(crate) fn official_launch_pin(runtime: &str) -> super::LaunchPin {
    let runtime_id = protocol::RuntimeId::parse(runtime).expect("runtime id");
    let definition = official_host()
        .resolve_id(&runtime_id)
        .unwrap_or_else(|error| panic!("{runtime} is served: {error:?}"));
    assert!(
        matches!(
            definition.binding().provenance,
            protocol::BindingProvenance::Package { .. }
        ),
        "{runtime} is an official fixture package"
    );
    super::LaunchPin::of(&definition)
}

/// A built-in Codex, Claude or Hermes runtime release v0.34.2 compiled in,
/// frozen as goldens below `compat/<runtime>/builtin-v0.34/`.
struct CompiledInGolden {
    /// Runtime id, which is also the name the descriptor gives its embedded
    /// detection manifest.
    runtime: &'static str,
    /// The golden `runtime.toml`.
    descriptor: &'static str,
    /// The golden detection manifest.
    detect: &'static str,
}

/// The v0.34.2 built-in Codex, Claude and Hermes goldens.
const COMPILED_IN_GOLDENS: [CompiledInGolden; 3] = [
    CompiledInGolden {
        runtime: "codex",
        descriptor: include_str!("../../../../../compat/codex/builtin-v0.34/runtime.toml"),
        detect: include_str!("../../../../../compat/codex/builtin-v0.34/detect.toml"),
    },
    CompiledInGolden {
        runtime: "claude",
        descriptor: include_str!("../../../../../compat/claude/builtin-v0.34/runtime.toml"),
        detect: include_str!("../../../../../compat/claude/builtin-v0.34/detect.toml"),
    },
    CompiledInGolden {
        runtime: "hermes",
        descriptor: include_str!("../../../../../compat/hermes/builtin-v0.34/runtime.toml"),
        detect: include_str!("../../../../../compat/hermes/builtin-v0.34/detect.toml"),
    },
];

/// Start of the first golden header line, followed by the source path.
const GOLDEN_HEADER_SOURCE: &str = "# Golden copy of ";

/// End of the first golden header line: the release the goldens froze.
const GOLDEN_HEADER_TAG: &str = " at tag v0.34.2.";

/// Second line of every golden header.
const GOLDEN_HEADER_NOTICE: &str =
    "# Do not edit: the parity tests compare the runtime package with these bytes.";

/// `golden` without its three-line header: the source line, the notice and
/// the blank line after them.
///
/// # Panics
///
/// Panics when the golden does not start with that header, which would be a
/// defect of the golden.
fn golden_body(golden: &str) -> &str {
    let mut lines = golden.splitn(4, '\n');
    let source = lines.next().unwrap_or_default();
    let notice = lines.next().unwrap_or_default();
    let blank = lines.next().unwrap_or_default();
    assert!(
        source.starts_with(GOLDEN_HEADER_SOURCE)
            && source.ends_with(GOLDEN_HEADER_TAG)
            && notice == GOLDEN_HEADER_NOTICE
            && blank.is_empty(),
        "a golden starts with its three-line header"
    );
    lines.next().unwrap_or_default()
}

/// Built-in runtimes as a daemon that compiles Codex, Claude and Hermes in
/// serves them: the shell of the wrapped [`BuiltinSource`], and the Codex,
/// Claude and Hermes built-ins of release v0.34.2, parsed from their goldens
/// ([`COMPILED_IN_GOLDENS`]) the way that daemon parsed its embedded
/// descriptors.
///
/// A record such a daemon wrote (an unpinned or a built-in-pinned launch
/// binding) resumes only through a built-in of the same runtime, so scenarios
/// over those records serve one. The goldens keep it the v0.34.2 built-in
/// whatever the runtime packages or the daemon's own sources declare.
///
/// The Hermes golden names the `hermes-v1` version-probe parser, which this
/// daemon does not compile in, so its `version_probe` line is dropped (see
/// [`compiled_in_descriptor`]) and the previous release's launch can be
/// replayed. The probe is not part of the launch binding, so a session
/// launched here records the binding release v0.34.2 recorded.
#[derive(Debug)]
pub(crate) struct CompiledInSource(pub(crate) BuiltinSource);

impl RuntimeSource for CompiledInSource {
    fn trust(&self) -> SourceTrust {
        SourceTrust::Builtin
    }

    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
        let mut definitions = self.0.load()?;
        for golden in &COMPILED_IN_GOLDENS {
            definitions.push(RuntimeDefinition::from_toml(
                &compiled_in_descriptor(golden),
                |package| DefinitionOrigin::Builtin {
                    package: Some(package),
                },
                |name| {
                    if name != golden.runtime {
                        return Err(DefinitionError::UnknownManifest);
                    }
                    let manifest = crate::detect::Manifest::parse_str(golden_body(golden.detect))
                        .unwrap_or_else(|error| {
                            panic!("the {} golden manifest parses: {error:?}", golden.runtime)
                        });
                    Ok(Arc::new(manifest))
                },
            )?);
        }
        Ok(definitions)
    }
}

/// The golden descriptor of `golden` without its header and without a
/// `version_probe` line.
fn compiled_in_descriptor(golden: &CompiledInGolden) -> String {
    golden_body(golden.descriptor)
        .lines()
        .filter(|line| !line.trim_start().starts_with(VERSION_PROBE_KEY))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A host serving Codex, Claude and Hermes as built-in runtimes through
/// [`CompiledInSource`], with `/bin/sh` as the shell.
///
/// # Panics
///
/// Panics when the registry cannot be built, which would be a defect of the
/// fixture.
pub(crate) fn compiled_in_host() -> RuntimeHost {
    RuntimeHost::new(compiled_in_registry("/bin/sh"))
}

/// The registry of [`compiled_in_host`], with `shell` as the shell.
///
/// # Panics
///
/// Panics when the registry cannot be built, which would be a defect of the
/// fixture.
pub(crate) fn compiled_in_registry(shell: &str) -> RuntimeRegistry {
    RuntimeRegistry::from_sources(&[&CompiledInSource(BuiltinSource::new(shell))])
        .expect("the compiled-in registry builds")
}

/// The profile keys that bind a host profile over `runtime` to its package in
/// [`official_host`].
///
/// # Panics
///
/// Panics when `runtime` is not `codex`, `claude` or `hermes`.
pub(crate) fn official_profile_pin(runtime: &str) -> String {
    let official = OFFICIAL_PACKAGES
        .iter()
        .find(|official| official.runtime == runtime)
        .unwrap_or_else(|| panic!("{runtime} is an official fixture package"));
    format!(
        "package = \"{}\"\ndigest = \"{}\"\n",
        official.package, official.digest
    )
}

/// The detection manifest the official package of `runtime` ships, parsed
/// from its `detect.toml`.
///
/// # Panics
///
/// Panics when `runtime` is not `codex`, `claude` or `hermes`, or when the
/// package manifest does not parse.
pub(crate) fn official_manifest(runtime: &str) -> crate::detect::Manifest {
    let official = OFFICIAL_PACKAGES
        .iter()
        .find(|official| official.runtime == runtime)
        .unwrap_or_else(|| panic!("{runtime} is an official fixture package"));
    crate::detect::Manifest::parse_str(official.detect)
        .unwrap_or_else(|error| panic!("the {runtime} package manifest parses: {error:?}"))
}

/// `body` with the [`official_profile_pin`] of its base inserted after its
/// first line when that line is `base = "codex"`, `base = "claude"` or
/// `base = "hermes"`, so the
/// profile binds the package [`official_host`] serves its base from; any
/// other profile is returned unchanged.
pub(crate) fn pin_official_base(body: &str) -> String {
    for official in &OFFICIAL_PACKAGES {
        let first = format!("base = \"{}\"\n", official.runtime);
        if let Some(rest) = body.strip_prefix(&first) {
            return format!("{first}{}{rest}", official_profile_pin(official.runtime));
        }
    }
    body.to_owned()
}

/// A host serving the built-in runtimes, the official fixture packages of
/// [`official_host`] and the Pi-shaped fixture declaring the integration
/// `handler` driving the hook schema `hook_schema`.
///
/// # Panics
///
/// Panics when the document or the registry is invalid, which would be a
/// defect of the fixture.
pub(crate) fn pi_shaped_integration_host(
    program: &Path,
    handler: &str,
    hook_schema: &str,
) -> RuntimeHost {
    pi_shaped_integration_host_as(program, PI_SHAPED_ID, handler, hook_schema)
}

/// Like [`pi_shaped_integration_host`], with the fixture runtime named
/// `runtime_id`.
///
/// # Panics
///
/// Panics when the document or the registry is invalid, which would be a
/// defect of the fixture.
pub(crate) fn pi_shaped_integration_host_as(
    program: &Path,
    runtime_id: &str,
    handler: &str,
    hook_schema: &str,
) -> RuntimeHost {
    let document = format!(
        "{}\n[integration]\nhandler = \"{handler}\"\nhook_schema = \"{hook_schema}\"\n{}",
        pi_shaped_document(program, PI_SHAPED_NO_CHECK).replace(
            &format!("id = \"{PI_SHAPED_ID}\""),
            &format!("id = \"{runtime_id}\"")
        ),
        config_home_table(handler),
    );
    let definition = RuntimeDefinition::from_toml(
        &document,
        |package| DefinitionOrigin::Package {
            package,
            digest: PackageDigest::parse(PACKAGE_DIGEST).expect("valid digest"),
        },
        |_name| Ok(Arc::new(generic_shell_manifest().clone())),
    )
    .expect("the integration fixture document is valid");
    let builtin = BuiltinSource::new("/bin/sh");
    let fixture = FixtureSource(vec![definition]);
    let registry = RuntimeRegistry::from_sources(&[&builtin, &OfficialFixtureSource, &fixture])
        .expect("the fixture registry builds");
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
/// its sessions admit hook reports, next to the official fixture packages of
/// [`official_host`], whose runtimes those reports may name.
///
/// # Panics
///
/// Panics when the registry cannot be built, which would be a defect of the
/// fixture.
pub(crate) fn pi_shaped_hooked_host(program: &Path, existence: &str) -> RuntimeHost {
    pi_shaped_hooked_host_as(program, existence, PI_SHAPED_ID)
}

/// Like [`pi_shaped_hooked_host`], with the fixture runtime named
/// `runtime_id`.
///
/// # Panics
///
/// Panics when the registry cannot be built, which would be a defect of the
/// fixture.
pub(crate) fn pi_shaped_hooked_host_as(
    program: &Path,
    existence: &str,
    runtime_id: &str,
) -> RuntimeHost {
    let document = with_integration(&pi_shaped_document(program, existence).replace(
        &format!("id = \"{PI_SHAPED_ID}\""),
        &format!("id = \"{runtime_id}\""),
    ));
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
    let registry = RuntimeRegistry::from_sources(&[&builtin, &OfficialFixtureSource, &fixture])
        .expect("the fixture registry builds");
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

/// Like [`pi_shaped_host`], with the fixture runtime also assigning every fork
/// a reference of its own (`assign_fork = true`), so a fork launches as
/// `--fork <source> --session-id <fork>`.
///
/// # Panics
///
/// Panics when the document or the registry is invalid, which would be a
/// defect of the fixture.
pub(crate) fn pi_shaped_forking_host(program: &Path, existence: &str) -> RuntimeHost {
    let launch_args = "launch_args = [\"--session-id\", \"{reference}\"]\n";
    let document = pi_shaped_document(program, existence)
        .replace(launch_args, &format!("{launch_args}assign_fork = true\n"));
    let definition = RuntimeDefinition::from_toml(
        &document,
        |package| DefinitionOrigin::Package {
            package,
            digest: PackageDigest::parse(PACKAGE_DIGEST).expect("valid digest"),
        },
        |_name| Ok(Arc::new(generic_shell_manifest().clone())),
    )
    .expect("the forking fixture document is valid");
    assert!(
        definition
            .native()
            .and_then(crate::agent::NativeSessionLaunch::assigned)
            .is_some_and(crate::agent::AssignedReference::assigns_fork),
        "the forking fixture assigns fork references"
    );
    let builtin = BuiltinSource::new("/bin/sh");
    let fixture = FixtureSource(vec![definition]);
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
        "{document}\n[integration]\nhandler = \"codex-hook-v1\"\nhook_schema = \"identity-subagent-v1\"\n{}",
        config_home_table("codex-hook-v1"),
    )
}

/// The `[config_home]` table a fixture runtime driven by `handler` declares so
/// its integration has a directory to act on; empty for a handler whose
/// lifecycle the daemon does not run.
pub(crate) fn config_home_table(handler: &str) -> &'static str {
    match handler {
        "codex-hook-v1" => "\n[config_home]\nenv = \"CODEX_HOME\"\ndefault = \".codex\"\n",
        "claude-hook-v1" => "\n[config_home]\nenv = \"CLAUDE_CONFIG_DIR\"\ndefault = \".claude\"\n",
        _ => "",
    }
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
            installed_at_unix_seconds: INSTALLED_AT_UNIX_SECONDS,
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

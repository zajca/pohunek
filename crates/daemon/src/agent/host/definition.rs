//! Validated, immutable agent runtime definitions.
//!
//! A [`RuntimeDefinition`] is everything the daemon needs to launch, frame
//! input for, resume, fork and observe one agent runtime, held as data. It is
//! built either from the `runtime.toml` shape ([`RuntimeDefinition::from_toml`])
//! or from explicit [`DefinitionParts`], and both paths run the same
//! validation. The `[resume]` and `[fork]` tables share the typed
//! [`NativeSessionLaunch`] template rules with host profiles, so a reference
//! slot is always one whole argv token.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pohunek_worker_protocol::HookSchema;
use protocol::{
    BindingFieldError, BindingProvenance, DescriptorDigest, LaunchBinding, PackageDigest,
    PackageId, PackageIdentity, PackageVersion, RuntimeId, RuntimeIdError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::config_home::ConfigHome;
use super::features::{check_gates, check_requires, FeatureTables};
use super::launch_env::DescriptorLaunchEnv;
use super::naming::{NamingLaunchArgs, SESSION_NAME_PLACEHOLDER};
use super::transcripts::Transcripts;
use crate::agent::{
    AssignedReference, ExistenceDecl, InputRules, InputTextPolicy, NativeArgs, NativeLaunchError,
    NativeReferenceError, NativeReferenceStrategy, NativeSessionLaunch, ReferenceExistence,
    SessionRefKind,
};
use crate::detect::Manifest;
use package::version_probe::{
    VersionProbeDeclaration, VersionProbeError, VersionProbePolicy, SEMVER_LINE_PARSER_ID,
    SEMVER_PARSER_ID, SEMVER_REVISION_LINE_PARSER_ID,
};

/// `schema` value this build accepts in a runtime definition.
pub const SUPPORTED_SCHEMA: u32 = 1;

/// `runtime_api` value this build accepts in a runtime definition.
pub const SUPPORTED_RUNTIME_API: u32 = 1;

/// Maximum accepted size of one runtime definition document, in bytes.
///
/// Bounds parser work for a definition that arrives from outside the binary.
/// Built-in definitions are far smaller.
pub const MAX_DEFINITION_BYTES: usize = 64 * 1024;

/// Maximum number of fixed launch arguments.
pub const MAX_LAUNCH_ARGS: usize = 64;

/// Maximum size of one launch argument, program, or template token, in bytes.
pub const MAX_ARG_BYTES: usize = 1024;

/// Maximum size of a display name or opaque handler id, in bytes.
pub const MAX_LABEL_BYTES: usize = 64;

/// Nanoseconds in one millisecond.
const NANOS_PER_MILLI: u32 = 1_000_000;

/// Domain tag mixed into every descriptor digest.
///
/// Changing the set of hashed fields requires a new tag so digests produced by
/// different field sets can never collide.
const DESCRIPTOR_DIGEST_DOMAIN: &str = "pohunek.runtime-descriptor.v1";

/// Why a runtime definition is invalid.
///
/// Messages name the offending field, never its value, so a diagnostic cannot
/// echo argument text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DefinitionError {
    /// The document is not valid TOML or does not match the schema. The
    /// diagnostic never reflects document text; `line` is the one-based line of
    /// the first problem when the parser reports it.
    #[error("runtime definition is malformed{}", line.map_or_else(String::new, |line| format!(" at line {line}")))]
    Malformed {
        /// One-based line of the first problem, if known.
        line: Option<usize>,
    },
    /// The document exceeds [`MAX_DEFINITION_BYTES`].
    #[error("runtime definition exceeds {MAX_DEFINITION_BYTES} bytes")]
    TooLarge,
    /// `schema` is not [`SUPPORTED_SCHEMA`].
    #[error("unsupported runtime definition schema {found}")]
    UnsupportedSchema {
        /// The declared schema version.
        found: u32,
    },
    /// `runtime_api` is not [`SUPPORTED_RUNTIME_API`].
    #[error("unsupported runtime_api {found}")]
    UnsupportedRuntimeApi {
        /// The declared runtime API version.
        found: u32,
    },
    /// An identifier field is invalid.
    #[error("{field}: {source}")]
    Identifier {
        /// Field that failed.
        field: &'static str,
        /// Underlying rule violation.
        source: RuntimeIdError,
    },
    /// A version or digest field is invalid.
    #[error("{field}: {source}")]
    Binding {
        /// Field that failed.
        field: &'static str,
        /// Underlying rule violation.
        source: BindingFieldError,
    },
    /// Runtime id, program kind and origin contradict each other.
    #[error("{0}")]
    Invariant(DefinitionInvariant),
    /// A resume or fork template is invalid.
    #[error("{field}: {source}")]
    Native {
        /// Field that failed.
        field: &'static str,
        /// Underlying rule violation.
        source: NativeLaunchError,
    },
    /// The `[native_reference]` declaration is invalid.
    #[error("{0}")]
    NativeReference(NativeReferenceError),
    /// A field violates a size, character or consistency rule.
    #[error("{field}: {reason}")]
    Field {
        /// Field that failed.
        field: &'static str,
        /// Fixed description of the violated rule.
        reason: &'static str,
    },
    /// `detect_manifest` names a manifest the source cannot provide.
    #[error("detect_manifest does not name an available detection manifest")]
    UnknownManifest,
    /// `requires` names a runtime feature this core does not implement.
    #[error("runtime feature {feature} is not supported by core {core}")]
    FeatureUnsupported {
        /// The required feature id.
        feature: String,
        /// Version of this core.
        core: &'static str,
    },
    /// A descriptor key or compiled id belongs to a runtime feature that
    /// `requires` does not list.
    #[error("{key} needs runtime feature {feature} in requires")]
    FeatureNotRequired {
        /// Dotted path of the descriptor key.
        key: &'static str,
        /// The feature that owns the key.
        feature: &'static str,
    },
}

impl From<VersionProbeError> for DefinitionError {
    fn from(error: VersionProbeError) -> Self {
        Self::Field {
            field: error.field,
            reason: error.reason,
        }
    }
}

/// A cross-field rule a runtime definition violates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DefinitionInvariant {
    /// The host login shell is only valid for the shell runtime.
    #[error("only the shell runtime may launch the host login shell")]
    HostShellOnlyForShell,
    /// The shell runtime must launch the host login shell.
    #[error("the shell runtime must launch the host login shell")]
    ShellRequiresHostShell,
    /// The shell runtime has no package identity and is built in.
    #[error("the shell runtime must be built in and carry no package identity")]
    ShellIsPackageless,
    /// A non-shell built-in runtime must name the package it stands for.
    #[error("a built-in runtime other than the shell requires a package identity")]
    BuiltinRequiresPackage,
    /// Only the shell runtime may declare a hook schema without an
    /// integration handler.
    #[error("only the shell runtime may declare a hook schema without an integration handler")]
    HookSchemaRequiresHandler,
}

/// The program a definition launches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchProgram {
    /// A fixed program name or absolute path declared by the definition.
    Fixed(String),
    /// The host's login shell, resolved when the registry was built. It is
    /// host state, not part of the descriptor, so it is excluded from the
    /// descriptor digest.
    HostShell(String),
}

impl LaunchProgram {
    /// The program to resolve at launch.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Fixed(program) | Self::HostShell(program) => program,
        }
    }
}

/// Where a definition comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionOrigin {
    /// Compiled into the daemon. The shell has no package identity.
    Builtin {
        /// Package the built-in descriptor stands for, if any.
        package: Option<PackageIdentity>,
    },
    /// Installed from an authenticated package archive.
    Package {
        /// Package identity.
        package: PackageIdentity,
        /// Digest of the installed archive.
        digest: PackageDigest,
    },
}

/// An opaque, validated id for a compiled parser or integration handler.
///
/// The definition only names the compiled component; the daemon owns what it
/// does.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HandlerId(String);

impl HandlerId {
    /// Validates a handler id: 1..=[`MAX_LABEL_BYTES`] bytes of lowercase
    /// ASCII alphanumerics, `-` and `_`.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] naming `field` when the rule is
    /// violated.
    pub fn parse(value: &str, field: &'static str) -> Result<Self, DefinitionError> {
        let valid = !value.is_empty()
            && value.len() <= MAX_LABEL_BYTES
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte)
            });
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(DefinitionError::Field {
                field,
                reason: "must be 1..=64 bytes of [a-z0-9_-]",
            })
        }
    }

    /// The validated id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A runtime's lifecycle-hook integration: the compiled handler that owns its
/// upstream assets, if any, and the core-owned hook schema its reports follow.
///
/// A definition only names both; the schema contents live in the compiled
/// registry ([`pohunek_worker_protocol::hook_schema`]).
#[derive(Debug, Clone)]
pub struct Integration {
    handler: Option<HandlerId>,
    hook_schema: &'static HookSchema,
}

impl Integration {
    /// Resolves `hook_schema` against the compiled registry and checks that
    /// `handler`, when present, is a compiled handler that drives it.
    ///
    /// A runtime without a handler (the shell) names a schema directly.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] when the schema id is not in the
    /// registry, the handler is not a compiled handler, or the handler does
    /// not support the schema.
    pub fn new(handler: Option<HandlerId>, hook_schema: &str) -> Result<Self, DefinitionError> {
        let hook_schema =
            pohunek_worker_protocol::hook_schema(hook_schema).ok_or(DefinitionError::Field {
                field: "integration.hook_schema",
                reason: "is not a hook schema this build provides",
            })?;
        if let Some(handler) = &handler {
            if !pohunek_worker_protocol::is_known_hook_handler(handler.as_str()) {
                return Err(DefinitionError::Field {
                    field: "integration.handler",
                    reason: "is not an integration handler this build provides",
                });
            }
            if !hook_schema.supports_handler(handler.as_str()) {
                return Err(DefinitionError::Field {
                    field: "integration.hook_schema",
                    reason: "is not supported by the integration handler",
                });
            }
        }
        Ok(Self {
            handler,
            hook_schema,
        })
    }

    /// The compiled integration handler id, if the runtime has one.
    #[must_use]
    pub fn handler(&self) -> Option<&HandlerId> {
        self.handler.as_ref()
    }

    /// The hook schema the runtime's reports follow.
    #[must_use]
    pub fn hook_schema(&self) -> &'static HookSchema {
        self.hook_schema
    }
}

/// When the daemon may write a launch's initial prompt into the agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputReadiness {
    /// Bracketed paste is enabled on screen, or the manifest reports idle from
    /// screen evidence. Silent agents fall back to the startup grace.
    #[default]
    BracketedPaste,
    /// Only the manifest's screen-sourced idle evidence counts. For an agent
    /// that enables bracketed paste before its prompt accepts input, which
    /// would otherwise let the prompt be written too early and dropped.
    /// Without that evidence by the grace deadline, the launch fails and
    /// nothing is written.
    ScreenIdle,
    /// The winning manifest rule reports idle from screen text or title.
    ///
    /// Bracketed paste and startup grace never qualify. A visible blocker
    /// takes precedence over an idle title; without eligible evidence the
    /// launch fails and writes nothing. Profile overrides keep this strict
    /// policy even when they contain no eligible idle rule.
    ManifestIdle,
}

impl InputReadiness {
    /// The readiness a session gets when `manifest` drives its detection.
    ///
    /// `ScreenIdle` holds only while the manifest has an idle rule on a screen
    /// region; without one it could never be observed, so the session gets
    /// `BracketedPaste`, the readiness of a runtime that declares none. A host
    /// profile's override manifest is the case this covers: a runtime's own
    /// manifest is checked by [`RuntimeDefinition::new`]. `ManifestIdle` never
    /// downgrades: an override manifest without an eligible idle rule keeps
    /// the strict readiness, so a prompt is refused rather than released by a
    /// weaker signal the runtime did not declare.
    #[must_use]
    pub fn for_manifest(self, manifest: &Manifest) -> Self {
        match self {
            Self::ScreenIdle if !manifest.has_screen_idle_rule() => Self::BracketedPaste,
            readiness => readiness,
        }
    }
}

/// Unvalidated inputs of a [`RuntimeDefinition`].
#[derive(Debug, Clone)]
pub struct DefinitionParts {
    /// Runtime identity.
    pub runtime_id: RuntimeId,
    /// Where the definition comes from.
    pub origin: DefinitionOrigin,
    /// Display name.
    pub display_name: String,
    /// Program to launch.
    pub program: LaunchProgram,
    /// Fixed launch arguments.
    pub default_args: Vec<String>,
    /// Input framing and safety rules.
    pub input_rules: InputRules,
    /// When the initial prompt may be written.
    pub input_readiness: InputReadiness,
    /// Whether the daemon's configured submit delay replaces the descriptor's.
    pub submit_delay_configurable: bool,
    /// Activity detection manifest.
    pub manifest: Arc<Manifest>,
    /// Native resume and fork launch, or `None` for an explicitly
    /// non-resumable runtime.
    pub native: Option<NativeSessionLaunch>,
    /// Whether the initial prompt is passed as a trailing launch argument.
    pub prompt_arg: bool,
    /// Compiled parser for the runtime's version probe, if it has one.
    pub version_probe_parser: Option<HandlerId>,
    /// Probe arguments and supported release range; required by the
    /// data-driven parser and rejected for every other one.
    pub version_probe_policy: Option<VersionProbePolicy>,
    /// Lifecycle-hook integration, if the runtime has one.
    pub integration: Option<Integration>,
}

/// A validated, immutable runtime definition.
#[derive(Debug, Clone)]
pub struct RuntimeDefinition {
    binding: LaunchBinding,
    display_name: String,
    program: LaunchProgram,
    default_args: Vec<String>,
    input_rules: InputRules,
    input_readiness: InputReadiness,
    submit_delay_configurable: bool,
    manifest: Arc<Manifest>,
    native: Option<NativeSessionLaunch>,
    prompt_arg: bool,
    version_probe_parser: Option<HandlerId>,
    version_probe_policy: Option<VersionProbePolicy>,
    integration: Option<Integration>,
    /// The literal environment pairs every launch and version probe of the
    /// runtime gets, from `[launch_env]`.
    launch_env: DescriptorLaunchEnv,
    /// The arguments that hand the session name to the agent, from
    /// `[naming] launch_args`; empty for a runtime that declares none.
    naming: NamingLaunchArgs,
    config_home: Option<ConfigHome>,
    /// Where below the config home the agent writes transcripts and which
    /// compiled parser reads them; `None` for a runtime that declares none.
    transcripts: Option<Transcripts>,
    /// The existence check recovery runs on a hook runtime's id-kind
    /// reference, as declared by `[native_reference.existence]`; `None` for a
    /// hook runtime that leaves the check undeclared.
    hook_existence: Option<ReferenceExistence>,
    /// The shortest wait for initial-input readiness this runtime needs to
    /// start, as declared by `[input] startup_grace_ms`; `None` leaves the
    /// host's grace alone.
    startup_grace: Option<Duration>,
}

impl RuntimeDefinition {
    /// Validates `parts` and derives the launch binding.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError`] for an empty or oversized label, program
    /// or argument, a control character in any of them, too many arguments, or
    /// a `ScreenIdle` or `ManifestIdle` readiness whose manifest has no idle
    /// rule on a region that readiness reads, since it would then never be
    /// observed.
    pub fn new(parts: DefinitionParts) -> Result<Self, DefinitionError> {
        Self::new_with_env(parts, DescriptorLaunchEnv::default())
    }

    /// [`Self::new`] with the validated `[launch_env]` pairs the runtime's
    /// launches and version probes get.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn new_with_env(
        parts: DefinitionParts,
        launch_env: DescriptorLaunchEnv,
    ) -> Result<Self, DefinitionError> {
        Self::new_declared(parts, launch_env, NamingLaunchArgs::default())
    }

    /// [`Self::new_with_env`] with the validated `[naming]` arguments the
    /// runtime's launches get.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub(super) fn new_declared(
        parts: DefinitionParts,
        launch_env: DescriptorLaunchEnv,
        naming: NamingLaunchArgs,
    ) -> Result<Self, DefinitionError> {
        let DefinitionParts {
            runtime_id,
            origin,
            display_name,
            program,
            default_args,
            input_rules,
            input_readiness,
            submit_delay_configurable,
            manifest,
            native,
            prompt_arg,
            version_probe_parser,
            version_probe_policy,
            integration,
        } = parts;
        validate_version_probe(version_probe_parser.as_ref(), version_probe_policy.as_ref())?;
        validate_identity_invariants(&runtime_id, &origin, &program)?;
        validate_input_readiness(input_readiness, &manifest)?;
        validate_integration(integration.as_ref(), &origin)?;
        validate_label(&display_name, "runtime.name")?;
        validate_token(program.as_str(), "runtime.program")?;
        if let LaunchProgram::Fixed(program) = &program {
            if program.contains('/') && !Path::new(program).is_absolute() {
                return Err(DefinitionError::Field {
                    field: "runtime.program",
                    reason: "must be a bare program name or an absolute path",
                });
            }
        }
        let submit_delay_ms = submit_delay_millis(input_rules.submit_delay)?;
        if let Some(native) = &native {
            validate_native(native)?;
        }
        if default_args.len() > MAX_LAUNCH_ARGS {
            return Err(DefinitionError::Field {
                field: "runtime.args",
                reason: "has too many arguments",
            });
        }
        validate_default_args(&default_args)?;
        let provenance = match origin {
            DefinitionOrigin::Builtin { package } => {
                let digest = descriptor_digest(&DescriptorFacts {
                    domain: DESCRIPTOR_DIGEST_DOMAIN,
                    runtime_id: runtime_id.as_str(),
                    program: match &program {
                        LaunchProgram::Fixed(program) => Some(program),
                        LaunchProgram::HostShell(_) => None,
                    },
                    args: &default_args,
                    input: InputFacts {
                        bracketed_paste: input_rules.bracketed_paste,
                        submit_delay_ms,
                        text_policy: text_policy_name(input_rules.text_policy),
                        allow_while_blocked: !input_rules.refuses_while_blocked(),
                    },
                    submit_delay_configurable,
                    prompt_arg,
                    native: native.as_ref(),
                    launch_env: launch_env.as_slice(),
                    naming: naming.as_slice(),
                });
                BindingProvenance::Builtin {
                    package,
                    descriptor_digest: digest,
                }
            }
            DefinitionOrigin::Package { package, digest } => BindingProvenance::Package {
                package,
                package_digest: digest,
            },
        };
        Ok(Self {
            binding: LaunchBinding {
                runtime_id,
                provenance,
            },
            display_name,
            program,
            default_args,
            input_rules,
            input_readiness,
            submit_delay_configurable,
            manifest,
            native,
            prompt_arg,
            version_probe_parser,
            version_probe_policy,
            integration,
            launch_env,
            naming,
            config_home: None,
            transcripts: None,
            hook_existence: None,
            startup_grace: None,
        })
    }

    /// This definition declaring the existence check recovery runs on a hook
    /// runtime's id-kind reference.
    #[must_use]
    pub fn with_hook_existence(mut self, existence: ReferenceExistence) -> Self {
        self.hook_existence = Some(existence);
        self
    }

    /// This definition declaring that a launch with initial input waits at
    /// least `grace` for the agent to show input readiness.
    ///
    /// The declaration is separate from [`DefinitionParts`] because it bounds
    /// the daemon's wait and takes no part in launching the agent.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] for a grace that is zero, not a
    /// whole number of milliseconds, or longer than the host accepts for its
    /// own startup grace.
    pub fn with_startup_grace(mut self, grace: Duration) -> Result<Self, DefinitionError> {
        if grace.is_zero()
            || grace > pohunek_service_config::MAX_DEADLINE
            || !grace.subsec_nanos().is_multiple_of(NANOS_PER_MILLI)
        {
            return Err(DefinitionError::Field {
                field: "input.startup_grace_ms",
                reason: "must be whole milliseconds between 1 and the host's longest grace",
            });
        }
        self.startup_grace = Some(grace);
        Ok(self)
    }

    /// This definition declaring `config_home` as the directory its agent keeps
    /// its configuration in.
    ///
    /// The declaration is separate from [`DefinitionParts`] because it names
    /// where the agent's own files live and takes no part in launching it.
    #[must_use]
    pub fn with_config_home(mut self, config_home: ConfigHome) -> Self {
        self.config_home = Some(config_home);
        self
    }

    /// This definition declaring `transcripts` as the transcript tree below
    /// its config home and the parser that reads it.
    ///
    /// Like the config home, the declaration takes no part in launching the
    /// agent; it only tells the external observer where to look.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] when the definition declares no
    /// config home and the transcripts name no root of their own, because the
    /// transcript tree is then relative to the config home.
    pub fn with_transcripts(mut self, transcripts: Transcripts) -> Result<Self, DefinitionError> {
        if self.config_home.is_none() && transcripts.root().is_none() {
            return Err(DefinitionError::Field {
                field: "transcripts",
                reason: "requires a [config_home] table or its own root_env and root_home",
            });
        }
        self.transcripts = Some(transcripts);
        Ok(self)
    }

    /// Parses a `runtime.toml` document.
    ///
    /// `origin` receives the package identity declared by the document and
    /// returns where the definition comes from. `manifest` resolves the
    /// document's `detect_manifest` name.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError`] for a malformed or oversized document, an
    /// unsupported `schema` or `runtime_api`, an unknown field, an invalid
    /// resume or fork template, a manifest the resolver rejects, or a
    /// `[transcripts]` table with an unknown parser, an unsafe directory or
    /// no `[config_home]` next to it.
    pub fn from_toml(
        source: &str,
        origin: impl FnOnce(PackageIdentity) -> DefinitionOrigin,
        manifest: impl FnOnce(&str) -> Result<Arc<Manifest>, DefinitionError>,
    ) -> Result<Self, DefinitionError> {
        Self::from_toml_with(source, &FeatureTables::PRODUCTION, origin, manifest)
    }

    /// [`Self::from_toml`] checking `requires` and the feature-gated keys
    /// against `features`.
    pub(super) fn from_toml_with(
        source: &str,
        features: &FeatureTables<'_>,
        origin: impl FnOnce(PackageIdentity) -> DefinitionOrigin,
        manifest: impl FnOnce(&str) -> Result<Arc<Manifest>, DefinitionError>,
    ) -> Result<Self, DefinitionError> {
        if source.len() > MAX_DEFINITION_BYTES {
            return Err(DefinitionError::TooLarge);
        }
        let (document, raw) = parse_document(source, features)?;
        if raw.schema != SUPPORTED_SCHEMA {
            return Err(DefinitionError::UnsupportedSchema { found: raw.schema });
        }
        if raw.runtime_api != SUPPORTED_RUNTIME_API {
            return Err(DefinitionError::UnsupportedRuntimeApi {
                found: raw.runtime_api,
            });
        }
        check_gates(&document, features)?;
        let package = PackageIdentity {
            id: PackageId::parse(&raw.id).map_err(|source| DefinitionError::Identifier {
                field: "id",
                source,
            })?,
            version: PackageVersion::parse(&raw.version).map_err(|source| {
                DefinitionError::Binding {
                    field: "version",
                    source,
                }
            })?,
        };
        let runtime_id =
            RuntimeId::parse(&raw.runtime.id).map_err(|source| DefinitionError::Identifier {
                field: "runtime.id",
                source,
            })?;
        let manifest = manifest(&raw.runtime.detect_manifest)?;
        let mut input_rules = input_rules_for(
            raw.input.text_policy.into_policy(),
            raw.input.bracketed_paste,
            Duration::from_millis(raw.input.submit_delay_ms),
        );
        if raw.input.refuse_while_blocked {
            input_rules = input_rules.refusing_while_blocked(true);
        }
        let input_readiness = raw.input.ready.into_readiness();
        let (native, hook_existence) = raw.native_and_existence()?;
        let config_home = raw
            .config_home
            .as_ref()
            .map(RawConfigHome::validated)
            .transpose()?;
        let transcripts = raw
            .transcripts
            .as_ref()
            .map(RawTranscripts::validated)
            .transpose()?;
        let launch_env = raw.validated_launch_env(config_home.as_ref())?;
        let naming = raw.validated_naming()?;
        let definition = Self::new_declared(
            DefinitionParts {
                runtime_id,
                origin: origin(package),
                display_name: raw.runtime.name,
                program: LaunchProgram::Fixed(raw.runtime.program),
                default_args: raw.runtime.args,
                input_rules,
                input_readiness,
                submit_delay_configurable: raw.input.submit_delay_configurable,
                manifest,
                native,
                prompt_arg: raw.runtime.prompt_arg,
                version_probe_parser: raw
                    .runtime
                    .version_probe
                    .as_ref()
                    .map(|probe| HandlerId::parse(&probe.parser, "runtime.version_probe.parser"))
                    .transpose()?,
                version_probe_policy: raw
                    .runtime
                    .version_probe
                    .as_ref()
                    .map(VersionProbeDeclaration::policy)
                    .transpose()?
                    .flatten(),
                integration: validated_integration(raw.integration)?,
            },
            launch_env,
            naming,
        )?;
        let definition = match hook_existence {
            Some(existence) => definition.with_hook_existence(existence),
            None => definition,
        };
        let definition = with_declared_startup_grace(definition, raw.input.startup_grace_ms)?;
        with_declared_files(definition, config_home, transcripts)
    }

    /// The runtime identity.
    #[must_use]
    pub fn runtime_id(&self) -> &RuntimeId {
        &self.binding.runtime_id
    }

    /// The launch binding a session records when it launches this runtime.
    #[must_use]
    pub fn binding(&self) -> &LaunchBinding {
        &self.binding
    }

    /// Display name.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Program to launch.
    #[must_use]
    pub fn program(&self) -> &LaunchProgram {
        &self.program
    }

    /// Fixed launch arguments.
    #[must_use]
    pub fn default_args(&self) -> &[String] {
        &self.default_args
    }

    /// Input framing and safety rules.
    #[must_use]
    pub fn input_rules(&self) -> InputRules {
        self.input_rules
    }

    /// When the initial prompt of a launch may be written.
    #[must_use]
    pub fn input_readiness(&self) -> InputReadiness {
        self.input_readiness
    }

    /// The shortest wait for initial-input readiness the runtime declares for
    /// a client that opts into the extended wait; the host's longer grace
    /// still applies.
    #[must_use]
    pub fn startup_grace(&self) -> Option<Duration> {
        self.startup_grace
    }

    /// Whether the daemon's configured submit delay replaces the descriptor's.
    #[must_use]
    pub fn submit_delay_configurable(&self) -> bool {
        self.submit_delay_configurable
    }

    /// Activity detection manifest.
    #[must_use]
    pub fn manifest(&self) -> &Arc<Manifest> {
        &self.manifest
    }

    /// Native resume and fork launch; `None` means explicitly non-resumable.
    #[must_use]
    pub fn native(&self) -> Option<&NativeSessionLaunch> {
        self.native.as_ref()
    }

    /// How this runtime obtains the native reference its recovery needs.
    #[must_use]
    pub fn native_reference_strategy(&self) -> NativeReferenceStrategy {
        match &self.native {
            None => NativeReferenceStrategy::None,
            Some(native) if native.assigned().is_some() => NativeReferenceStrategy::Assigned,
            Some(_) => NativeReferenceStrategy::Hook,
        }
    }

    /// Whether the initial prompt is passed as a trailing launch argument.
    #[must_use]
    pub fn prompt_arg(&self) -> bool {
        self.prompt_arg
    }

    /// Compiled parser id for the runtime's version probe, if any.
    #[must_use]
    pub fn version_probe_parser(&self) -> Option<&HandlerId> {
        self.version_probe_parser.as_ref()
    }

    /// Probe arguments and supported release range of a data-driven version
    /// probe, if the runtime declares one.
    #[must_use]
    pub fn version_probe_policy(&self) -> Option<&VersionProbePolicy> {
        self.version_probe_policy.as_ref()
    }

    /// Compiled integration handler id, if any.
    #[must_use]
    pub fn integration_handler(&self) -> Option<&HandlerId> {
        self.integration.as_ref().and_then(Integration::handler)
    }

    /// The hook schema the runtime's lifecycle reports follow, or `None` for
    /// a runtime without an integration, whose sessions admit no hook report.
    #[must_use]
    pub fn hook_schema(&self) -> Option<&'static HookSchema> {
        self.integration.as_ref().map(Integration::hook_schema)
    }

    /// Where the agent keeps its configuration, when the descriptor declares it.
    #[must_use]
    pub fn config_home(&self) -> Option<&ConfigHome> {
        self.config_home.as_ref()
    }

    /// The literal environment pairs, in name order, that every launch and
    /// version probe of this runtime gets; empty when the descriptor declares
    /// none.
    #[must_use]
    pub fn launch_env(&self) -> &[(String, String)] {
        self.launch_env.as_slice()
    }

    /// The arguments that hand a session named `name` to the agent, from
    /// `[naming] launch_args`; empty for a runtime that declares none.
    #[must_use]
    pub fn naming_args(&self, name: &str) -> Vec<String> {
        self.naming.render(name)
    }

    /// Where the agent writes transcripts below its config home and which
    /// compiled parser reads them, when the descriptor declares it.
    #[must_use]
    pub fn transcripts(&self) -> Option<&Transcripts> {
        self.transcripts.as_ref()
    }

    /// The existence check recovery runs on a hook runtime's id-kind
    /// reference, when the descriptor declares one under
    /// `[native_reference.existence]`.
    #[must_use]
    pub fn hook_existence(&self) -> Option<&ReferenceExistence> {
        self.hook_existence.as_ref()
    }
}

/// `definition` declaring where its agent keeps its own files: the config home
/// and the transcript tree below it.
/// `definition` with the `[input] startup_grace_ms` its descriptor declares,
/// if any.
fn with_declared_startup_grace(
    definition: RuntimeDefinition,
    millis: Option<u64>,
) -> Result<RuntimeDefinition, DefinitionError> {
    match millis {
        Some(millis) => definition.with_startup_grace(Duration::from_millis(millis)),
        None => Ok(definition),
    }
}

fn with_declared_files(
    definition: RuntimeDefinition,
    config_home: Option<ConfigHome>,
    transcripts: Option<Transcripts>,
) -> Result<RuntimeDefinition, DefinitionError> {
    let definition = match config_home {
        Some(config_home) => definition.with_config_home(config_home),
        None => definition,
    };
    match transcripts {
        Some(transcripts) => definition.with_transcripts(transcripts),
        None => Ok(definition),
    }
}

/// A hook schema without an integration handler is only valid for the shell,
/// which has no package; every package runtime that declares an integration
/// names its handler.
fn validate_integration(
    integration: Option<&Integration>,
    origin: &DefinitionOrigin,
) -> Result<(), DefinitionError> {
    let handlerless = integration.is_some_and(|integration| integration.handler().is_none());
    if handlerless && !matches!(origin, DefinitionOrigin::Builtin { package: None }) {
        return Err(DefinitionError::Invariant(
            DefinitionInvariant::HookSchemaRequiresHandler,
        ));
    }
    Ok(())
}

/// Rejects a declared readiness the manifest can never report: `screen_idle`
/// needs an idle rule on a screen region, `manifest_idle` one on a screen or
/// title region. Without it the readiness would never be observed and every
/// strict create would time out.
fn validate_input_readiness(
    readiness: InputReadiness,
    manifest: &Manifest,
) -> Result<(), DefinitionError> {
    match readiness {
        InputReadiness::ScreenIdle if !manifest.has_screen_idle_rule() => {
            Err(DefinitionError::Field {
                field: "input.ready",
                reason: "screen_idle requires a manifest idle rule on a screen region",
            })
        }
        InputReadiness::ManifestIdle if !manifest.has_manifest_idle_rule() => {
            Err(DefinitionError::Field {
                field: "input.ready",
                reason: "manifest_idle requires a manifest idle rule on a screen or title region",
            })
        }
        _ => Ok(()),
    }
}

/// A parser that takes a supported-version policy requires one whose output
/// grammar it reads, and every other parser must not carry one.
fn validate_version_probe(
    parser: Option<&HandlerId>,
    policy: Option<&VersionProbePolicy>,
) -> Result<(), DefinitionError> {
    let parser = parser.map(HandlerId::as_str);
    let data_driven = matches!(
        parser,
        Some(SEMVER_PARSER_ID | SEMVER_LINE_PARSER_ID | SEMVER_REVISION_LINE_PARSER_ID)
    );
    match (data_driven, policy) {
        (true, None) => Err(DefinitionError::Field {
            field: "runtime.version_probe",
            reason: "this parser requires args, min and below",
        }),
        (false, Some(_)) => Err(DefinitionError::Field {
            field: "runtime.version_probe",
            reason: "args, min and below are only accepted by the semver-v1, semver-line-v1 and semver-revision-line-v1 parsers",
        }),
        (true, Some(policy)) => {
            let wants_line = matches!(
                parser,
                Some(SEMVER_LINE_PARSER_ID | SEMVER_REVISION_LINE_PARSER_ID)
            );
            match (wants_line, policy.has_line_template()) {
                (true, false) => Err(DefinitionError::Field {
                    field: "runtime.version_probe.line",
                    reason: "is required by the semver-line-v1 and semver-revision-line-v1 parsers",
                }),
                (false, true) => Err(DefinitionError::Field {
                    field: "runtime.version_probe.line",
                    reason: "is only accepted by the semver-line-v1 and semver-revision-line-v1 parsers",
                }),
                _ => Ok(()),
            }
        }
        (false, None) => Ok(()),
    }
}

/// The runtime's lifecycle-hook integration, when the descriptor declares one.
fn validated_integration(
    raw: Option<RawIntegration>,
) -> Result<Option<Integration>, DefinitionError> {
    raw.map(|integration| {
        Integration::new(
            Some(HandlerId::parse(
                &integration.handler,
                "integration.handler",
            )?),
            &integration.hook_schema,
        )
    })
    .transpose()
}

/// Checks the fixed launch arguments: each is a valid token and none holds the
/// `[naming]` placeholder, which only `[naming] launch_args` may carry.
fn validate_default_args(args: &[String]) -> Result<(), DefinitionError> {
    for arg in args {
        validate_token(arg, "runtime.args")?;
        if arg.contains(SESSION_NAME_PLACEHOLDER) {
            return Err(DefinitionError::Field {
                field: "runtime.args",
                reason: "must not hold the {session_name} placeholder",
            });
        }
    }
    Ok(())
}

fn validate_label(value: &str, field: &'static str) -> Result<(), DefinitionError> {
    if value.is_empty() || value.len() > MAX_LABEL_BYTES || value.chars().any(char::is_control) {
        return Err(DefinitionError::Field {
            field,
            reason: "must be 1..=64 bytes without control characters",
        });
    }
    Ok(())
}

pub(super) fn validate_token(value: &str, field: &'static str) -> Result<(), DefinitionError> {
    if value.is_empty() || value.len() > MAX_ARG_BYTES || value.chars().any(char::is_control) {
        return Err(DefinitionError::Field {
            field,
            reason: "must be 1..=1024 bytes without control characters",
        });
    }
    Ok(())
}

/// Enforces the shell/non-shell cross-field rules.
///
/// The descriptor digest omits the host login shell, so the shell is the only
/// definition allowed to use it; every other built-in launches a fixed program
/// that the digest covers and names a package identity.
fn validate_identity_invariants(
    runtime_id: &RuntimeId,
    origin: &DefinitionOrigin,
    program: &LaunchProgram,
) -> Result<(), DefinitionError> {
    let is_shell = runtime_id.as_str() == RuntimeId::SHELL;
    let host_shell = matches!(program, LaunchProgram::HostShell(_));
    let invariant = if host_shell && !is_shell {
        DefinitionInvariant::HostShellOnlyForShell
    } else if is_shell && !host_shell {
        DefinitionInvariant::ShellRequiresHostShell
    } else if is_shell && !matches!(origin, DefinitionOrigin::Builtin { package: None }) {
        DefinitionInvariant::ShellIsPackageless
    } else if matches!(origin, DefinitionOrigin::Builtin { package: None }) && !is_shell {
        DefinitionInvariant::BuiltinRequiresPackage
    } else {
        return Ok(());
    };
    Err(DefinitionError::Invariant(invariant))
}

/// Returns the delay in whole milliseconds, rejecting a delay the descriptor
/// digest could not represent exactly (sub-millisecond precision or more than
/// `u64::MAX` milliseconds).
fn submit_delay_millis(delay: Duration) -> Result<u64, DefinitionError> {
    const INVALID: DefinitionError = DefinitionError::Field {
        field: "input.submit_delay",
        reason: "must be a whole number of milliseconds that fits in 64 bits",
    };
    if !delay.subsec_nanos().is_multiple_of(NANOS_PER_MILLI) {
        return Err(INVALID);
    }
    u64::try_from(delay.as_millis()).map_err(|_error| INVALID)
}

/// Bounds the token count and size of the resume, fork and assigned launch
/// templates, and checks that an assigned reference is an id.
fn validate_native(native: &NativeSessionLaunch) -> Result<(), DefinitionError> {
    if native.assigned().is_some() && native.reference_kind() != SessionRefKind::Id {
        return Err(DefinitionError::NativeReference(
            NativeReferenceError::AssignedRequiresIdReference,
        ));
    }
    let templates = [
        ("resume.args", Some(native.resume_args())),
        ("fork.args", native.fork_args()),
        (
            "native_reference.launch_args",
            native.assigned().map(AssignedReference::launch_args),
        ),
    ];
    for (field, args) in templates {
        let Some(args) = args else { continue };
        if args.as_slice().len() > MAX_LAUNCH_ARGS {
            return Err(DefinitionError::Field {
                field,
                reason: "has too many arguments",
            });
        }
        let oversized = args.as_slice().iter().any(|arg| {
            matches!(arg, crate::agent::NativeArg::Literal(value) if value.len() > MAX_ARG_BYTES)
        });
        if oversized {
            return Err(DefinitionError::Field {
                field,
                reason: "has a token over 1024 bytes",
            });
        }
    }
    Ok(())
}

fn text_policy_name(policy: InputTextPolicy) -> &'static str {
    match policy {
        InputTextPolicy::Unrestricted => "unrestricted",
        InputTextPolicy::HermesSafeText => "hermes_safe_text",
    }
}

/// The structural launch fields a descriptor digest covers.
///
/// The set is deliberately limited to what changes how a session launches,
/// frames input, and resumes or forks. Display text, the detection manifest,
/// the input readiness, the version-probe parser and the integration handler
/// are excluded: they do not alter a launch, and the digest never claims to
/// authenticate package contents.
#[derive(Serialize)]
struct DescriptorFacts<'a> {
    domain: &'static str,
    runtime_id: &'a str,
    program: Option<&'a String>,
    args: &'a [String],
    input: InputFacts,
    submit_delay_configurable: bool,
    prompt_arg: bool,
    native: Option<&'a NativeSessionLaunch>,
    /// Omitted while empty, so a descriptor without `[launch_env]` hashes to
    /// the value persisted sessions record.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    launch_env: &'a [(String, String)],
    /// Omitted while empty, so a descriptor without `[naming]` hashes to the
    /// value persisted sessions record.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    naming: &'a [String],
}

#[derive(Serialize)]
struct InputFacts {
    bracketed_paste: bool,
    submit_delay_ms: u64,
    text_policy: &'static str,
    allow_while_blocked: bool,
}

fn descriptor_digest(facts: &DescriptorFacts<'_>) -> DescriptorDigest {
    let encoded = serde_json::to_vec(facts)
        .expect("descriptor facts are plain strings, numbers and booleans and always serialize");
    let hash = Sha256::digest(&encoded);
    let hex = hash.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    DescriptorDigest::parse(&format!("sha256:{hex}"))
        .expect("a SHA-256 digest renders as 64 lowercase hex characters")
}

/// Text policy names accepted in `[input]`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawTextPolicy {
    Unrestricted,
    HermesSafeText,
}

impl RawTextPolicy {
    fn into_policy(self) -> InputTextPolicy {
        match self {
            Self::Unrestricted => InputTextPolicy::Unrestricted,
            Self::HermesSafeText => InputTextPolicy::HermesSafeText,
        }
    }
}

/// Builds the compiled rules for a text policy; the policy fixes whether
/// input is allowed while the agent awaits owner action.
fn input_rules_for(
    policy: InputTextPolicy,
    bracketed_paste: bool,
    submit_delay: Duration,
) -> InputRules {
    match policy {
        InputTextPolicy::Unrestricted => InputRules::unrestricted(bracketed_paste, submit_delay),
        InputTextPolicy::HermesSafeText => InputRules::hermes(bracketed_paste, submit_delay),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDefinition {
    schema: u32,
    id: String,
    version: String,
    runtime_api: u32,
    /// Runtime features the descriptor needs; checked on the untyped document
    /// before this struct is parsed.
    #[serde(default, rename = "requires")]
    _requires: Vec<String>,
    /// Commands the descriptor also exports; validated by
    /// [`super::commands::commands_of`] against the package manifest.
    #[serde(default, rename = "commands")]
    _commands: Option<toml::Value>,
    runtime: RawRuntime,
    input: RawInput,
    resume: RawResume,
    fork: RawFork,
    native_reference: RawNativeReference,
    #[serde(default)]
    integration: Option<RawIntegration>,
    #[serde(default)]
    launch_env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    naming: Option<RawNaming>,
    #[serde(default)]
    config_home: Option<RawConfigHome>,
    #[serde(default)]
    transcripts: Option<RawTranscripts>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNaming {
    launch_args: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuntime {
    id: String,
    name: String,
    program: String,
    args: Vec<String>,
    detect_manifest: String,
    #[serde(default)]
    version_probe: Option<VersionProbeDeclaration>,
    /// Absent means the prompt is delivered as terminal input.
    #[serde(default)]
    prompt_arg: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInput {
    bracketed_paste: bool,
    submit_delay_ms: u64,
    text_policy: RawTextPolicy,
    /// Absent means plain text is delivered while the session reads `blocked`.
    #[serde(default)]
    refuse_while_blocked: bool,
    /// Absent means the descriptor's delay is final.
    #[serde(default)]
    submit_delay_configurable: bool,
    /// Absent means bracketed-paste readiness.
    #[serde(default)]
    ready: RawInputReadiness,
    /// Absent means the host's startup grace alone bounds the readiness wait.
    #[serde(default)]
    startup_grace_ms: Option<u64>,
}

/// Parses `source` as an untyped table and as a [`RawDefinition`], checking
/// `requires` between the two so an unknown feature wins over the parse error
/// of a key only that feature's core understands.
fn parse_document(
    source: &str,
    features: &FeatureTables<'_>,
) -> Result<(toml::Table, RawDefinition), DefinitionError> {
    let document: toml::Table =
        toml::from_str(source).map_err(|error| malformed_at(source, &error))?;
    check_requires(&document, features)?;
    let raw = toml::from_str(source).map_err(|error| malformed_at(source, &error))?;
    Ok((document, raw))
}

/// The [`DefinitionError::Malformed`] of a TOML `error` in `source`, with the
/// one-based line of the problem when the parser reports it.
pub(super) fn malformed_at(source: &str, error: &toml::de::Error) -> DefinitionError {
    let line = error.span().map(|span| {
        source[..span.start.min(source.len())]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1
    });
    DefinitionError::Malformed { line }
}

impl RawDefinition {
    /// The `[naming] launch_args`, empty when the table is absent.
    fn validated_naming(&self) -> Result<NamingLaunchArgs, DefinitionError> {
        self.naming
            .as_ref()
            .map(|naming| NamingLaunchArgs::new(naming.launch_args.clone()))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    /// The `[launch_env]` pairs, refused when one names a variable this
    /// descriptor reads its config home or conversation root from.
    fn validated_launch_env(
        &self,
        config_home: Option<&ConfigHome>,
    ) -> Result<DescriptorLaunchEnv, DefinitionError> {
        let launch_env = self
            .launch_env
            .clone()
            .map(|declared| DescriptorLaunchEnv::new(&self.runtime.id, declared))
            .transpose()?
            .unwrap_or_default();
        launch_env.refuse_owned(
            config_home
                .map(ConfigHome::env)
                .into_iter()
                .chain(
                    self.native_reference
                        .existence
                        .as_ref()
                        .and_then(|existence| existence.root_env.as_deref()),
                )
                .chain(
                    self.transcripts
                        .as_ref()
                        .and_then(|transcripts| transcripts.root_env.as_deref()),
                ),
        )?;
        Ok(launch_env)
    }
}

impl RawConfigHome {
    fn validated(&self) -> Result<ConfigHome, DefinitionError> {
        let home = ConfigHome::new(&self.env, &self.default)?;
        match &self.subdir {
            Some(subdir) => home.with_subdir(subdir),
            None => Ok(home),
        }
    }
}

/// Readiness names accepted in `[input]`.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawInputReadiness {
    #[default]
    BracketedPaste,
    ScreenIdle,
    ManifestIdle,
}

impl RawInputReadiness {
    fn into_readiness(self) -> InputReadiness {
        match self {
            Self::BracketedPaste => InputReadiness::BracketedPaste,
            Self::ScreenIdle => InputReadiness::ScreenIdle,
            Self::ManifestIdle => InputReadiness::ManifestIdle,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResume {
    supported: bool,
    #[serde(default)]
    reference_kind: Option<SessionRefKind>,
    #[serde(default)]
    args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFork {
    supported: bool,
    #[serde(default)]
    args: Option<Vec<String>>,
}

/// Strategy names accepted in `[native_reference]`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawStrategy {
    None,
    Hook,
    Assigned,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNativeReference {
    strategy: RawStrategy,
    /// Argv that passes the generated reference at launch; `assigned` only.
    #[serde(default)]
    launch_args: Option<Vec<String>>,
    /// Whether a fork also gets a generated reference, passed with
    /// `launch_args` after the fork arguments; `assigned` only.
    #[serde(default)]
    assign_fork: Option<bool>,
    /// How recovery confirms the conversation exists; `assigned` always, or
    /// `hook` with an id-kind reference.
    #[serde(default)]
    existence: Option<ExistenceDecl>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIntegration {
    handler: String,
    hook_schema: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigHome {
    env: String,
    default: String,
    /// Directory below the `env` value; absent means the value is the home.
    #[serde(default)]
    subdir: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranscripts {
    parser: String,
    dir: String,
    /// Variable that relocates the transcript root; with `root_home`, replaces
    /// the config home as the root `dir` is relative to.
    #[serde(default)]
    root_env: Option<String>,
    /// Root below the user's home directory while `root_env` is unset.
    #[serde(default)]
    root_home: Option<String>,
    /// Directory levels below `dir` the observer lists.
    #[serde(default)]
    max_depth: Option<u64>,
    /// Object keys that lead to the native session id in the first record.
    #[serde(default)]
    id_path: Option<Vec<String>>,
    /// Object keys that lead to the working directory in the first record.
    #[serde(default)]
    cwd_path: Option<Vec<String>>,
}

impl RawTranscripts {
    /// The validated declaration; `root_env` and `root_home` come together.
    fn validated(&self) -> Result<Transcripts, DefinitionError> {
        let mut transcripts = Transcripts::new(&self.parser, &self.dir)?;
        match (&self.root_env, &self.root_home) {
            (Some(env), Some(home)) => transcripts = transcripts.with_root(env, home)?,
            (None, None) => {}
            (Some(_), None) => {
                return Err(DefinitionError::Field {
                    field: "transcripts.root_home",
                    reason: "is required with root_env",
                })
            }
            (None, Some(_)) => {
                return Err(DefinitionError::Field {
                    field: "transcripts.root_env",
                    reason: "is required with root_home",
                })
            }
        }
        if let Some(depth) = self.max_depth {
            transcripts = transcripts.with_max_depth(depth)?;
        }
        if self.id_path.is_some() || self.cwd_path.is_some() {
            transcripts =
                transcripts.with_head_paths(self.id_path.as_deref(), self.cwd_path.as_deref())?;
        }
        Ok(transcripts)
    }
}

impl RawDefinition {
    /// Resolves `[resume]`, `[fork]` and `[native_reference]` into the native
    /// launch spec plus the hook existence check, or `None` for a runtime that
    /// does not recover natively.
    ///
    /// A hook runtime declares its conversation-existence check only next to a
    /// supported resume; the returned existence is `None` for a hook runtime
    /// that leaves the check undeclared.
    fn native_and_existence(
        &self,
    ) -> Result<(Option<NativeSessionLaunch>, Option<ReferenceExistence>), DefinitionError> {
        let native = self.resume_and_fork()?;
        self.apply_strategy(native)
    }

    /// Applies the declared `[native_reference]` strategy to the resume and
    /// fork launch the other tables describe.
    ///
    /// `none` freezes recovery off, so it must sit next to `resume.supported =
    /// false`; `hook` and `assigned` both need a supported resume; only
    /// `assigned` carries a launch template and `assign_fork` (which needs a
    /// supported fork), and only `assigned`, or `hook` with an id-kind
    /// reference, carries an explicit existence check.
    fn apply_strategy(
        &self,
        native: Option<NativeSessionLaunch>,
    ) -> Result<(Option<NativeSessionLaunch>, Option<ReferenceExistence>), DefinitionError> {
        let declaration = &self.native_reference;
        let invalid = |error| DefinitionError::NativeReference(error);
        let reject_launch_args = || {
            if declaration.launch_args.is_some() {
                return Err(invalid(NativeReferenceError::UnexpectedField {
                    field: "launch_args",
                }));
            }
            if declaration.assign_fork.is_some() {
                return Err(invalid(NativeReferenceError::UnexpectedField {
                    field: "assign_fork",
                }));
            }
            Ok(())
        };
        match (declaration.strategy, native) {
            (RawStrategy::None, None) => {
                reject_launch_args()?;
                if declaration.existence.is_some() {
                    return Err(invalid(NativeReferenceError::UnexpectedField {
                        field: "existence",
                    }));
                }
                Ok((None, None))
            }
            (RawStrategy::None, Some(_)) => {
                Err(invalid(NativeReferenceError::NoneRequiresNoResume))
            }
            (RawStrategy::Hook, None) => Err(invalid(NativeReferenceError::HookRequiresResume)),
            (RawStrategy::Hook, Some(native)) => {
                reject_launch_args()?;
                if native.reference_kind() != SessionRefKind::Id {
                    if declaration.existence.is_some() {
                        return Err(invalid(NativeReferenceError::UnexpectedField {
                            field: "existence",
                        }));
                    }
                    return Ok((Some(native), None));
                }
                let existence = declaration
                    .existence
                    .as_ref()
                    .map(|decl| {
                        decl.clone()
                            .into_hook()
                            .map_err(|source| invalid(NativeReferenceError::Existence(source)))
                    })
                    .transpose()?;
                Ok((Some(native), existence))
            }
            (RawStrategy::Assigned, None) => {
                Err(invalid(NativeReferenceError::AssignedRequiresResume))
            }
            (RawStrategy::Assigned, Some(native)) => {
                let tokens = declaration
                    .launch_args
                    .as_deref()
                    .ok_or(invalid(NativeReferenceError::AssignedRequiresLaunchArgs))?;
                let launch_args = NativeArgs::from_template(tokens)
                    .map_err(|source| invalid(NativeReferenceError::LaunchArgs(source)))?;
                let existence = declaration
                    .existence
                    .clone()
                    .ok_or(invalid(NativeReferenceError::AssignedRequiresExistence))?;
                let existence = existence
                    .into_assigned()
                    .map_err(|source| invalid(NativeReferenceError::Existence(source)))?;
                let mut assigned = AssignedReference::new(launch_args, existence);
                if declaration.assign_fork == Some(true) {
                    if !native.supports_fork() {
                        return Err(invalid(NativeReferenceError::AssignForkRequiresFork));
                    }
                    assigned = assigned.with_fork_assignment();
                }
                let native = native
                    .with_assigned(assigned)
                    .map_err(|source| match source {
                        NativeLaunchError::AssignedForkRequiresFork => {
                            invalid(NativeReferenceError::AssignForkRequiresFork)
                        }
                        _ => invalid(NativeReferenceError::AssignedRequiresIdReference),
                    })?;
                Ok((Some(native), None))
            }
        }
    }

    /// Resolves `[resume]` and `[fork]` into one native launch spec.
    ///
    /// A fork is expressible only alongside the resume that declares the
    /// reference kind, and each `supported = false` table must be empty so a
    /// leftover argv cannot be mistaken for an active one.
    fn resume_and_fork(&self) -> Result<Option<NativeSessionLaunch>, DefinitionError> {
        if !self.resume.supported {
            if self.resume.reference_kind.is_some() || self.resume.args.is_some() {
                return Err(DefinitionError::Field {
                    field: "resume",
                    reason: "supported = false cannot carry reference_kind or args",
                });
            }
            if self.fork.supported || self.fork.args.is_some() {
                return Err(DefinitionError::Field {
                    field: "fork",
                    reason: "a fork requires a supported resume",
                });
            }
            return Ok(None);
        }
        let reference_kind = self.resume.reference_kind.ok_or(DefinitionError::Field {
            field: "resume.reference_kind",
            reason: "is required when resume is supported",
        })?;
        let resume_args = self
            .resume
            .args
            .as_deref()
            .ok_or(DefinitionError::Field {
                field: "resume.args",
                reason: "is required when resume is supported",
            })
            .and_then(|tokens| template("resume.args", tokens))?;
        let fork_args = match (self.fork.supported, self.fork.args.as_deref()) {
            (true, Some(tokens)) => Some(template("fork.args", tokens)?),
            (true, None) => {
                return Err(DefinitionError::Field {
                    field: "fork.args",
                    reason: "is required when fork is supported",
                })
            }
            (false, None) => None,
            (false, Some(_)) => {
                return Err(DefinitionError::Field {
                    field: "fork",
                    reason: "supported = false cannot carry args",
                })
            }
        };
        Ok(Some(NativeSessionLaunch::new(
            reference_kind,
            resume_args,
            fork_args,
        )))
    }
}

fn template(field: &'static str, tokens: &[String]) -> Result<NativeArgs, DefinitionError> {
    NativeArgs::from_template(tokens).map_err(|source| DefinitionError::Native { field, source })
}

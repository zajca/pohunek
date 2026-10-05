//! Validated, immutable agent runtime definitions.
//!
//! A [`RuntimeDefinition`] is everything the daemon needs to launch, frame
//! input for, resume, fork and observe one agent runtime, held as data. It is
//! built either from the `runtime.toml` shape ([`RuntimeDefinition::from_toml`])
//! or from explicit [`DefinitionParts`], and both paths run the same
//! validation. The `[resume]` and `[fork]` tables share the typed
//! [`NativeSessionLaunch`] template rules with host profiles, so a reference
//! slot is always one whole argv token.

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
use super::version_probe::{VersionProbePolicy, SEMVER_LINE_PARSER_ID, SEMVER_PARSER_ID};
use crate::agent::{
    AssignedReference, ExistenceSpec, InputRules, InputTextPolicy, NativeArgs, NativeLaunchError,
    NativeReferenceError, NativeReferenceStrategy, NativeSessionLaunch, ReferenceExistence,
    SessionRefKind,
};
use crate::detect::Manifest;

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
    submit_delay_configurable: bool,
    manifest: Arc<Manifest>,
    native: Option<NativeSessionLaunch>,
    prompt_arg: bool,
    version_probe_parser: Option<HandlerId>,
    version_probe_policy: Option<VersionProbePolicy>,
    integration: Option<Integration>,
    config_home: Option<ConfigHome>,
}

impl RuntimeDefinition {
    /// Validates `parts` and derives the launch binding.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError`] for an empty or oversized label, program
    /// or argument, a control character in any of them, or too many arguments.
    pub fn new(parts: DefinitionParts) -> Result<Self, DefinitionError> {
        let DefinitionParts {
            runtime_id,
            origin,
            display_name,
            program,
            default_args,
            input_rules,
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
        for arg in &default_args {
            validate_token(arg, "runtime.args")?;
        }
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
                        allow_while_blocked: input_rules.allows_while_blocked(),
                    },
                    submit_delay_configurable,
                    prompt_arg,
                    native: native.as_ref(),
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
            submit_delay_configurable,
            manifest,
            native,
            prompt_arg,
            version_probe_parser,
            version_probe_policy,
            integration,
            config_home: None,
        })
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
    /// resume or fork template, or a manifest the resolver rejects.
    pub fn from_toml(
        source: &str,
        origin: impl FnOnce(PackageIdentity) -> DefinitionOrigin,
        manifest: impl FnOnce(&str) -> Result<Arc<Manifest>, DefinitionError>,
    ) -> Result<Self, DefinitionError> {
        if source.len() > MAX_DEFINITION_BYTES {
            return Err(DefinitionError::TooLarge);
        }
        let raw: RawDefinition = toml::from_str(source).map_err(|error| {
            let line = error.span().map(|span| {
                source[..span.start.min(source.len())]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count()
                    + 1
            });
            DefinitionError::Malformed { line }
        })?;
        if raw.schema != SUPPORTED_SCHEMA {
            return Err(DefinitionError::UnsupportedSchema { found: raw.schema });
        }
        if raw.runtime_api != SUPPORTED_RUNTIME_API {
            return Err(DefinitionError::UnsupportedRuntimeApi {
                found: raw.runtime_api,
            });
        }
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
        let input_rules = input_rules_for(
            raw.input.text_policy.into_policy(),
            raw.input.bracketed_paste,
            Duration::from_millis(raw.input.submit_delay_ms),
        );
        let native = raw.native()?;
        let config_home = raw
            .config_home
            .as_ref()
            .map(|declared| ConfigHome::new(&declared.env, &declared.default))
            .transpose()?;
        let definition = Self::new(DefinitionParts {
            runtime_id,
            origin: origin(package),
            display_name: raw.runtime.name,
            program: LaunchProgram::Fixed(raw.runtime.program),
            default_args: raw.runtime.args,
            input_rules,
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
                .map(RawVersionProbe::policy)
                .transpose()?
                .flatten(),
            integration: raw
                .integration
                .map(|integration| {
                    Integration::new(
                        Some(HandlerId::parse(
                            &integration.handler,
                            "integration.handler",
                        )?),
                        &integration.hook_schema,
                    )
                })
                .transpose()?,
        })?;
        Ok(match config_home {
            Some(config_home) => definition.with_config_home(config_home),
            None => definition,
        })
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

/// A parser that takes a supported-version policy requires one whose output
/// grammar it reads, and every other parser must not carry one.
fn validate_version_probe(
    parser: Option<&HandlerId>,
    policy: Option<&VersionProbePolicy>,
) -> Result<(), DefinitionError> {
    let parser = parser.map(HandlerId::as_str);
    let data_driven = matches!(parser, Some(SEMVER_PARSER_ID | SEMVER_LINE_PARSER_ID));
    match (data_driven, policy) {
        (true, None) => Err(DefinitionError::Field {
            field: "runtime.version_probe",
            reason: "this parser requires args, min and below",
        }),
        (false, Some(_)) => Err(DefinitionError::Field {
            field: "runtime.version_probe",
            reason:
                "args, min and below are only accepted by the semver-v1 and semver-line-v1 parsers",
        }),
        (true, Some(policy)) => {
            let wants_line = parser == Some(SEMVER_LINE_PARSER_ID);
            match (wants_line, policy.has_line_template()) {
                (true, false) => Err(DefinitionError::Field {
                    field: "runtime.version_probe.line",
                    reason: "is required by the semver-line-v1 parser",
                }),
                (false, true) => Err(DefinitionError::Field {
                    field: "runtime.version_probe.line",
                    reason: "is only accepted by the semver-line-v1 parser",
                }),
                _ => Ok(()),
            }
        }
        (false, None) => Ok(()),
    }
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

fn validate_token(value: &str, field: &'static str) -> Result<(), DefinitionError> {
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
/// the version-probe parser and the integration handler are excluded: they do
/// not alter a launch, and the digest never claims to authenticate package
/// contents.
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
    runtime: RawRuntime,
    input: RawInput,
    resume: RawResume,
    fork: RawFork,
    native_reference: RawNativeReference,
    #[serde(default)]
    integration: Option<RawIntegration>,
    #[serde(default)]
    config_home: Option<RawConfigHome>,
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
    version_probe: Option<RawVersionProbe>,
    /// Absent means the prompt is delivered as terminal input.
    #[serde(default)]
    prompt_arg: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVersionProbe {
    parser: String,
    /// Probe argv; the data-driven parser only.
    #[serde(default)]
    args: Option<Vec<String>>,
    /// Lowest supported release, inclusive; the data-driven parser only.
    #[serde(default)]
    min: Option<String>,
    /// First unsupported release; the data-driven parser only.
    #[serde(default)]
    below: Option<String>,
    /// Output template; the `semver-line-v1` parser only.
    #[serde(default)]
    line: Option<String>,
}

impl RawVersionProbe {
    /// The declared policy, `None` when the parser takes none.
    ///
    /// Presence is checked against the parser by [`validate_version_probe`];
    /// here a partial declaration is already malformed.
    fn policy(&self) -> Result<Option<VersionProbePolicy>, DefinitionError> {
        match (&self.args, &self.min, &self.below) {
            (None, None, None) if self.line.is_none() => Ok(None),
            (Some(args), Some(min), Some(below)) => match &self.line {
                Some(line) => {
                    VersionProbePolicy::with_line_template(args.clone(), min, below, line)
                }
                None => VersionProbePolicy::new(args.clone(), min, below),
            }
            .map(Some),
            _ => Err(DefinitionError::Field {
                field: "runtime.version_probe",
                reason: "args, min and below are declared together",
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInput {
    bracketed_paste: bool,
    submit_delay_ms: u64,
    text_policy: RawTextPolicy,
    /// Absent means the descriptor's delay is final.
    #[serde(default)]
    submit_delay_configurable: bool,
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
    /// How recovery confirms the conversation exists; `assigned` only.
    #[serde(default)]
    existence: Option<ExistenceSpec>,
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
}

impl RawDefinition {
    /// Resolves `[resume]`, `[fork]` and `[native_reference]` into one native
    /// launch spec, or `None` for a runtime that does not recover natively.
    fn native(&self) -> Result<Option<NativeSessionLaunch>, DefinitionError> {
        let native = self.resume_and_fork()?;
        self.apply_strategy(native)
    }

    /// Applies the declared `[native_reference]` strategy to the resume and
    /// fork launch the other tables describe.
    ///
    /// `none` freezes recovery off, so it must sit next to `resume.supported =
    /// false`; `hook` and `assigned` both need a supported resume, and only
    /// `assigned` carries a launch template and an explicit existence check.
    fn apply_strategy(
        &self,
        native: Option<NativeSessionLaunch>,
    ) -> Result<Option<NativeSessionLaunch>, DefinitionError> {
        let declaration = &self.native_reference;
        let invalid = |error| DefinitionError::NativeReference(error);
        let reject_assigned_fields = || {
            if declaration.launch_args.is_some() {
                return Err(invalid(NativeReferenceError::UnexpectedField {
                    field: "launch_args",
                }));
            }
            if declaration.existence.is_some() {
                return Err(invalid(NativeReferenceError::UnexpectedField {
                    field: "existence",
                }));
            }
            Ok(())
        };
        match (declaration.strategy, native) {
            (RawStrategy::None, None) => {
                reject_assigned_fields()?;
                Ok(None)
            }
            (RawStrategy::None, Some(_)) => {
                Err(invalid(NativeReferenceError::NoneRequiresNoResume))
            }
            (RawStrategy::Hook, None) => Err(invalid(NativeReferenceError::HookRequiresResume)),
            (RawStrategy::Hook, Some(native)) => {
                reject_assigned_fields()?;
                Ok(Some(native))
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
                let existence = ReferenceExistence::try_from(existence)
                    .map_err(|source| invalid(NativeReferenceError::Existence(source)))?;
                native
                    .with_assigned(AssignedReference::new(launch_args, existence))
                    .map(Some)
                    .map_err(|_source| invalid(NativeReferenceError::AssignedRequiresIdReference))
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

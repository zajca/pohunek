//! Tests for runtime definitions, sources and the registry.

use std::sync::Arc;
use std::time::Duration;

use protocol::{
    BindingProvenance, PackageDigest, PackageId, PackageIdentity, PackageVersion, RuntimeId,
    RuntimeRef,
};

use super::{
    check_pin, validate_launch_runtime, BuiltinSource, DefinitionError, DefinitionInvariant,
    DefinitionOrigin, DefinitionParts, LaunchPin, LaunchProgram, RegistryError, RuntimeDefinition,
    RuntimeHost, RuntimeRegistry, RuntimeSource, SourceTrust, RESERVED_RUNTIME_IDS,
};
use crate::agent::{InputRules, NativeLaunchError, NativeSessionLaunch, SessionRef};
use crate::detect::{
    claude_manifest, codex_manifest, generic_shell_manifest, hermes_manifest, Manifest,
};

const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn shell_manifest() -> Arc<Manifest> {
    Arc::new(generic_shell_manifest().clone())
}

fn builtin_registry(shell: &str) -> RuntimeRegistry {
    RuntimeRegistry::from_sources(&[&BuiltinSource::new(shell)]).expect("built-in registry builds")
}

fn id(value: &str) -> RuntimeId {
    RuntimeId::parse(value).expect("valid test id")
}

fn package(id_value: &str) -> PackageIdentity {
    PackageIdentity {
        id: PackageId::parse(id_value).expect("valid package id"),
        version: PackageVersion::parse("1.0.0").expect("valid version"),
    }
}

/// A well-formed runtime document with the given `[resume]` and `[fork]`
/// tables.
fn document(resume: &str, fork: &str) -> String {
    format!(
        r#"
schema = 1
id = "acme.runtime.demo"
version = "1.0.0"
runtime_api = 1

[runtime]
id = "demo"
name = "Demo"
program = "demo"
args = []
detect_manifest = "any"

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
{resume}

[fork]
{fork}
"#
    )
}

fn parse(source: &str) -> Result<RuntimeDefinition, DefinitionError> {
    RuntimeDefinition::from_toml(
        source,
        |package| DefinitionOrigin::Package {
            package,
            digest: PackageDigest::parse(DIGEST).expect("valid digest"),
        },
        |_name| Ok(shell_manifest()),
    )
}

fn package_definition(runtime: &str) -> RuntimeDefinition {
    RuntimeDefinition::new(DefinitionParts {
        runtime_id: id(runtime),
        origin: DefinitionOrigin::Package {
            package: package("acme.runtime"),
            digest: PackageDigest::parse(DIGEST).expect("valid digest"),
        },
        display_name: "Acme".to_owned(),
        program: LaunchProgram::Fixed("acme".to_owned()),
        default_args: Vec::new(),
        input_rules: InputRules::unrestricted(false, Duration::ZERO),
        submit_delay_configurable: false,
        manifest: shell_manifest(),
        native: None,
        prompt_arg: false,
        version_probe_parser: None,
        integration_handler: None,
    })
    .expect("valid package definition")
}

#[derive(Debug)]
struct FixedSource {
    trust: SourceTrust,
    definitions: Vec<RuntimeDefinition>,
}

impl RuntimeSource for FixedSource {
    fn trust(&self) -> SourceTrust {
        self.trust
    }

    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
        Ok(self.definitions.clone())
    }
}

/// Expected launch facts of one built-in runtime, written out literally so the
/// descriptors are pinned to the behavior the daemon shipped with.
struct Expected {
    kind: RuntimeRef,
    program: &'static str,
    args: &'static [&'static str],
    input: InputRules,
    resume: &'static [&'static str],
    fork: Option<&'static [&'static str]>,
    manifest: fn() -> &'static Manifest,
}

#[test]
fn builtin_definitions_match_the_compiled_behavior() {
    let delay = Duration::from_millis(150);
    let expected = [
        Expected {
            kind: RuntimeRef::shell(),
            program: "/bin/sh",
            args: &[],
            input: InputRules::unrestricted(false, Duration::ZERO),
            resume: &[],
            fork: None,
            manifest: generic_shell_manifest,
        },
        Expected {
            kind: RuntimeRef::codex(),
            program: "codex",
            args: &[],
            input: InputRules::unrestricted(true, delay),
            resume: &["resume", "{reference}"],
            fork: None,
            manifest: codex_manifest,
        },
        Expected {
            kind: RuntimeRef::claude(),
            program: "claude",
            args: &[],
            input: InputRules::unrestricted(false, delay),
            resume: &["--resume", "{reference}"],
            fork: Some(&["--resume", "{reference}", "--fork-session"]),
            manifest: claude_manifest,
        },
        Expected {
            kind: RuntimeRef::hermes(),
            program: "hermes",
            args: &["chat"],
            input: InputRules::hermes(true, delay),
            resume: &["--resume", "{reference}"],
            fork: None,
            manifest: hermes_manifest,
        },
    ];
    let registry = builtin_registry("/bin/sh");
    for case in expected {
        let kind = &case.kind;
        let definition = registry
            .resolve(&id(kind.as_wire()))
            .expect("built-in resolves");

        assert_eq!(definition.runtime_id().as_str(), kind.as_wire());
        assert_eq!(definition.program().as_str(), case.program, "{kind:?}");
        assert_eq!(definition.default_args(), case.args, "{kind:?}");
        assert_eq!(definition.input_rules(), case.input, "{kind:?}");
        let native = if case.resume.is_empty() {
            None
        } else {
            Some(
                NativeSessionLaunch::from_templates(
                    crate::agent::SessionRefKind::Id,
                    case.resume,
                    case.fork,
                )
                .expect("expected templates are valid"),
            )
        };
        assert_eq!(definition.native(), native.as_ref(), "{kind:?}");
        assert_eq!(
            format!("{:?}", definition.manifest()),
            format!("{:?}", (case.manifest)()),
            "{kind:?}"
        );
    }
}

#[test]
fn builtin_session_behavior_flags_match_the_compiled_dispatch() {
    let registry = builtin_registry("/bin/sh");
    let flags = |name: &str| {
        let definition = registry.resolve(&id(name)).expect("built-in resolves");
        (
            definition.prompt_arg(),
            definition.submit_delay_configurable(),
            definition
                .native()
                .is_some_and(NativeSessionLaunch::supports_fork),
            definition
                .version_probe_parser()
                .map(|parser| parser.as_str().to_owned()),
        )
    };
    // The initial prompt is a launch argument for Codex and Claude only, the
    // configured submit delay applies to Claude only, and Claude is the only
    // native fork. Hermes is the only runtime with a version probe.
    assert_eq!(flags("shell"), (false, false, false, None));
    assert_eq!(flags("codex"), (true, false, false, None));
    assert_eq!(flags("claude"), (true, true, true, None));
    assert_eq!(
        flags("hermes"),
        (false, false, false, Some("hermes-v1".to_owned()))
    );
}

#[test]
fn shell_is_the_only_definition_without_a_package_and_is_not_resumable() {
    let registry = builtin_registry("/bin/sh");
    for entry in registry.inventory() {
        let BindingProvenance::Builtin {
            package,
            descriptor_digest,
        } = &entry.binding.provenance
        else {
            panic!("built-in definitions carry built-in provenance");
        };
        let is_shell = entry.runtime_id.as_str() == RuntimeId::SHELL;
        assert_eq!(package.is_none(), is_shell, "{}", entry.runtime_id);
        assert!(descriptor_digest.as_str().starts_with("sha256:"));
    }
    let shell = registry.resolve(&id("shell")).expect("shell resolves");
    assert!(shell.native().is_none());
    assert!(matches!(shell.program(), LaunchProgram::HostShell(_)));
}

#[test]
fn descriptor_digest_tracks_structural_launch_fields_only() {
    let digest = |registry: &RuntimeRegistry, name: &str| {
        registry
            .resolve(&id(name))
            .expect("resolves")
            .binding()
            .provenance
            .clone()
    };
    let first = builtin_registry("/bin/sh");
    let second = builtin_registry("/usr/bin/fish");
    for name in ["shell", "codex", "claude", "hermes"] {
        // The host login shell is not part of the descriptor.
        assert_eq!(digest(&first, name), digest(&second, name), "{name}");
    }
    assert_ne!(digest(&first, "codex"), digest(&first, "claude"));
}

#[test]
fn descriptor_digest_changes_with_launch_fields() {
    let build = |args: Vec<String>, prompt_arg: bool, name: &str| {
        RuntimeDefinition::new(DefinitionParts {
            runtime_id: id("demo"),
            origin: DefinitionOrigin::Builtin {
                package: Some(package("pohunek.runtime.demo")),
            },
            display_name: name.to_owned(),
            program: LaunchProgram::Fixed("demo".to_owned()),
            default_args: args,
            input_rules: InputRules::unrestricted(false, Duration::ZERO),
            submit_delay_configurable: false,
            manifest: shell_manifest(),
            native: None,
            prompt_arg,
            version_probe_parser: None,
            integration_handler: None,
        })
        .expect("valid")
        .binding()
        .clone()
    };
    let base = build(Vec::new(), false, "Demo");
    assert_eq!(base, build(Vec::new(), false, "Demo"));
    assert_eq!(
        base,
        build(Vec::new(), false, "Renamed"),
        "display text is not structural"
    );
    assert_ne!(base, build(vec!["--x".to_owned()], false, "Demo"));
    assert_ne!(base, build(Vec::new(), true, "Demo"));
}

#[test]
fn resume_templates_cover_id_and_path_references() {
    let by_id = parse(&document(
        "supported = true\nreference_kind = \"id\"\nargs = [\"resume\", \"{reference}\"]",
        "supported = false",
    ))
    .expect("valid");
    let native = by_id.native().expect("resumable");
    assert_eq!(
        native
            .resume_argv(&SessionRef::id("abc").expect("id"))
            .expect("renders"),
        ["resume", "abc"]
    );
    assert!(!native.supports_fork());

    let by_path = parse(&document(
        "supported = true\nreference_kind = \"path\"\nargs = [\"--transcript\", \"{reference}\"]",
        "supported = false",
    ))
    .expect("valid");
    let native = by_path.native().expect("resumable");
    assert_eq!(
        native
            .resume_argv(&SessionRef::path("/work/t.jsonl").expect("path"))
            .expect("renders"),
        ["--transcript", "/work/t.jsonl"]
    );
    let mismatch = native
        .resume_argv(&SessionRef::id("abc").expect("id"))
        .expect_err("an id is not a path reference");
    assert_eq!(mismatch.code, "native_reference_kind_mismatch");
}

#[test]
fn fork_uses_its_own_explicit_argv() {
    let definition = parse(&document(
        "supported = true\nreference_kind = \"id\"\nargs = [\"--resume\", \"{reference}\"]",
        "supported = true\nargs = [\"--resume\", \"{reference}\", \"--fork-session\"]",
    ))
    .expect("valid");
    let native = definition.native().expect("resumable");
    let reference = SessionRef::id("abc").expect("id");
    assert_eq!(
        native.resume_argv(&reference).expect("renders"),
        ["--resume", "abc"]
    );
    assert_eq!(
        native.fork_argv(&reference).expect("renders"),
        ["--resume", "abc", "--fork-session"]
    );
}

#[test]
fn reference_slot_must_be_one_whole_token() {
    let resume = |args: &str| {
        parse(&document(
            &format!("supported = true\nreference_kind = \"id\"\nargs = {args}"),
            "supported = false",
        ))
        .expect_err("invalid template")
    };
    assert_eq!(
        resume("[\"--session={reference}\"]"),
        DefinitionError::Native {
            field: "resume.args",
            source: NativeLaunchError::BracesInLiteral { index: 0 }
        }
    );
    assert_eq!(
        resume("[\"{reference}\", \"{reference}\"]"),
        DefinitionError::Native {
            field: "resume.args",
            source: NativeLaunchError::DuplicateReference
        }
    );
    assert_eq!(
        resume("[\"resume\"]"),
        DefinitionError::Native {
            field: "resume.args",
            source: NativeLaunchError::MissingReference
        }
    );
    assert_eq!(
        resume("[\"\", \"{reference}\"]"),
        DefinitionError::Native {
            field: "resume.args",
            source: NativeLaunchError::EmptyLiteral { index: 0 }
        }
    );
    let fork = parse(&document(
        "supported = true\nreference_kind = \"id\"\nargs = [\"resume\", \"{reference}\"]",
        "supported = true\nargs = [\"fork\", \"x{reference}\"]",
    ))
    .expect_err("embedded placeholder in fork");
    assert_eq!(
        fork,
        DefinitionError::Native {
            field: "fork.args",
            source: NativeLaunchError::BracesInLiteral { index: 1 }
        }
    );
}

#[test]
fn resume_and_fork_tables_must_be_consistent() {
    let field = |resume: &str, fork: &str| match parse(&document(resume, fork)) {
        Err(DefinitionError::Field { field, .. }) => field,
        other => panic!("expected a field error, got {other:?}"),
    };
    assert_eq!(
        field(
            "supported = false",
            "supported = true\nargs = [\"{reference}\"]"
        ),
        "fork"
    );
    assert_eq!(
        field(
            "supported = false\nargs = [\"resume\", \"{reference}\"]",
            "supported = false"
        ),
        "resume"
    );
    assert_eq!(
        field("supported = true", "supported = false"),
        "resume.reference_kind"
    );
    assert_eq!(
        field(
            "supported = true\nreference_kind = \"id\"",
            "supported = false"
        ),
        "resume.args"
    );
    assert_eq!(
        field(
            "supported = true\nreference_kind = \"id\"\nargs = [\"{reference}\"]",
            "supported = true"
        ),
        "fork.args"
    );
    assert_eq!(
        field(
            "supported = true\nreference_kind = \"id\"\nargs = [\"{reference}\"]",
            "supported = false\nargs = [\"{reference}\"]"
        ),
        "fork"
    );
}

#[test]
fn documents_are_strict() {
    let base = document("supported = false", "supported = false");
    parse(&base).expect("baseline is valid");
    for (from, to) in [
        ("schema = 1", "schema = 1\nextra = true"),
        ("[input]", "[input]\nunknown = 1"),
        ("name = \"Demo\"", "name = \"Demo\"\nenv = {}"),
    ] {
        assert!(
            matches!(
                parse(&base.replace(from, to)),
                Err(DefinitionError::Malformed { .. })
            ),
            "{to}"
        );
    }
    assert_eq!(
        parse(&base.replace("schema = 1", "schema = 2")).expect_err("rejected"),
        DefinitionError::UnsupportedSchema { found: 2 }
    );
    assert_eq!(
        parse(&base.replace("runtime_api = 1", "runtime_api = 9")).expect_err("rejected"),
        DefinitionError::UnsupportedRuntimeApi { found: 9 }
    );
    assert!(matches!(
        parse(&base.replace("id = \"demo\"", "id = \"Demo\"")),
        Err(DefinitionError::Identifier {
            field: "runtime.id",
            ..
        })
    ));
    assert!(matches!(
        parse(&base.replace("program = \"demo\"", "program = \"\"")),
        Err(DefinitionError::Field {
            field: "runtime.program",
            ..
        })
    ));
    assert_eq!(
        parse(&" ".repeat(super::MAX_DEFINITION_BYTES + 1)).expect_err("rejected"),
        DefinitionError::TooLarge
    );
    assert_eq!(
        RuntimeDefinition::from_toml(
            &base,
            |package| DefinitionOrigin::Builtin {
                package: Some(package)
            },
            |_| Err(DefinitionError::UnknownManifest),
        )
        .map(|_| ()),
        Err(DefinitionError::UnknownManifest)
    );
}

#[test]
fn registry_resolves_installed_runtimes_and_reports_missing_ones() {
    let registry = builtin_registry("/bin/sh");
    assert_eq!(
        registry
            .resolve(&id("claude"))
            .expect("installed")
            .display_name(),
        "Claude Code"
    );
    let error = registry.resolve(&id("absent")).expect_err("not installed");
    assert_eq!(
        error,
        protocol::ProtocolError::runtime_not_installed(&id("absent"))
    );
}

#[test]
fn inventory_is_ordered_and_complete() {
    let inventory = builtin_registry("/bin/sh").inventory();
    let ids: Vec<_> = inventory
        .iter()
        .map(|entry| entry.runtime_id.as_str())
        .collect();
    assert_eq!(ids, ["claude", "codex", "hermes", "shell"]);
    let mut reserved = RESERVED_RUNTIME_IDS;
    reserved.sort_unstable();
    assert_eq!(ids, reserved);
}

#[test]
fn reserved_ids_cannot_be_claimed_by_other_sources() {
    for reserved in RESERVED_RUNTIME_IDS {
        if reserved == RuntimeId::SHELL {
            // The shell cannot even be constructed from a package.
            continue;
        }
        let source = FixedSource {
            trust: SourceTrust::External,
            definitions: vec![package_definition(reserved)],
        };
        assert_eq!(
            RuntimeRegistry::from_sources(&[&source]).expect_err("reserved"),
            RegistryError::Reserved {
                runtime_id: id(reserved)
            }
        );
    }
    let source = FixedSource {
        trust: SourceTrust::External,
        definitions: vec![package_definition("acme")],
    };
    let registry = RuntimeRegistry::from_sources(&[&BuiltinSource::new("/bin/sh"), &source])
        .expect("a non-reserved id registers");
    registry.resolve(&id("acme")).expect("acme is registered");
    registry.resolve(&id("codex")).expect("codex is registered");
}

#[test]
fn provenance_must_match_source_trust() {
    let builtin_definition = builtin_registry("/bin/sh")
        .resolve(&id("shell"))
        .expect("shell")
        .as_ref()
        .clone();
    let external = FixedSource {
        trust: SourceTrust::External,
        definitions: vec![builtin_definition],
    };
    assert_eq!(
        RuntimeRegistry::from_sources(&[&external]).expect_err("mismatch"),
        RegistryError::ProvenanceMismatch {
            runtime_id: id("shell")
        }
    );
    let builtin = FixedSource {
        trust: SourceTrust::Builtin,
        definitions: vec![package_definition("acme")],
    };
    assert_eq!(
        RuntimeRegistry::from_sources(&[&builtin]).expect_err("mismatch"),
        RegistryError::ProvenanceMismatch {
            runtime_id: id("acme")
        }
    );
}

#[test]
fn duplicate_ids_are_rejected() {
    let first = FixedSource {
        trust: SourceTrust::External,
        definitions: vec![package_definition("acme")],
    };
    let second = FixedSource {
        trust: SourceTrust::External,
        definitions: vec![package_definition("acme")],
    };
    assert_eq!(
        RuntimeRegistry::from_sources(&[&first, &second]).expect_err("duplicate"),
        RegistryError::Duplicate {
            runtime_id: id("acme")
        }
    );
}

#[test]
fn definitions_reject_unsafe_launch_text() {
    let build = |name: &str, args: Vec<String>| {
        RuntimeDefinition::new(DefinitionParts {
            runtime_id: id("demo"),
            origin: DefinitionOrigin::Builtin {
                package: Some(package("pohunek.runtime.demo")),
            },
            display_name: name.to_owned(),
            program: LaunchProgram::Fixed("demo".to_owned()),
            default_args: args,
            input_rules: InputRules::unrestricted(false, Duration::ZERO),
            submit_delay_configurable: false,
            manifest: shell_manifest(),
            native: None,
            prompt_arg: false,
            version_probe_parser: None,
            integration_handler: None,
        })
    };
    build("Demo", Vec::new()).expect("baseline is valid");
    assert!(matches!(
        build("", Vec::new()),
        Err(DefinitionError::Field {
            field: "runtime.name",
            ..
        })
    ));
    assert!(matches!(
        build("Demo", vec!["bad\0arg".to_owned()]),
        Err(DefinitionError::Field {
            field: "runtime.args",
            ..
        })
    ));
    assert!(matches!(
        build("Demo", vec![String::new()]),
        Err(DefinitionError::Field {
            field: "runtime.args",
            ..
        })
    ));
    assert!(matches!(
        build("Demo", vec!["x".to_owned(); super::MAX_LAUNCH_ARGS + 1]),
        Err(DefinitionError::Field {
            field: "runtime.args",
            ..
        })
    ));
}

fn builtin_parts() -> DefinitionParts {
    DefinitionParts {
        runtime_id: id("demo"),
        origin: DefinitionOrigin::Builtin {
            package: Some(package("pohunek.runtime.demo")),
        },
        display_name: "Demo".to_owned(),
        program: LaunchProgram::Fixed("demo".to_owned()),
        default_args: Vec::new(),
        input_rules: InputRules::unrestricted(false, Duration::ZERO),
        submit_delay_configurable: false,
        manifest: shell_manifest(),
        native: None,
        prompt_arg: false,
        version_probe_parser: None,
        integration_handler: None,
    }
}

fn field_of(result: Result<RuntimeDefinition, DefinitionError>) -> (&'static str, &'static str) {
    match result {
        Err(DefinitionError::Field { field, reason }) => (field, reason),
        other => panic!("expected a field error, got {other:?}"),
    }
}

#[test]
fn fixed_programs_are_bare_names_or_absolute_paths() {
    let with_program = |program: &str| {
        RuntimeDefinition::new(DefinitionParts {
            program: LaunchProgram::Fixed(program.to_owned()),
            ..builtin_parts()
        })
    };
    for accepted in ["demo", "/usr/bin/demo", "demo.sh"] {
        with_program(accepted).expect(accepted);
    }
    for rejected in ["./demo", "bin/demo", "../demo", "a/"] {
        assert_eq!(
            field_of(with_program(rejected)).0,
            "runtime.program",
            "{rejected}"
        );
    }
    let toml = document("supported = false", "supported = false");
    parse(&toml.replace("program = \"demo\"", "program = \"bin/demo\"")).expect_err("relative");
    parse(&toml.replace("program = \"demo\"", "program = \"/opt/demo\"")).expect("absolute");
}

#[test]
fn malformed_documents_never_reflect_their_text() {
    let base = document("supported = false", "supported = false");
    let sentinel = "sentinel-secret-value";
    let cases = [
        base.replace("unrestricted", sentinel),
        base.replace("schema = 1", &format!("schema = 1\n\"{sentinel}\" = 1")),
        base.replace("name = \"Demo\"", &format!("name = {sentinel}")),
    ];
    for source in cases {
        let error = parse(&source).expect_err("malformed");
        assert!(
            matches!(error, DefinitionError::Malformed { .. }),
            "{error}"
        );
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains(sentinel), "{rendered}");
        }
    }
    let error = parse(&base.replace("unrestricted", "bogus")).expect_err("malformed");
    assert!(error.to_string().contains("line"), "{error}");
}

#[test]
fn submit_delay_must_be_exactly_representable() {
    let with_delay = |delay: Duration| {
        RuntimeDefinition::new(DefinitionParts {
            input_rules: InputRules::unrestricted(false, delay),
            ..builtin_parts()
        })
    };
    let digest = |delay: Duration| {
        with_delay(delay)
            .expect("representable")
            .binding()
            .provenance
            .clone()
    };
    assert_ne!(
        digest(Duration::from_millis(150)),
        digest(Duration::from_millis(151))
    );
    for rejected in [
        Duration::from_micros(1500),
        Duration::from_nanos(1),
        Duration::new(u64::MAX, 0),
        Duration::new(u64::MAX / 1000 + 1, 0),
    ] {
        assert_eq!(
            field_of(with_delay(rejected)).0,
            "input.submit_delay",
            "{rejected:?}"
        );
    }
}

#[test]
fn native_templates_are_bounded_on_every_path() {
    let template = |count: usize, size: usize| {
        let mut tokens = vec!["x".repeat(size); count - 1];
        tokens.push("{reference}".to_owned());
        tokens
    };
    let parts_with = |resume: Vec<String>, fork: Option<Vec<String>>| {
        let native = NativeSessionLaunch::from_templates(
            crate::agent::SessionRefKind::Id,
            &resume,
            fork.as_deref(),
        )
        .expect("structurally valid template");
        RuntimeDefinition::new(DefinitionParts {
            native: Some(native),
            ..builtin_parts()
        })
    };
    parts_with(
        template(super::MAX_LAUNCH_ARGS, super::MAX_ARG_BYTES),
        Some(template(super::MAX_LAUNCH_ARGS, super::MAX_ARG_BYTES)),
    )
    .expect("boundary is accepted");
    assert_eq!(
        field_of(parts_with(template(super::MAX_LAUNCH_ARGS + 1, 1), None)),
        ("resume.args", "has too many arguments")
    );
    assert_eq!(
        field_of(parts_with(template(2, super::MAX_ARG_BYTES + 1), None)),
        ("resume.args", "has a token over 1024 bytes")
    );
    assert_eq!(
        field_of(parts_with(
            template(2, 1),
            Some(template(super::MAX_LAUNCH_ARGS + 1, 1))
        ))
        .0,
        "fork.args"
    );
    assert_eq!(
        field_of(parts_with(
            template(2, 1),
            Some(template(2, super::MAX_ARG_BYTES + 1))
        ))
        .0,
        "fork.args"
    );

    let toml_args = |count: usize| {
        let tokens: Vec<String> = template(count, 1)
            .iter()
            .map(|token| format!("\"{token}\""))
            .collect();
        parse(&document(
            &format!(
                "supported = true\nreference_kind = \"id\"\nargs = [{}]",
                tokens.join(", ")
            ),
            "supported = false",
        ))
    };
    toml_args(super::MAX_LAUNCH_ARGS).expect("boundary is accepted");
    assert_eq!(
        field_of(toml_args(super::MAX_LAUNCH_ARGS + 1)).0,
        "resume.args"
    );
}

#[test]
fn shell_and_package_identity_invariants_are_enforced() {
    let invariant = |parts: DefinitionParts| match RuntimeDefinition::new(parts) {
        Err(DefinitionError::Invariant(invariant)) => invariant,
        other => panic!("expected an invariant error, got {other:?}"),
    };
    let host_shell = || LaunchProgram::HostShell("/bin/sh".to_owned());
    let shell = |program: LaunchProgram, origin: DefinitionOrigin| DefinitionParts {
        runtime_id: id("shell"),
        origin,
        program,
        ..builtin_parts()
    };
    let packageless = || DefinitionOrigin::Builtin { package: None };

    // The valid shell shape is accepted.
    RuntimeDefinition::new(shell(host_shell(), packageless())).expect("valid shell");

    assert_eq!(
        invariant(DefinitionParts {
            program: host_shell(),
            ..builtin_parts()
        }),
        DefinitionInvariant::HostShellOnlyForShell
    );
    assert_eq!(
        invariant(DefinitionParts {
            program: host_shell(),
            origin: package_origin(),
            ..builtin_parts()
        }),
        DefinitionInvariant::HostShellOnlyForShell
    );
    assert_eq!(
        invariant(DefinitionParts {
            origin: packageless(),
            ..builtin_parts()
        }),
        DefinitionInvariant::BuiltinRequiresPackage
    );
    assert_eq!(
        invariant(shell(LaunchProgram::Fixed("sh".to_owned()), packageless())),
        DefinitionInvariant::ShellRequiresHostShell
    );
    assert_eq!(
        invariant(shell(host_shell(), package_origin())),
        DefinitionInvariant::ShellIsPackageless
    );
    assert_eq!(
        invariant(shell(
            host_shell(),
            DefinitionOrigin::Builtin {
                package: Some(package("pohunek.runtime.shell"))
            }
        )),
        DefinitionInvariant::ShellIsPackageless
    );
}

fn package_origin() -> DefinitionOrigin {
    DefinitionOrigin::Package {
        package: package("acme.runtime"),
        digest: PackageDigest::parse(DIGEST).expect("valid digest"),
    }
}

#[test]
fn builtin_digest_differs_for_different_programs() {
    let digest = |program: &str| {
        RuntimeDefinition::new(DefinitionParts {
            program: LaunchProgram::Fixed(program.to_owned()),
            ..builtin_parts()
        })
        .expect("valid")
        .binding()
        .clone()
    };
    assert_ne!(digest("demo"), digest("other"));
    assert_eq!(digest("demo"), digest("demo"));
}

fn builtin_definition(name: &str) -> Arc<RuntimeDefinition> {
    Arc::clone(
        builtin_registry("/bin/sh")
            .resolve(&id(name))
            .expect("built-in resolves"),
    )
}

#[test]
fn the_host_answers_unlaunchable_kinds_with_stable_distinct_errors() {
    let host = RuntimeHost::from_host_environment();
    for kind in [
        RuntimeRef::shell(),
        RuntimeRef::codex(),
        RuntimeRef::claude(),
        RuntimeRef::hermes(),
    ] {
        host.resolve_ref(&kind).expect("built-in kind resolves");
    }
    // A valid runtime id nothing backs is not installed; a value outside the
    // grammar is presentation-only.
    let uninstalled = host
        .resolve_ref(&RuntimeRef::from_wire("acme"))
        .expect_err("uninstalled runtime");
    assert_eq!(uninstalled.code, "runtime_not_installed");
    assert!(uninstalled.msg.contains("acme"));
    for historical in ["Acme Agent", "", "a/b"] {
        let error = host
            .resolve_ref(&RuntimeRef::from_wire(historical))
            .expect_err("historical value");
        assert_eq!(error.code, "agent_kind_unsupported", "{historical:?}");
        assert!(!error.msg.contains(historical) || historical.is_empty());
    }
}

#[test]
fn only_a_host_shell_runtime_launches_with_the_host_shell_arguments() {
    let args = vec!["-c".to_owned(), "exit 0".to_owned()];
    let host = RuntimeHost::from_host_environment().with_shell_command("/bin/sh", args.clone());
    let launch_args = |name: &str| host.launch_args(&builtin_definition(name));
    assert_eq!(launch_args("shell"), args);
    assert_eq!(launch_args("hermes"), vec!["chat".to_owned()]);
    assert!(launch_args("codex").is_empty());
}

#[test]
fn a_launch_pin_round_trips_and_a_missing_pin_is_unpinned() {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Holder {
        #[serde(default, skip_serializing_if = "LaunchPin::is_unpinned")]
        pin: LaunchPin,
    }
    let unpinned: Holder = serde_json::from_str("{}").expect("absent pin parses");
    assert_eq!(unpinned.pin, LaunchPin::Unpinned);
    assert_eq!(serde_json::to_string(&unpinned).expect("serialize"), "{}");

    let pinned = Holder {
        pin: LaunchPin::of(&builtin_definition("claude")),
    };
    let json = serde_json::to_string(&pinned).expect("serialize");
    let back: Holder = serde_json::from_str(&json).expect("round trip");
    assert_eq!(back.pin, pinned.pin);
    assert_eq!(
        back.pin
            .binding()
            .map(|binding| binding.runtime_id.as_str()),
        Some("claude")
    );
}

#[test]
fn an_unpinned_snapshot_resumes_only_through_a_builtin_definition() {
    check_pin(&LaunchPin::Unpinned, &builtin_definition("claude")).expect("built-in serves");
    let error = check_pin(&LaunchPin::Unpinned, &package_definition("acme"))
        .expect_err("a package definition cannot vouch for an unpinned snapshot");
    assert_eq!(error.code, "runtime_not_installed");
}

#[test]
fn a_pin_needs_the_same_runtime_from_the_same_origin() {
    let claude = builtin_definition("claude");
    let codex = builtin_definition("codex");
    let pin = LaunchPin::of(&claude);
    check_pin(&pin, &claude).expect("the frozen definition serves its pin");
    assert_eq!(
        check_pin(&pin, &codex).expect_err("another runtime").code,
        "runtime_not_installed"
    );
    assert_eq!(
        check_pin(&pin, &package_definition("claude"))
            .expect_err("a package replacing a built-in")
            .code,
        "runtime_not_installed"
    );

    let package = package_definition("acme");
    let package_pin = LaunchPin::of(&package);
    check_pin(&package_pin, &package).expect("same package serves");
    assert_eq!(
        check_pin(&package_pin, &builtin_definition("claude"))
            .expect_err("a built-in replacing a package")
            .code,
        "runtime_not_installed"
    );
    let other_digest = RuntimeDefinition::new(DefinitionParts {
        runtime_id: id("acme"),
        origin: DefinitionOrigin::Package {
            package: package_pin
                .binding()
                .and_then(|binding| match &binding.provenance {
                    BindingProvenance::Package { package, .. } => Some(package.clone()),
                    BindingProvenance::Builtin { .. } => None,
                })
                .expect("package pin"),
            digest: PackageDigest::parse(
                "sha256:3333333333333333333333333333333333333333333333333333333333333333",
            )
            .expect("valid digest"),
        },
        display_name: "Acme".to_owned(),
        program: LaunchProgram::Fixed("acme".to_owned()),
        default_args: Vec::new(),
        input_rules: InputRules::unrestricted(false, Duration::ZERO),
        submit_delay_configurable: false,
        manifest: shell_manifest(),
        native: None,
        prompt_arg: false,
        version_probe_parser: None,
        integration_handler: None,
    })
    .expect("valid package definition");
    assert_eq!(
        check_pin(&package_pin, &other_digest)
            .expect_err("a different archive")
            .code,
        "runtime_not_installed"
    );
}

#[cfg(unix)]
mod launch_validation {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn executable(dir: &std::path::Path, body: &str) -> String {
        let path = dir.join("agent");
        std::fs::write(&path, body).expect("write executable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("set executable mode");
        path.display().to_string()
    }

    #[test]
    fn a_runtime_without_a_version_probe_launches_without_validation() {
        let missing = "/nonexistent/pohunek/agent";
        for name in ["shell", "codex", "claude"] {
            let validated = validate_launch_runtime(&builtin_definition(name), missing)
                .expect("no probe, no validation");
            assert!(validated.is_none(), "{name}");
        }
    }

    #[test]
    fn the_hermes_probe_runs_for_a_definition_that_names_its_parser() {
        let dir = crate::test_support::scoped_dir("pohunek-launch-validation-");
        let hermes = builtin_definition("hermes");
        assert_eq!(
            hermes
                .version_probe_parser()
                .map(super::super::HandlerId::as_str),
            Some("hermes-v1")
        );

        let supported = executable(&dir, "#!/bin/sh\necho 'Hermes Agent v0.20.0'\n");
        let validated = validate_launch_runtime(&hermes, &supported)
            .expect("the pinned release passes")
            .expect("the probed executable is pinned");
        assert_eq!(
            validated.as_path(),
            std::path::Path::new(&supported)
                .canonicalize()
                .expect("canonical path")
        );

        let newer = executable(&dir, "#!/bin/sh\necho 'Hermes Agent v0.21.0'\n");
        let error = validate_launch_runtime(&hermes, &newer).expect_err("another release");
        assert_eq!(error.code, "agent_runtime_unsupported");
        let error = validate_launch_runtime(&hermes, "/nonexistent/pohunek/hermes")
            .expect_err("a missing executable");
        assert_eq!(error.code, "agent_runtime_unsupported");
    }

    #[test]
    fn a_parser_the_daemon_does_not_provide_is_refused() {
        let unknown = RuntimeDefinition::new(DefinitionParts {
            runtime_id: id("acme"),
            origin: DefinitionOrigin::Package {
                package: package("acme.runtime"),
                digest: PackageDigest::parse(DIGEST).expect("valid digest"),
            },
            display_name: "Acme".to_owned(),
            program: LaunchProgram::Fixed("acme".to_owned()),
            default_args: Vec::new(),
            input_rules: InputRules::unrestricted(false, Duration::ZERO),
            submit_delay_configurable: false,
            manifest: shell_manifest(),
            native: None,
            prompt_arg: false,
            version_probe_parser: Some(
                super::super::HandlerId::parse("acme-v1", "test").expect("handler id"),
            ),
            integration_handler: None,
        })
        .expect("valid package definition");
        let error = validate_launch_runtime(&unknown, "acme").expect_err("unknown parser");
        assert_eq!(error.code, "agent_runtime_unsupported");
    }
}

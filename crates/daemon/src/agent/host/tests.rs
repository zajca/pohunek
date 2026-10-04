//! Tests for runtime definitions, sources and the registry.

use std::sync::Arc;
use std::time::Duration;

use protocol::{
    AgentKind, BindingProvenance, PackageDigest, PackageId, PackageIdentity, PackageVersion,
    RuntimeId,
};

use super::{
    BuiltinSource, DefinitionError, DefinitionOrigin, DefinitionParts, LaunchProgram,
    RegistryError, RuntimeDefinition, RuntimeRegistry, RuntimeSource, SourceTrust,
    RESERVED_RUNTIME_IDS,
};
use crate::agent::{
    adapter_for, base_native_launch, default_args, default_program, InputRules, NativeLaunchError,
    NativeSessionLaunch, SessionRef,
};
use crate::detect::{generic_shell_manifest, Manifest};

const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

const KINDS: [AgentKind; 4] = [
    AgentKind::Shell,
    AgentKind::Codex,
    AgentKind::Claude,
    AgentKind::Hermes,
];

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

#[test]
fn builtin_definitions_match_the_compiled_behavior() {
    let shell = default_program(&AgentKind::Shell);
    let registry = builtin_registry(&shell);
    for kind in KINDS {
        let runtime_id = id(kind.as_wire());
        let definition = registry.resolve(&runtime_id).expect("built-in resolves");
        let adapter = adapter_for(&kind);

        assert_eq!(definition.runtime_id().as_str(), adapter.id(), "{kind:?}");
        assert_eq!(definition.program().as_str(), default_program(&kind));
        assert_eq!(definition.default_args(), default_args(&kind).as_slice());
        assert_eq!(definition.input_rules(), adapter.input_rules(), "{kind:?}");
        assert_eq!(definition.native(), base_native_launch(&kind).as_ref());
        assert_eq!(
            format!("{:?}", definition.manifest()),
            format!("{:?}", adapter.manifest()),
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
            origin: DefinitionOrigin::Builtin { package: None },
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
        origin: DefinitionOrigin::Builtin { package: None },
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

//! Schema 1 to 2 mapping of resume bindings to the native launch shape.
//!
//! Two generations of bindings predate `native_launch`:
//!
//! - v0.33.0 froze the recovery as flat fields (`resume_mode`, `ref_kind`,
//!   `resumable`, `fork_*`). They map one to one onto a launch spec, so the
//!   translation needs nothing but the record.
//! - v0.33.1 dropped those fields without writing a `native_launch`, leaving a
//!   binding with a native reference, a snapshot program and no launch spec.
//!   A built-in agent has exactly one spec, so it is restored here. Any other
//!   runtime can only be resolved through the runtime registry, which does not
//!   exist yet while the store migrates; such a binding is marked with
//!   [`UNRESOLVED_KEY`] and repaired when the registry loads it.
//!
//! The mapping is a pure function of the record, so the migrated bytes are
//! reproducible (see `Store::is_schema_migration_of`).

// Rust guideline compliant 2026-06-26

use protocol::{RuntimeId, SessionCapabilities};
use serde_json::{Map, Value};

use crate::agent::host::{BuiltinSource, RuntimeSource};
use crate::agent::{NativeArg, NativeSessionLaunch, SessionRefKind, REFERENCE_PLACEHOLDER};

/// Key of the marker on a binding whose runtime must supply its launch spec.
pub(super) const UNRESOLVED_KEY: &str = "native_launch_unresolved";

const NATIVE_LAUNCH_KEY: &str = "native_launch";

/// Flat fields v0.33.0 stored in place of `native_launch`.
const LEGACY_KEYS: [&str; 7] = [
    "resume_mode",
    "ref_kind",
    "resumable",
    "fork_mode",
    "fork_resume_mode",
    "fork_ref_kind",
    "forkable",
];

/// Shell program the built-in source is constructed with; the migration reads
/// only the native launch specs, which do not depend on it.
const BUILTIN_SOURCE_SHELL: &str = "/bin/sh";

/// What the mapping did to one binding.
enum Outcome {
    /// The binding gained this launch spec.
    Mapped(NativeSessionLaunch),
    /// The binding awaits resolution through the runtime registry.
    Unresolved,
    /// The binding carries a native reference but its legacy fields cannot be
    /// mapped to a launch spec, so it cannot recover natively.
    Unmappable,
    /// The binding needed no new launch spec.
    Unchanged,
}

/// What the migration does to the native recovery of one record, as far as the
/// stored bytes can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RecoveryOutcome {
    /// The recovery has a launch spec or never had native recovery.
    Preserved,
    /// The launch spec is completed from the runtime registry when the
    /// record loads, which the migration cannot do.
    NeedsRegistry,
    /// The recovery cannot be mapped to a launch spec.
    Lost,
}

impl From<&Outcome> for RecoveryOutcome {
    fn from(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Mapped(_) | Outcome::Unchanged => Self::Preserved,
            Outcome::Unresolved => Self::NeedsRegistry,
            Outcome::Unmappable => Self::Lost,
        }
    }
}

/// Runs the mapping on a copy of `record` and reports what it does to the
/// recovery; the same function [`migrate_record`] applies, so the verdict
/// cannot drift from the migration.
pub(crate) fn assess_recovery(record: &Map<String, Value>) -> RecoveryOutcome {
    let mut copy = record.clone();
    migrate_record(&mut copy)
}

/// Maps one record of any kind; only resume bindings and the recovery binding
/// of session records are rewritten.
pub(super) fn migrate_record(record: &mut Map<String, Value>) -> RecoveryOutcome {
    match record.get("kind").and_then(Value::as_str) {
        Some("resume") => RecoveryOutcome::from(&migrate_binding(record)),
        Some("session") => migrate_session(record),
        _ => RecoveryOutcome::Preserved,
    }
}

fn migrate_session(record: &mut Map<String, Value>) -> RecoveryOutcome {
    let Some(Value::Object(recovery)) = record.get_mut("recovery") else {
        return RecoveryOutcome::Preserved;
    };
    let outcome = migrate_binding(recovery);
    let recovery_outcome = RecoveryOutcome::from(&outcome);
    let Outcome::Mapped(launch) = outcome else {
        return recovery_outcome;
    };
    let capabilities = SessionCapabilities {
        resume: true,
        fork: launch.supports_fork(),
    };
    if let (Some(Value::Object(info)), Ok(capabilities)) =
        (record.get_mut("info"), serde_json::to_value(capabilities))
    {
        info.insert("capabilities".to_owned(), capabilities);
    }
    recovery_outcome
}

fn migrate_binding(binding: &mut Map<String, Value>) -> Outcome {
    let had_legacy_fields = LEGACY_KEYS.iter().any(|key| binding.contains_key(*key));
    let already_current = binding
        .get(NATIVE_LAUNCH_KEY)
        .is_some_and(|launch| !launch.is_null());
    let outcome = if already_current {
        Outcome::Unchanged
    } else if had_legacy_fields {
        match legacy_launch(binding) {
            Legacy::Spec(launch) => Outcome::Mapped(launch),
            Legacy::Unmappable if has_native_reference(binding) => Outcome::Unmappable,
            Legacy::Unmappable | Legacy::NotRecoverable => Outcome::Unchanged,
        }
    } else if is_damaged(binding) {
        damaged_outcome(binding)
    } else {
        Outcome::Unchanged
    };
    for key in LEGACY_KEYS {
        binding.remove(key);
    }
    match &outcome {
        Outcome::Mapped(launch) => {
            if let Ok(encoded) = serde_json::to_value(launch) {
                binding.insert(NATIVE_LAUNCH_KEY.to_owned(), encoded);
            }
        }
        Outcome::Unresolved => {
            binding.insert(UNRESOLVED_KEY.to_owned(), Value::Bool(true));
        }
        Outcome::Unmappable | Outcome::Unchanged => {}
    }
    outcome
}

/// What the flat v0.33.0 fields translate to.
enum Legacy {
    /// The fields are one launch spec.
    Spec(NativeSessionLaunch),
    /// v0.33.0 itself did not recover this binding.
    NotRecoverable,
    /// The fields are incomplete or name a mode or kind that is not known.
    Unmappable,
}

/// Translates the flat v0.33.0 fields with the rules that version applied when
/// it relaunched: a binding with a snapshot program that was not `resumable`
/// does not recover, otherwise the frozen mode and reference kind are the
/// resume operation, and a fork needs its own complete, matching fields.
fn legacy_launch(binding: &Map<String, Value>) -> Legacy {
    let has_program = text(binding, "program").is_some_and(|program| !program.is_empty());
    if !flag(binding, "resumable") && has_program {
        return Legacy::NotRecoverable;
    }
    let spec = || {
        let mode = ResumeMode::parse(text(binding, "resume_mode")?)?;
        let kind = reference_kind(text(binding, "ref_kind")?)?;
        let resume = mode.template().map(str::to_owned);
        let fork = legacy_fork(binding, kind);
        NativeSessionLaunch::from_templates(kind, &resume, fork.as_deref()).ok()
    };
    spec().map_or(Legacy::Unmappable, Legacy::Spec)
}

/// The fork operation of the flat fields: the frozen resume mode followed by
/// the one fork extension v0.33.0 knew, which the built-in definitions carry.
fn legacy_fork(binding: &Map<String, Value>, kind: SessionRefKind) -> Option<Vec<String>> {
    if !flag(binding, "forkable") || text(binding, "fork_mode").is_none() {
        return None;
    }
    let mode = ResumeMode::parse(text(binding, "fork_resume_mode")?)?;
    // A spec has one reference kind for both operations.
    if reference_kind(text(binding, "fork_ref_kind")?)? != kind {
        return None;
    }
    let mut fork = mode.template().map(str::to_owned).to_vec();
    fork.extend(builtin_fork_extension()?);
    Some(fork)
}

/// The literal arguments a built-in fork operation appends to its resume
/// operation.
fn builtin_fork_extension() -> Option<Vec<String>> {
    let definitions = BuiltinSource::new(BUILTIN_SOURCE_SHELL).load().ok()?;
    definitions.iter().find_map(|definition| {
        let launch = definition.native()?;
        let resume = launch.resume_args().as_slice();
        let fork = launch.fork_args()?.as_slice();
        fork.strip_prefix(resume)?
            .iter()
            .map(|arg| match arg {
                NativeArg::Literal(literal) => Some(literal.clone()),
                NativeArg::Reference => None,
            })
            .collect()
    })
}

/// How v0.33.0 shaped a resume argv.
#[derive(Clone, Copy)]
enum ResumeMode {
    /// `--resume <reference>`.
    Flag,
    /// `resume <reference>`.
    Subcommand,
}

impl ResumeMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "flag" => Some(Self::Flag),
            "subcommand" => Some(Self::Subcommand),
            _ => None,
        }
    }

    fn template(self) -> [&'static str; 2] {
        match self {
            Self::Flag => ["--resume", REFERENCE_PLACEHOLDER],
            Self::Subcommand => ["resume", REFERENCE_PLACEHOLDER],
        }
    }
}

fn reference_kind(value: &str) -> Option<SessionRefKind> {
    match value {
        "id" => Some(SessionRefKind::Id),
        "path" => Some(SessionRefKind::Path),
        _ => None,
    }
}

/// Whether the binding has the shape v0.33.1 left behind: a native reference
/// and a snapshot program, but no launch spec and no recorded launch pin.
///
/// A binding without a snapshot program resolves its spec at relaunch, a
/// pinned one was written by a daemon that already knew the launch spec, and a
/// binding without a reference never recovered natively.
fn is_damaged(binding: &Map<String, Value>) -> bool {
    let has_reference = has_native_reference(binding);
    let has_program = text(binding, "program").is_some_and(|program| !program.is_empty());
    let unpinned = binding.get("launch_binding").is_none_or(Value::is_null);
    has_reference && has_program && unpinned
}

/// Whether the binding stores a native session id or path.
fn has_native_reference(binding: &Map<String, Value>) -> bool {
    ["native_session_id", "native_session_path"]
        .into_iter()
        .any(|key| text(binding, key).is_some_and(|reference| !reference.is_empty()))
}

fn damaged_outcome(binding: &Map<String, Value>) -> Outcome {
    let agent = text(binding, "agent");
    let base = text(binding, "agent_base").or(agent);
    match (agent, base) {
        // A bare built-in runtime always launched with its own spec; a shell
        // has none and so recovers nothing.
        (Some(agent), Some(base)) if agent == base => match builtin_native_launch(base) {
            BuiltinSpec::Spec(launch) => Outcome::Mapped(launch),
            BuiltinSpec::NoNativeRecovery => Outcome::Unchanged,
            BuiltinSpec::NotBuiltin => Outcome::Unresolved,
        },
        _ => Outcome::Unresolved,
    }
}

enum BuiltinSpec {
    Spec(NativeSessionLaunch),
    NoNativeRecovery,
    NotBuiltin,
}

/// The compiled native launch spec of the built-in runtime `id`.
fn builtin_native_launch(id: &str) -> BuiltinSpec {
    let Ok(runtime_id) = RuntimeId::parse(id) else {
        return BuiltinSpec::NotBuiltin;
    };
    let Ok(definitions) = BuiltinSource::new(BUILTIN_SOURCE_SHELL).load() else {
        return BuiltinSpec::NotBuiltin;
    };
    definitions
        .iter()
        .find(|definition| *definition.runtime_id() == runtime_id)
        .map_or(BuiltinSpec::NotBuiltin, |definition| {
            definition
                .native()
                .cloned()
                .map_or(BuiltinSpec::NoNativeRecovery, BuiltinSpec::Spec)
        })
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn flag(object: &Map<String, Value>, key: &str) -> bool {
    object.get(key).and_then(Value::as_bool).unwrap_or(false)
}

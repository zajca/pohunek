//! Schema 1 to 2 mapping of resume bindings to the native launch shape.
//!
//! Two generations of bindings predate `native_launch`:
//!
//! - v0.33.0 froze the recovery as flat fields (`resume_mode`, `ref_kind`,
//!   `resumable`, `fork_*`). They map one to one onto a launch spec, so the
//!   translation needs nothing but the record.
//! - v0.33.1 dropped those fields without writing a `native_launch`, leaving a
//!   binding with a native reference, a snapshot program and no launch spec.
//!   A runtime compiled into v0.33.1 had exactly one spec, so it is restored
//!   here from [`V0_33_RUNTIMES`]. Any other runtime can only be resolved
//!   through the runtime registry, which does not exist yet while the store
//!   migrates; such a binding is marked with [`UNRESOLVED_KEY`] and repaired
//!   when the registry loads it.
//!
//! The mapping is a pure function of the record and of frozen facts about the
//! releases that wrote it, so the migrated bytes are reproducible (see
//! `Store::is_schema_migration_of`) and independent of the runtimes the
//! running daemon ships.

// Rust guideline compliant 2026-10-12

use protocol::SessionCapabilities;
use serde_json::{Map, Value};

use crate::agent::{NativeSessionLaunch, SessionRefKind, REFERENCE_PLACEHOLDER};

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

/// The literal arguments v0.33.0 appended to the resume operation of a binding
/// it forked natively.
///
/// A historical store-format fact: v0.33.0 knew one fork mode
/// (`ForkMode::ClaudeSession`), which appended `CLAUDE_FORK_SESSION_ARG` in
/// `crates/daemon/src/agent/mod.rs` at tag `v0.33.0`. It must never follow a
/// current runtime descriptor, because a stored fork means what the release
/// that wrote it launched.
const V0_33_0_FORK_EXTENSION: [&str; 1] = ["--fork-session"];

/// The runtimes compiled into v0.33.1 and the native launch each recovered
/// with.
///
/// A historical store-format fact: the resume and fork arguments are verbatim
/// from `crates/daemon/src/agent/builtin/{codex,claude,hermes}.toml` at tag
/// `v0.33.1`, and that release assembled the shell without native recovery.
/// A damaged v0.33.1 binding means what that release launched, so this table
/// must never follow the current runtime descriptors, which may change or be
/// removed without changing what a stored binding recovers.
const V0_33_RUNTIMES: [FrozenRuntime; 4] = [
    FrozenRuntime {
        id: "shell",
        launch: None,
    },
    FrozenRuntime {
        id: "codex",
        launch: Some(FrozenLaunch {
            reference_kind: SessionRefKind::Id,
            resume: &["resume", REFERENCE_PLACEHOLDER],
            fork: None,
        }),
    },
    FrozenRuntime {
        id: "claude",
        launch: Some(FrozenLaunch {
            reference_kind: SessionRefKind::Id,
            resume: &["--resume", REFERENCE_PLACEHOLDER],
            fork: Some(&["--resume", REFERENCE_PLACEHOLDER, "--fork-session"]),
        }),
    },
    FrozenRuntime {
        id: "hermes",
        launch: Some(FrozenLaunch {
            reference_kind: SessionRefKind::Id,
            resume: &["--resume", REFERENCE_PLACEHOLDER],
            fork: None,
        }),
    },
];

/// One runtime a released daemon compiled in.
struct FrozenRuntime {
    /// The runtime id the release stored as `agent` and `agent_base`.
    id: &'static str,
    /// The native launch, or `None` when the runtime had no native recovery.
    launch: Option<FrozenLaunch>,
}

/// The native launch templates of one released runtime.
struct FrozenLaunch {
    reference_kind: SessionRefKind,
    resume: &'static [&'static str],
    fork: Option<&'static [&'static str]>,
}

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
/// [`V0_33_0_FORK_EXTENSION`].
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
    fork.extend(V0_33_0_FORK_EXTENSION.map(str::to_owned));
    Some(fork)
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
        // A bare runtime compiled into v0.33.1 always launched with its own
        // spec; the shell has none and so recovers nothing.
        (Some(agent), Some(base)) if agent == base => match v0_33_native_launch(base) {
            FrozenSpec::Spec(launch) => Outcome::Mapped(launch),
            FrozenSpec::NoNativeRecovery => Outcome::Unchanged,
            FrozenSpec::NotCompiledIn => Outcome::Unresolved,
        },
        _ => Outcome::Unresolved,
    }
}

/// What v0.33.1 knew about the native launch of one runtime id.
enum FrozenSpec {
    Spec(NativeSessionLaunch),
    NoNativeRecovery,
    /// The release did not compile the runtime in, or its frozen templates do
    /// not form a valid spec; either way only the registry can resolve it.
    NotCompiledIn,
}

/// The native launch spec the runtime `id` had when v0.33.1 compiled it in.
fn v0_33_native_launch(id: &str) -> FrozenSpec {
    let Some(runtime) = V0_33_RUNTIMES.iter().find(|runtime| runtime.id == id) else {
        return FrozenSpec::NotCompiledIn;
    };
    let Some(launch) = &runtime.launch else {
        return FrozenSpec::NoNativeRecovery;
    };
    NativeSessionLaunch::from_templates(launch.reference_kind, launch.resume, launch.fork)
        .map_or(FrozenSpec::NotCompiledIn, FrozenSpec::Spec)
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn flag(object: &Map<String, Value>, key: &str) -> bool {
    object.get(key).and_then(Value::as_bool).unwrap_or(false)
}

//! Frozen typed mirrors of the v0.33 releases' legacy metadata-store readers.
//!
//! The two v0.33 releases have no upgrade preflight, so the rollback gate
//! (`super::rollback`) must prove on its own that a schema-1 store survives
//! the reader of the exact previous release. Each reader parses every line as
//! a `serde` `Record` and **silently skips** a line it cannot deserialize or
//! whose agent kinds it does not support, and every mutation later rewrites
//! the whole file from what it loaded — so one skipped line permanently loses
//! a session's recovery, a worktree binding, or a known project. A syntax
//! check alone cannot prove this: a record with a known `kind` but a missing
//! or ill-typed field is accepted there and yet dropped by the reader.
//!
//! The two releases disagree, so each one is frozen separately in this crate,
//! independent of the legacy daemon binary, and a rollback validates against
//! the reader of the exact previous version. Each mirror mirrors its tag
//! attribute for attribute (the `store/mod.rs` of `v0.33.0` / `v0.33.1`, plus
//! the `protocol` and daemon `agent` wire types it embeds): required fields,
//! `serde` defaults and renames, the `deny_unknown_fields` sets, enum
//! spellings, the validated wire types (canonical decimal strings, `sha256:`
//! digests, native launch argv invariants), and the post-parse
//! persistability checks. The agent-kind rules differ the widest: v0.33.0's
//! `AgentKind` recognizes only the reserved built-in spellings, while
//! v0.33.1's `RuntimeRef` keeps a grammar-valid custom id, and v0.33.1's
//! native launch snapshot (`NativeSessionLaunch`, `LaunchBinding`) v0.33.0
//! ignores in favor of its own mode/`fork` enum fields. A line that the
//! selected reader would drop refuses the rollback, and a line it would keep
//! passes — one release's acceptance never covers another's.
//!
//! Wire shapes here are consume-only: nothing in this module serializes.
//! When a schema-1 store is ever re-read for another release, re-verify every
//! rule against the authority tag before changing anything.

// Rust guideline compliant 2026-10-09

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::de;
use serde_json::Value;

/// Wire spellings that reserve their own base in both legacy readers —
/// v0.33.1's `RESERVED_RUNTIME_IDS` (`agent/host/registry.rs`) and the
/// `legacy_agent_base_from_agent` match in the v0.33.0 `store/mod.rs`.
///
/// A resume binding written without `agent_base` derives it from its `agent`
/// only when the agent is one of these names.
const RESERVED_AGENT_KINDS: [&str; 4] = ["shell", "codex", "claude", "hermes"];

/// Maximum UTF-8 bytes of a runtime id (`protocol/runtime_id.rs`).
const MAX_RUNTIME_ID_BYTES: usize = 64;
/// Maximum UTF-8 bytes of a runtime package id (`protocol/runtime_id.rs`).
const MAX_PACKAGE_ID_BYTES: usize = 128;
/// Maximum UTF-8 bytes of a runtime package version (`protocol/runtime_id.rs`).
const MAX_PACKAGE_VERSION_BYTES: usize = 64;
/// Hex characters after the `sha256:` prefix of a validated digest.
const DIGEST_HEX_CHARS: usize = 64;

/// The schema the legacy readers stamp and read.
const LEGACY_SCHEMA: u32 = 1;

/// The released legacy reader a rollback must prove a schema-1 store
/// against, selected by the previous release's version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LegacyReader {
    /// The v0.33.0 reader: `AgentKind` bases and the mode/`fork` snapshot
    /// fields of `ResumeBinding`.
    V0_33_0,
    /// The v0.33.1 reader: `RuntimeRef` id bases and the native launch
    /// snapshot of `ResumeBinding`.
    V0_33_1,
}

/// The released version of the v0.33.0 reader; its daemon has no upgrade
/// preflight and writes schema 1.
pub(super) const V0_33_0_RELEASE: &str = "0.33.0";
/// The released version of the v0.33.1 reader; same gate.
pub(super) const V0_33_1_RELEASE: &str = "0.33.1";

impl LegacyReader {
    /// Selects the frozen reader of a released legacy daemon version.
    pub(super) fn from_version(version: &str) -> Option<Self> {
        match version {
            V0_33_0_RELEASE => Some(Self::V0_33_0),
            V0_33_1_RELEASE => Some(Self::V0_33_1),
            _ => None,
        }
    }
}

/// The post-parse skip rule of one frozen reader, over its record type.
///
/// Both readers fail a line whose deserialization closes; some lines that
/// deserialize carry agent kinds the reader still drops at load, and the
/// trait carries that per-version rule so one validation loop proves both.
trait ReaderSkip {
    /// Why the reader would skip a record that deserialized, or `None` when
    /// it keeps it.
    fn skip_reason(record: &Self) -> Option<&'static str>;
}

/// Validates one schema-1 store body against the reader that `reader`
/// freezes.
///
/// Besides the syntax, schema, and kind checks the store scan already makes,
/// this proves what they cannot: that every line deserializes through the
/// frozen reader of the previous release and passes its post-parse
/// persistability rules, so that reader keeps, rather than silently skips,
/// every record on its next whole-store rewrite. Returns the number of
/// records the reader would keep.
///
/// # Errors
///
/// Returns `("legacy_store_invalid", detail)` for a line the reader would
/// drop and `("legacy_store_schema_unsupported", detail)` for a schema it
/// would never write.
pub(super) fn validate_records(
    bytes: &[u8],
    reader: LegacyReader,
) -> Result<usize, (&'static str, String)> {
    match reader {
        LegacyReader::V0_33_0 => validate_through::<Record330>(bytes),
        LegacyReader::V0_33_1 => validate_through::<Record>(bytes),
    }
}

/// Validates a store body against one frozen reader's typed record shape.
fn validate_through<T>(bytes: &[u8]) -> Result<usize, (&'static str, String)>
where
    T: serde::de::DeserializeOwned + ReaderSkip,
{
    let body = std::str::from_utf8(bytes).map_err(|error| {
        (
            "legacy_store_invalid",
            format!("metadata store is not UTF-8: {error}"),
        )
    })?;
    let mut records = 0;
    for (index, line) in body.lines().enumerate() {
        // The reader filters blank lines before parsing; skip them too, but
        // keep numbering the lines by their real position.
        if line.trim().is_empty() {
            continue;
        }
        let number = index + 1;
        let parsed: Value = serde_json::from_str(line).map_err(|error| {
            (
                "legacy_store_invalid",
                format!("line {number} is not JSON: {error}"),
            )
        })?;
        let record = parsed.as_object().ok_or_else(|| {
            (
                "legacy_store_invalid",
                format!("line {number} is not a record"),
            )
        })?;
        let schema = record
            .get("schema_version")
            .map_or(Some(u64::from(LEGACY_SCHEMA)), Value::as_u64);
        if schema != Some(u64::from(LEGACY_SCHEMA)) {
            return Err((
                "legacy_store_schema_unsupported",
                format!(
                    "line {number} has schema {schema:?}; the previous daemon reads schema \
                     {LEGACY_SCHEMA}"
                ),
            ));
        }
        if !matches!(
            record.get("kind").and_then(Value::as_str),
            Some("session" | "resume" | "worktree" | "project")
        ) {
            return Err((
                "legacy_store_invalid",
                format!("line {number} has an unknown record kind"),
            ));
        }
        let checked = serde_json::from_str::<T>(line);
        match checked {
            Ok(record) => {
                if let Some(reason) = T::skip_reason(&record) {
                    return Err((
                        "legacy_store_invalid",
                        format!("line {number} would be skipped: {reason}"),
                    ));
                }
            }
            Err(error) => {
                return Err((
                    "legacy_store_invalid",
                    format!("line {number} would be skipped: {error}"),
                ));
            }
        }
        records += 1;
    }
    Ok(records)
}

/// The post-parse persistability gate of a legacy session record.
///
/// Mirrors the legacy reader: a session record loads but is dropped unless
/// every agent kind it carries is a runtime id — the summary's base, the
/// active nested agent's base when present, and the recovery binding's base
/// when present.
fn session_persistable(record: &SessionRecord) -> bool {
    persistable(&record.info.agent_base)
        && record
            .info
            .active_agent_base
            .as_ref()
            .is_none_or(persistable)
        && record
            .recovery
            .as_ref()
            .is_none_or(|binding| persistable(&binding.agent_base))
}

impl ReaderSkip for Record {
    fn skip_reason(record: &Self) -> Option<&'static str> {
        match record {
            Record::Session(session) if !session_persistable(session) => {
                Some("the agent kind is not a runtime id")
            }
            _ => None,
        }
    }
}

/// One line of the legacy store: internally tagged by `kind`, `snake_case`.
///
/// Mirrors the v0.33.1 `Record` enum. Unknown fields are ignored (the legacy
/// reader never denies them at this level); an unknown kind fails the
/// deserialization the reader skips.
#[derive(Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Record {
    /// A durable logical session.
    Session(Box<SessionRecord>),
    /// A native recovery binding of one logical session.
    Resume(Box<ResumeBinding>),
    /// A bound worktree.
    Worktree(WorktreeBinding),
    /// A known project.
    Project(ProjectRecord),
}

/// A runtime reference read back through the legacy reader: any string, with
/// a grammar-valid one a runtime id.
///
/// Mirrors `protocol`'s lenient `RuntimeRef`, whose `Deserialize` classifies
/// every wire string and never fails. Persistence keeps only id references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RuntimeRef {
    /// The verbatim wire value.
    wire: String,
    /// Whether the value parses as a grammar-valid runtime id.
    id: bool,
}

impl RuntimeRef {
    /// Classifies a wire string the legacy way.
    fn from_wire(wire: String) -> Self {
        Self {
            id: runtime_id_grammar(&wire),
            wire,
        }
    }

    /// Whether the reference names a runtime id, so the record persists.
    fn persistable(&self) -> bool {
        self.id
    }
}

impl<'de> serde::Deserialize<'de> for RuntimeRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from_wire)
    }
}

/// Whether an agent kind keeps a legacy record: a runtime id, installed or
/// not.
fn persistable(base: &RuntimeRef) -> bool {
    base.persistable()
}

/// A validated string newtype the legacy reader enforces on load.
macro_rules! validated_wire {
    ($name:ident, $validate:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub(super) struct $name(String);

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let wire = String::deserialize(deserializer)?;
                let validate: fn(&str) -> bool = $validate;
                if validate(&wire) {
                    Ok(Self(wire))
                } else {
                    Err(serde::de::Error::custom(
                        "the value is not a valid wire value",
                    ))
                }
            }
        }
    };
}

validated_wire!(StrictRuntimeId, runtime_id_grammar);
validated_wire!(StrictPackageId, package_id_grammar);
validated_wire!(StrictPackageVersion, package_version_grammar);
validated_wire!(StrictDigest, digest_grammar);

/// Whether `wire` parses as a grammar-valid runtime id.
///
/// Mirrors `protocol`'s `validate_identifier`: non-empty, at most
/// `MAX_RUNTIME_ID_BYTES` bytes of lowercase ASCII alphanumerics, `.`, `_` and
/// `-`, no leading `.` or `-`, no `..`.
fn runtime_id_grammar(wire: &str) -> bool {
    identifier_grammar(wire, MAX_RUNTIME_ID_BYTES)
}

/// Whether `wire` parses as a grammar-valid package id.
///
/// Same character rules as a runtime id, wider ceiling.
fn package_id_grammar(wire: &str) -> bool {
    identifier_grammar(wire, MAX_PACKAGE_ID_BYTES)
}

fn identifier_grammar(wire: &str, max_bytes: usize) -> bool {
    !wire.is_empty()
        && wire.len() <= max_bytes
        && wire.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
        && !wire.starts_with(['.', '-'])
        && !wire.contains("..")
}

/// Whether `wire` parses as a package version: 1..64 bytes of ASCII
/// alphanumerics and `.`, `+`, `_`, `-`.
fn package_version_grammar(wire: &str) -> bool {
    !wire.is_empty()
        && wire.len() <= MAX_PACKAGE_VERSION_BYTES
        && wire
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".+_-".contains(&byte))
}

/// Whether `wire` parses as a content digest: `sha256:` plus 64 lowercase hex
/// characters.
fn digest_grammar(wire: &str) -> bool {
    wire.strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == DIGEST_HEX_CHARS && hex.bytes().all(is_lower_hex))
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

/// A canonical unsigned decimal wire string.
///
/// Mirrors `protocol`'s `decimal_wire_type`: digits that parse as `u64` and
/// spell it back exactly, so no leading zeroes, signs, or overflow pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DecimalWire(u64);

impl<'de> serde::Deserialize<'de> for DecimalWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wired = String::deserialize(deserializer)?;
        let parsed = wired.parse::<u64>().map_err(|_parse_error| {
            de::Error::custom("the decimal wire integer is not canonical")
        })?;
        if parsed.to_string() == wired {
            Ok(Self(parsed))
        } else {
            Err(de::Error::custom(
                "the decimal wire integer is not canonical",
            ))
        }
    }
}

/// A resume binding, whose `agent_base` is derived on load.
///
/// Mirrors the v0.33.1 `ResumeBinding`: the raw serde form below, plus the
/// legacy load rules in [`TryFrom<RawResumeBinding>`] — a recorded
/// `agent_base` wins, otherwise only a reserved runtime id derives its own
/// base, and the result must name a runtime id for the record to persist.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "RawResumeBinding")]
pub(super) struct ResumeBinding {
    /// The pohunek session id.
    pub session_id: String,
    /// Owner-set display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Agent name backing the session.
    pub agent: String,
    /// Recorded or derived agent base, whose id-ness decides persistence.
    pub agent_base: RuntimeRef,
    /// Working directory to relaunch in.
    pub cwd: PathBuf,
    /// Terminal width at capture time.
    pub cols: u16,
    /// Terminal height at capture time.
    pub rows: u16,
    /// Captured native session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Captured native session path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_path: Option<String>,
    /// Project this session belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Whether the session's cwd is a linked worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_linked_worktree: Option<bool>,
    /// Owner-controlled session metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// Resolved launch program, frozen at creation.
    #[serde(default)]
    pub program: String,
    /// Resolved launch args, frozen at creation.
    #[serde(default)]
    pub args: Vec<String>,
    /// Resolved input-framing rules, frozen at creation.
    #[serde(default)]
    pub input_rules: StoredInputRules,
    /// Native-session launch spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_launch: Option<NativeSessionLaunch>,
    /// Launch binding frozen with the snapshot; recorded only when pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_binding: Option<LaunchBinding>,
}

impl TryFrom<RawResumeBinding> for ResumeBinding {
    type Error = String;

    fn try_from(raw: RawResumeBinding) -> Result<Self, Self::Error> {
        // Derivation per the legacy reader: a recorded base wins; otherwise
        // only a reserved id derives its own base, anything else fails to
        // load, skipping the line.
        let agent_base = if let Some(base) = raw.agent_base {
            Some(base)
        } else {
            RESERVED_AGENT_KINDS
                .contains(&raw.agent.as_str())
                .then(|| RuntimeRef::from_wire(raw.agent.clone()))
        };
        let agent_base = agent_base.ok_or_else(|| "agent_name_has_no_base".to_owned())?;
        if !persistable(&agent_base) {
            return Err("agent_base is not a valid runtime id".to_owned());
        }
        Ok(Self {
            session_id: raw.session_id,
            name: raw.name,
            agent: raw.agent,
            agent_base,
            cwd: raw.cwd,
            cols: raw.cols,
            rows: raw.rows,
            native_session_id: raw.native_session_id,
            native_session_path: raw.native_session_path,
            project_id: raw.project_id,
            is_linked_worktree: raw.is_linked_worktree,
            metadata: raw.metadata,
            program: raw.program,
            args: raw.args,
            input_rules: raw.input_rules,
            native_launch: raw.native_launch,
            launch_binding: raw.launch_binding,
        })
    }
}

/// The serde form of a legacy resume binding.
///
/// Field for field the legacy `RawResumeBinding`, with its `serde` defaults.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct RawResumeBinding {
    session_id: String,
    #[serde(default)]
    name: Option<String>,
    agent: String,
    #[serde(default)]
    agent_base: Option<RuntimeRef>,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    #[serde(default)]
    native_session_id: Option<String>,
    #[serde(default)]
    native_session_path: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    is_linked_worktree: Option<bool>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default)]
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    input_rules: StoredInputRules,
    #[serde(default)]
    native_launch: Option<NativeSessionLaunch>,
    #[serde(default)]
    launch_binding: Option<LaunchBinding>,
}

/// Native-session launch spec frozen into a legacy recovery snapshot.
///
/// Mirrors the legacy `NativeSessionLaunch`: unknown fields denied, a
/// reference kind of `id` or `path`, and argv fragments that pass the
/// `NativeArgs` invariants.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeSessionLaunch {
    reference_kind: SessionRefKind,
    resume_args: NativeArgs,
    #[serde(default)]
    fork_args: Option<NativeArgs>,
}

/// Whether a native launch reference is an opaque id or a transcript path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SessionRefKind {
    /// An opaque native session id.
    Id,
    /// A path to a native session transcript file.
    Path,
}

/// A validated argv fragment with exactly one reference slot.
///
/// Mirrors the legacy `NativeArgs` (`try_from` the token list): at least one
/// token, exactly one reference, and every literal non-empty, free of control
/// characters and braces.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "Vec<NativeArg>")]
pub(super) struct NativeArgs(Vec<NativeArg>);

impl TryFrom<Vec<NativeArg>> for NativeArgs {
    type Error = String;

    fn try_from(args: Vec<NativeArg>) -> Result<Self, Self::Error> {
        if args.is_empty() {
            return Err("the argument list is empty".to_owned());
        }
        let mut references = 0;
        for arg in &args {
            match arg {
                NativeArg::Literal(value) => {
                    if value.is_empty() {
                        return Err("an argument is an empty literal".to_owned());
                    }
                    if value.chars().any(char::is_control) {
                        return Err("an argument contains a control character".to_owned());
                    }
                    if value.contains(['{', '}']) {
                        return Err("an argument contains braces".to_owned());
                    }
                }
                NativeArg::Reference => references += 1,
            }
        }
        match references {
            0 => Err("the argument list has no reference placeholder".to_owned()),
            1 => Ok(Self(args)),
            _ => Err("the argument list has more than one reference placeholder".to_owned()),
        }
    }
}

/// One element of a legacy native launch argv.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum NativeArg {
    /// A fixed argument passed through verbatim.
    Literal(String),
    /// The slot filled with the native session reference.
    Reference,
}

/// Input-framing rules frozen into a legacy recovery snapshot.
///
/// Both fields default, like the legacy `StoredInputRules`; unknown fields
/// are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub(super) struct StoredInputRules {
    /// Whether prompt text is wrapped in bracketed-paste markers.
    #[serde(default)]
    pub bracketed_paste: bool,
    /// Delay before the submit byte, in whole milliseconds.
    #[serde(default)]
    pub submit_delay_ms: u64,
}

/// The legacy launch-time runtime identity, frozen with a snapshot.
///
/// Unknown fields denied, and the runtime id validated by its strict grammar.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LaunchBinding {
    /// Selected runtime identity.
    runtime_id: StrictRuntimeId,
    /// Where the runtime definition came from.
    provenance: BindingProvenance,
}

/// Where a legacy launch binding's identity comes from, internal-tagged, with
/// every field validated as the reader does.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum BindingProvenance {
    /// A runtime compiled into the daemon.
    Builtin {
        /// Package identity the descriptor stands for, when it has one.
        #[serde(default)]
        package: Option<PackageIdentity>,
        /// Digest of the descriptor's structural launch fields.
        descriptor_digest: StrictDigest,
    },
    /// A runtime installed from a package.
    Package {
        /// Package that exports the runtime.
        package: PackageIdentity,
        /// Digest of the installed package archive.
        package_digest: StrictDigest,
    },
}

/// Package identity of a legacy launch binding.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PackageIdentity {
    /// Package id.
    id: StrictPackageId,
    /// Package version.
    version: StrictPackageVersion,
}

/// Durable lifecycle outcome of a legacy session record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DesiredState {
    /// A worker runtime should be live.
    Running,
    /// The runtime should be terminal.
    Stopped,
    /// The runtime and logical record should be removed.
    Removed,
}

/// Durable lifecycle operation legacy reconciliation must finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TransactionKind {
    /// Initial worker creation.
    Create,
    /// Explicit runtime stop.
    Stop,
    /// Explicit provider-native recovery.
    Recover,
    /// Logical session removal.
    Remove,
}

/// One in-progress legacy lifecycle transaction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct SessionTransaction {
    /// Stable operation identifier.
    pub id: String,
    /// The operation.
    pub kind: TransactionKind,
    /// The implementation phase.
    pub phase: String,
    /// Worker replaced by a recovery transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_worker_id: Option<String>,
    /// Runtime generation replaced by a recovery transaction; the persisted
    /// key stays `runtime_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "previous_runtime_id")]
    pub previous_worker_instance_id: Option<String>,
    /// Daemon instance whose in-memory work the transaction depends on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_instance_id: Option<String>,
}

/// Availability of a legacy PTY runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RuntimeState {
    /// A worker unit is being initialized.
    Starting,
    /// The worker and PTY are connected.
    Live,
    /// The daemon is reconnecting.
    Reconnecting,
    /// A terminal child outcome was observed.
    Terminal,
    /// The worker or host was lost.
    Lost,
    /// More than one runtime claims the logical session.
    Conflict,
    /// The worker speaks no compatible private protocol.
    Incompatible,
}

/// Lifecycle state of a legacy logical session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SessionState {
    /// The session is starting.
    Starting,
    /// The session runs.
    Running,
    /// A stop was requested.
    Stopped,
    /// The session completed successfully.
    Done,
    /// The session failed.
    Failed,
}

/// Source scale of a published runtime state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StateSource {
    /// Derived from the OSC terminal title.
    OscTitle,
    /// Derived from OSC progress reports.
    OscProgress,
    /// Derived from screen-content manifest matching.
    Screen,
    /// Derived from process or PTY activity.
    Process,
    /// Reported by an agent hook.
    Report,
}

/// Source of a session's working-directory value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CwdSource {
    /// Captured at session launch or resume.
    Launch,
    /// Read from the focus process.
    Procwatch,
    /// Reported by OSC 7 terminal output.
    Osc7,
}

/// Coarse detected activity of an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AgentActivity {
    /// The agent is working.
    Working,
    /// The agent waits for input.
    Blocked,
    /// The agent runs but produces no work.
    Idle,
}

/// Lifecycle of one provider-managed subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SubagentLifecycle {
    /// The subagent is running.
    Running,
    /// The subagent completed successfully.
    Completed,
    /// The subagent completed unsuccessfully.
    Failed,
    /// The subagent was cancelled.
    Cancelled,
    /// The owning runtime ended first.
    Lost,
}

/// Mutation capabilities frozen for a legacy session.
///
/// Unknown fields were denied at v0.33.1, so an unrecognized capability key
/// skips the line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionCapabilities {
    /// The session has a frozen native resume.
    pub resume: bool,
    /// The session has a frozen native fork.
    pub fork: bool,
}

/// Durable worker runtime information of a legacy session summary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct SessionRuntime {
    /// Availability of the runtime.
    pub state: RuntimeState,
    /// Monotonic generation, a canonical decimal wire string.
    pub runtime_generation: DecimalWire,
    /// Worker that owns the PTY, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// Worker instance identity, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_instance_id: Option<String>,
    /// When this generation started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Latest successful daemon connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_at: Option<String>,
    /// Reason when the runtime is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_reason: Option<String>,
}

/// One provider-managed subagent of a legacy session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct SubagentInfo {
    /// Provider-native identifier.
    pub id: String,
    /// Parent provider-native identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Provider that owns the subagent.
    pub provider: RuntimeRef,
    /// Provider-defined subagent type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Lifecycle state.
    pub lifecycle: SubagentLifecycle,
    /// Coarse activity while running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<AgentActivity>,
    /// Worker-owned monotonic revision, a canonical decimal wire string.
    pub revision: DecimalWire,
    /// Unix-millisecond timestamp of the accepted start hook.
    pub started_at_ms: u64,
    /// Unix-millisecond timestamp of the latest transition.
    pub updated_at_ms: u64,
    /// Unix-millisecond timestamp of the terminal transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
}

/// A non-fatal worktree-setup warning of a legacy session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct SessionWarning {
    /// The warning kind.
    pub kind: SessionWarningKind,
    /// Human-readable summary.
    pub message: String,
    /// Optional raw detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The kind of a non-fatal worktree-setup warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SessionWarningKind {
    /// A remote `git fetch` failed.
    Fetch,
    /// The requested base branch fell back to the default branch.
    BaseBranchFallback,
    /// The setup script failed.
    SetupScript,
    /// A lifecycle hook failed.
    Hook,
}

/// Sanitized client-facing legacy session snapshot.
///
/// Mirrors the v0.33.1 `SessionInfo`: required observation fields, defaults
/// for everything v0.33 kept optional, and unknown fields kept.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct SessionInfo {
    /// Stable session identifier.
    pub id: String,
    /// Whether the entry is an observe-only external process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<bool>,
    /// Capabilities frozen at session start.
    #[serde(default)]
    pub capabilities: SessionCapabilities,
    /// Owner-set display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Agent profile name backing the session.
    pub agent: String,
    /// Runtime identity backing the session, whose id-ness decides persistence.
    pub agent_base: RuntimeRef,
    /// Current working directory.
    pub cwd: PathBuf,
    /// Source that last set the working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_source: Option<CwdSource>,
    /// Process id of the session root process.
    pub pid: u32,
    /// Durable worker runtime information; `None` predates worker-backed
    /// sessions or marks an observe-only entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<SessionRuntime>,
    /// Terminal width.
    pub cols: u16,
    /// Terminal height.
    pub rows: u16,
    /// Lifecycle state.
    pub state: SessionState,
    /// Source of the state signal.
    pub state_source: StateSource,
    /// Detected agent activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<AgentActivity>,
    /// Provider-managed subagents.
    #[serde(default)]
    pub subagents: Vec<SubagentInfo>,
    /// Active nested agent profile name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_agent: Option<String>,
    /// Runtime identity of the nested agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_agent_base: Option<RuntimeRef>,
    /// Process id of the nested agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_agent_pid: Option<u32>,
    /// Native session id of the nested agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_agent_session_id: Option<String>,
    /// Native session path of the nested agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_agent_session_path: Option<String>,
    /// Captured native session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Captured native session path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_path: Option<String>,
    /// Project the session belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Current display label of the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_label: Option<String>,
    /// Whether the checkout is a linked worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_linked_worktree: Option<bool>,
    /// Source git repository, when bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    /// Branch checked out in the worktree, when bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Path to the bound worktree, when bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    /// Worktree-setup warnings.
    #[serde(default)]
    pub warnings: Vec<SessionWarning>,
    /// Owner-controlled metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// Creation timestamp.
    pub created_at: String,
    /// Last update timestamp.
    pub updated_at: String,
    /// Process exit code, when the session exited with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// A legacy logical session record with its recovery snapshot and runtime.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct SessionRecord {
    /// On-disk record schema.
    pub schema_version: u32,
    /// Stable logical session identifier.
    pub session_id: String,
    /// Desired lifecycle outcome.
    pub desired_state: DesiredState,
    /// In-progress operation, when reconciliation has work to finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<SessionTransaction>,
    /// Sanitized client-facing snapshot.
    pub info: SessionInfo,
    /// Native-recovery snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<ResumeBinding>,
    /// Last accepted native identity ordering key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_identity_ordering: Option<NativeIdentityOrdering>,
    /// Last durable worker binding.
    pub runtime: RuntimeRecord,
}

/// The last durable worker binding of a legacy session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct RuntimeRecord {
    /// Worker availability.
    pub state: RuntimeState,
    /// Stable worker identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// Stable PTY generation identity; the persisted key stays `runtime_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "runtime_id")]
    pub worker_instance_id: Option<String>,
    /// Supervisor service identifier of the worker job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
    /// Daemon-issued worker generation token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    /// Absolute worker executable named by the job definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    /// Machine-readable loss or conflict reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Durable ordering key for legacy native identity reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct NativeIdentityOrdering {
    /// Runtime generation that accepted the report; the persisted key stays
    /// `runtime_id`.
    #[serde(rename = "runtime_id")]
    pub worker_instance_id: String,
    /// Process id validated for the runtime root.
    pub pid: u32,
    /// Kernel start identity protecting against pid reuse.
    pub pid_start_identity: u64,
    /// Highest accepted monotonic sequence.
    pub sequence: u64,
}

/// A lifecycle status of a legacy worktree binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WorktreeStatus {
    /// The worktree is bound and in use.
    Active,
    /// The worktree's branch was merged.
    Merged,
    /// The worktree was cleaned up.
    Deleted,
}

/// A legacy bound worktree record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct WorktreeBinding {
    /// The pohunek session id that owns the worktree.
    pub session_id: String,
    /// Canonicalized path of the source repository.
    pub repository: PathBuf,
    /// Branch checked out in the worktree.
    pub branch: String,
    /// Base branch the worktree's branch was created from.
    pub base_branch: String,
    /// Filesystem-safe branch slug.
    pub branch_slug: String,
    /// Absolute path of the worktree directory.
    pub path: PathBuf,
    /// Agent name the worktree was bound for.
    #[serde(default)]
    pub agent: String,
    /// Lifecycle status.
    pub status: WorktreeStatus,
    /// Project this worktree belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Creation timestamp.
    pub created_at: String,
    /// Last-update timestamp.
    pub updated_at: String,
}

/// A legacy known-project record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct ProjectRecord {
    /// The git common dir, the project's identity key.
    pub git_common_dir: PathBuf,
    /// The repository's main checkout.
    pub repo_root: PathBuf,
    /// Operator-set display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_name: Option<String>,
    /// Credential-redacted origin URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
    /// Base branch for worktrees created against this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_branch: Option<String>,
    /// Whether the repository is bare.
    #[serde(default)]
    pub is_bare: bool,
    /// Whether the record was auto-registered or added explicitly.
    pub source: ProjectSource,
    /// Registration timestamp.
    pub added_at: String,
    /// Last-used timestamp.
    pub last_used_at: String,
}

/// Whether a legacy project was auto-registered or added explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProjectSource {
    /// Auto-registered at session start.
    Auto,
    /// Added explicitly by the operator.
    Manual,
}

// --- the v0.33.0 reader (suffix `330`, mirroring `git show v0.33.0:...`) ----

/// An agent kind as the v0.33.0 reader classifies it: any wire string, with
/// the reserved built-in spellings the known kinds.
///
/// Mirrors the v0.33.0 `protocol` `AgentKind`, whose `Deserialize` maps the
/// reserved spellings and keeps any other value as an unrecognized one. The
/// reader persists only known kinds, so a grammar-valid custom runtime id
/// such as `claude-sonnet` skips a store line here that the v0.33.1 reader
/// keeps. The string itself is dropped after classification; validation only
/// decides keep-or-skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AgentKind330 {
    /// Whether the value is one of the reserved built-in spellings.
    is_known_kind: bool,
}

impl AgentKind330 {
    /// Classifies a wire agent kind the v0.33.0 `AgentKind` does.
    fn from_wire(wire: &str) -> Self {
        Self {
            is_known_kind: RESERVED_AGENT_KINDS.contains(&wire),
        }
    }

    /// Whether the kind is one of the reserved built-in spellings, so a
    /// v0.33.0 record carrying it persists.
    fn is_known(self) -> bool {
        self.is_known_kind
    }
}

impl<'de> serde::Deserialize<'de> for AgentKind330 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&wire))
    }
}

/// How a v0.33.0 agent's resume invocation is shaped on the command line.
///
/// Mirrors the v0.33.0 daemon `agent::ResumeMode` (`snake_case` wire): a
/// `--resume` flag or a `resume` subcommand. A record with an unrecognized
/// spelling fails the reader's deserialization and is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ResumeMode330 {
    /// `<program> --resume <ref>` (Claude).
    Flag,
    /// `<program> resume <ref>` (Codex).
    Subcommand,
}

/// A compiled provider-native fork command shape of the v0.33.0 reader.
///
/// Mirrors the v0.33.0 daemon `agent::ForkMode` (`snake_case` wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ForkMode330 {
    /// Append `--fork-session` to a Claude Code resume operation.
    ClaudeSession,
}

/// A single line of the v0.33.0 store, internally tagged by `kind`,
/// `snake_case`.
///
/// Mirrors the v0.33.0 `Record` enum: the same variant set as the v0.33.1
/// [`Record`], wrapping the v0.33.0 session and resume types. Unknown fields
/// are ignored (the reader never denies them at this level); an unknown kind
/// fails the deserialization the reader skips.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Record330 {
    /// A durable logical session.
    Session(Box<SessionRecord330>),
    /// A native recovery binding of one logical session.
    Resume(ResumeBinding330),
    /// A bound worktree.
    Worktree(WorktreeBinding),
    /// A known project.
    Project(ProjectRecord),
}

impl ReaderSkip for Record330 {
    fn skip_reason(record: &Self) -> Option<&'static str> {
        match record {
            Record330::Session(session) if !session_agents_are_known(session) => {
                Some("the agent kind is not a known agent kind")
            }
            _ => None,
        }
    }
}

/// Whether a v0.33.0 session record keeps its agent kinds.
///
/// Mirrors the v0.33.0 reader's `record_agents_are_known`: the summary's
/// base, the active nested agent's base when present, and the recovery
/// binding's base when present must all be known `AgentKind` spellings.
fn session_agents_are_known(record: &SessionRecord330) -> bool {
    record.info.agent_base.is_known()
        && record
            .info
            .active_agent_base
            .as_ref()
            .is_none_or(|kind| kind.is_known())
        && record
            .recovery
            .as_ref()
            .is_none_or(|binding| binding.agent_base.is_known())
}

/// A v0.33.0 resume binding, whose `agent_base` is classified on load.
///
/// Mirrors the v0.33.0 `ResumeBinding`: the raw serde form below, plus the
/// v0.33.0 load rules in [`TryFrom<RawResumeBinding330>`] — a recorded
/// `agent_base` wins, otherwise only a reserved kind derives its own base,
/// and a base the reader does not know fails its `validate_persistence`,
/// which skips the line. Unlike the v0.33.1 reader, the relaunch shape is the
/// mode/fork enums ([`ResumeMode330`], [`ForkMode330`], [`SessionRefKind`])
/// rather than a native launch spec, and each spelled field must deserialize.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "RawResumeBinding330")]
pub(super) struct ResumeBinding330 {
    /// The pohunek session id.
    pub session_id: String,
    /// Owner-set display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Agent name backing the session.
    pub agent: String,
    /// Recorded or derived agent base, whose known-ness decides persistence.
    pub agent_base: AgentKind330,
    /// Working directory to relaunch in.
    pub cwd: PathBuf,
    /// Terminal width at capture time.
    pub cols: u16,
    /// Terminal height at capture time.
    pub rows: u16,
    /// Captured native session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Captured native session path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_path: Option<String>,
    /// Project this session belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Whether the session's cwd is a linked worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_linked_worktree: Option<bool>,
    /// Owner-controlled session metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// Resolved launch program, frozen at creation.
    #[serde(default)]
    pub program: String,
    /// Resolved launch args, frozen at creation.
    #[serde(default)]
    pub args: Vec<String>,
    /// Resolved input-framing rules, frozen at creation.
    #[serde(default)]
    pub input_rules: StoredInputRules,
    /// Resume argv mode frozen at creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_mode: Option<ResumeMode330>,
    /// Native-reference kind frozen at creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_kind: Option<SessionRefKind>,
    /// Whether the session resumes at all, frozen at creation.
    #[serde(default)]
    pub resumable: bool,
    /// Provider-native fork argv shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_mode: Option<ForkMode330>,
    /// Resume argv operation used by fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_resume_mode: Option<ResumeMode330>,
    /// Native-reference kind used by fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_ref_kind: Option<SessionRefKind>,
    /// Whether the session retained a native fork capability at creation.
    #[serde(default)]
    pub forkable: bool,
}

impl TryFrom<RawResumeBinding330> for ResumeBinding330 {
    type Error = String;

    fn try_from(raw: RawResumeBinding330) -> Result<Self, Self::Error> {
        // Derivation per the v0.33.0 reader: a recorded base wins; otherwise
        // only a reserved kind derives its own base, anything else fails to
        // load, skipping the line. A base the reader does not know fails its
        // `validate_persistence`, which also skips the line.
        let agent_base = if let Some(base) = raw.agent_base {
            Some(base)
        } else {
            RESERVED_AGENT_KINDS
                .contains(&raw.agent.as_str())
                .then(|| AgentKind330::from_wire(&raw.agent))
        };
        let agent_base = agent_base.ok_or_else(|| "agent_name_has_no_base".to_owned())?;
        if !agent_base.is_known() {
            return Err("the agent kind is not supported for persistence at 0.33.0".to_owned());
        }
        Ok(Self {
            session_id: raw.session_id,
            name: raw.name,
            agent: raw.agent,
            agent_base,
            cwd: raw.cwd,
            cols: raw.cols,
            rows: raw.rows,
            native_session_id: raw.native_session_id,
            native_session_path: raw.native_session_path,
            project_id: raw.project_id,
            is_linked_worktree: raw.is_linked_worktree,
            metadata: raw.metadata,
            program: raw.program,
            args: raw.args,
            input_rules: raw.input_rules,
            resume_mode: raw.resume_mode,
            ref_kind: raw.ref_kind,
            resumable: raw.resumable,
            fork_mode: raw.fork_mode,
            fork_resume_mode: raw.fork_resume_mode,
            fork_ref_kind: raw.fork_ref_kind,
            forkable: raw.forkable,
        })
    }
}

/// The serde form of a v0.33.0 resume binding.
///
/// Field for field the v0.33.0 `RawResumeBinding`, with its `serde` defaults.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct RawResumeBinding330 {
    session_id: String,
    #[serde(default)]
    name: Option<String>,
    agent: String,
    #[serde(default)]
    agent_base: Option<AgentKind330>,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    #[serde(default)]
    native_session_id: Option<String>,
    #[serde(default)]
    native_session_path: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    is_linked_worktree: Option<bool>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default)]
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    input_rules: StoredInputRules,
    #[serde(default)]
    resume_mode: Option<ResumeMode330>,
    #[serde(default)]
    ref_kind: Option<SessionRefKind>,
    #[serde(default)]
    resumable: bool,
    #[serde(default)]
    fork_mode: Option<ForkMode330>,
    #[serde(default)]
    fork_resume_mode: Option<ResumeMode330>,
    #[serde(default)]
    fork_ref_kind: Option<SessionRefKind>,
    #[serde(default)]
    forkable: bool,
}

/// A v0.33.0 logical session record with its recovery snapshot and runtime.
///
/// Mirrors the v0.33.0 `SessionRecord`: the same field set as the v0.33.1
/// [`SessionRecord`], with the summary and snapshot of the v0.33.0 reader.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct SessionRecord330 {
    /// On-disk record schema.
    pub schema_version: u32,
    /// Stable logical session identifier.
    pub session_id: String,
    /// Desired lifecycle outcome.
    pub desired_state: DesiredState,
    /// In-progress operation, when reconciliation has work to finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<SessionTransaction>,
    /// Sanitized client-facing snapshot.
    pub info: SessionInfo330,
    /// Native-recovery snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<ResumeBinding330>,
    /// Last accepted native identity ordering key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_identity_ordering: Option<NativeIdentityOrdering>,
    /// Last durable worker binding.
    pub runtime: RuntimeRecord,
}

/// Durable worker runtime information of a v0.33.0 session summary.
///
/// Mirrors the v0.33.0 `protocol` `SessionRuntime`: the persisted key for the
/// PTY generation stays `runtime_id`; v0.33.1 renamed it
/// `worker_instance_id`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct SessionRuntime330 {
    /// Availability of the runtime.
    pub state: RuntimeState,
    /// Monotonic generation, a canonical decimal wire string.
    pub runtime_generation: DecimalWire,
    /// Worker that owns the PTY, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// PTY generation identity, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_id: Option<String>,
    /// When this generation started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Latest successful daemon connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_at: Option<String>,
    /// Reason when the runtime is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_reason: Option<String>,
}

/// One provider-managed subagent of a v0.33.0 session.
///
/// Mirrors the v0.33.0 `protocol` `SubagentInfo`, whose provider is an
/// `AgentKind` rather than the v0.33.1 `RuntimeRef`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct SubagentInfo330 {
    /// Provider-native identifier.
    pub id: String,
    /// Parent provider-native identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Provider that owns the subagent.
    pub provider: AgentKind330,
    /// Provider-defined subagent type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Lifecycle state.
    pub lifecycle: SubagentLifecycle,
    /// Coarse activity while running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<AgentActivity>,
    /// Worker-owned monotonic revision, a canonical decimal wire string.
    pub revision: DecimalWire,
    /// Unix-millisecond timestamp of the accepted start hook.
    pub started_at_ms: u64,
    /// Unix-millisecond timestamp of the latest transition.
    pub updated_at_ms: u64,
    /// Unix-millisecond timestamp of the terminal transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
}

/// Sanitized client-facing v0.33.0 session snapshot.
///
/// Mirrors the v0.33.0 `protocol` `SessionInfo`: the same required fields,
/// defaults and `deny_unknown_fields` set as [`SessionInfo`], with the v0.33.0
/// `AgentKind` bases, runtime and subagent types.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct SessionInfo330 {
    /// Stable session identifier.
    pub id: String,
    /// Whether the entry is an observe-only external process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<bool>,
    /// Capabilities frozen at session start.
    #[serde(default)]
    pub capabilities: SessionCapabilities,
    /// Owner-set display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Agent profile name backing the session.
    pub agent: String,
    /// Resolved base kind backing the session, whose known-ness decides
    /// persistence.
    pub agent_base: AgentKind330,
    /// Current working directory.
    pub cwd: PathBuf,
    /// Source that last set the working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_source: Option<CwdSource>,
    /// Process id of the session root process.
    pub pid: u32,
    /// Durable worker runtime information; `None` predates worker-backed
    /// sessions or marks an observe-only entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<SessionRuntime330>,
    /// Terminal width.
    pub cols: u16,
    /// Terminal height.
    pub rows: u16,
    /// Lifecycle state.
    pub state: SessionState,
    /// Source of the state signal.
    pub state_source: StateSource,
    /// Detected agent activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<AgentActivity>,
    /// Provider-managed subagents.
    #[serde(default)]
    pub subagents: Vec<SubagentInfo330>,
    /// Active nested agent profile name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_agent: Option<String>,
    /// Resolved base kind for the nested agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_agent_base: Option<AgentKind330>,
    /// Process id of the nested agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_agent_pid: Option<u32>,
    /// Native session id of the nested agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_agent_session_id: Option<String>,
    /// Native session path of the nested agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_agent_session_path: Option<String>,
    /// Captured native session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Captured native session path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_path: Option<String>,
    /// Project the session belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Current display label of the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_label: Option<String>,
    /// Whether the checkout is a linked worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_linked_worktree: Option<bool>,
    /// Source git repository, when bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    /// Branch checked out in the worktree, when bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Path to the bound worktree, when bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    /// Worktree-setup warnings.
    #[serde(default)]
    pub warnings: Vec<SessionWarning>,
    /// Owner-controlled metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// Creation timestamp.
    pub created_at: String,
    /// Last update timestamp.
    pub updated_at: String,
    /// Process exit code, when the session exited with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

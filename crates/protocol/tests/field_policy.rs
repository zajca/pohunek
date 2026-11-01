//! Field policy of the protocol types: which types refuse a key they do not
//! know, which ignore it, and a ratchet that keeps the table complete.
//!
//! A client decodes daemon results and errors with [`protocol::tolerant`], so a
//! key added by a newer daemon must not fail the call. The types the daemon
//! decodes from a peer or from disk stay strict where they carry
//! `deny_unknown_fields`. Each row of [`ROWS`] names a type, its class and the
//! reason for it:
//!
//! - `Strict`: both decodes refuse an unknown key.
//! - `Relax`: both decodes ignore an unknown key. These types are only decoded
//!   by clients.
//! - `Lenient`: both decodes ignore an unknown key because the type carries no
//!   `deny_unknown_fields` and no hand-written refusal.
//!
//! The probe adds an extra key to a document of the type. Where a complete
//! document is cheap the row supplies it. Otherwise the key goes into a seed
//! that is incomplete: serde reports an unknown key before it reports a missing
//! field, so a refusing type answers with an error that names the key.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use pohunek_test_support::manifest_dir;
use serde_json::{json, Value};

const PROBE_KEY: &str = "__unknown_key_probe__";

const LENIENT_RESULT: &str = "result type with no deny_unknown_fields and no hand-written refusal";
const LENIENT_EVENT: &str = "event type with no deny_unknown_fields and no hand-written refusal";
const LENIENT_PARAMS: &str = "param type with no deny_unknown_fields and no hand-written refusal";
const WIRE_ONLY: &str = "decoded only by clients, never by the daemon";
const PARAMS_STRICT: &str = "client param: the daemon refuses a key it does not know";
const DUAL_USE: &str =
    "persisted by the daemon or sent as params, so a daemon decode refuses an unknown key";
const SIGNED: &str = "signed or cross-validated document: a new field changes its meaning";
const IDENTITY: &str = "identity coordinate: a new field changes the identity";
const FRAMING: &str = "envelope framing: a new key is a protocol version change";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Strict,
    Relax,
    Lenient,
}

/// How a row is probed.
#[derive(Clone, Copy)]
enum Probe {
    /// An incomplete document that carries only what selects the type (an
    /// enum tag); a refusing type answers with an error that names the key.
    Key(fn() -> Value),
    /// Complete documents that decode as they are; adding the key must break
    /// them for a strict type and leave them decoding otherwise.
    Documents(fn() -> Vec<Value>),
}

struct Row {
    name: &'static str,
    class: Class,
    /// Private `Wire*` mirror structs of the type that carry the attribute.
    mirrors: &'static [&'static str],
    reason: &'static str,
    probe: Probe,
    plain: fn(Value) -> Result<(), String>,
    tolerant: fn(Value) -> Result<(), String>,
}

fn empty_document() -> Value {
    json!({})
}

fn principal_owner() -> Value {
    json!({"kind": "principal"})
}

fn catalog_trust() -> Value {
    json!({"kind": "catalog"})
}

fn builtin_provenance() -> Value {
    json!({"kind": "builtin"})
}

macro_rules! row {
    ($ty:ident, $class:ident, $reason:expr) => {
        row!($ty, $class, [], Probe::Key(empty_document), $reason)
    };
    ($ty:ident, $class:ident, [$($mirror:literal),*], $probe:expr, $reason:expr) => {
        Row {
            name: stringify!($ty),
            class: Class::$class,
            mirrors: &[$($mirror),*],
            reason: $reason,
            probe: $probe,
            plain: |value| {
                serde_json::from_value::<protocol::$ty>(value)
                    .map(drop)
                    .map_err(|error| error.to_string())
            },
            tolerant: |value| {
                protocol::tolerant::from_value::<protocol::$ty>(value)
                    .map(drop)
                    .map_err(|error| error.to_string())
            },
        }
    };
}

fn request_bases() -> Vec<Value> {
    vec![
        json!({"v": {"minimum": 4, "maximum": 4}, "id": "r-1", "method": "daemon.health", "params": null}),
    ]
}

fn response_bases() -> Vec<Value> {
    vec![
        json!({"v": 4, "id": "r-1", "ok": {}}),
        json!({"v": 4, "id": "r-1", "err": {"class": "runtime", "code": "x", "msg": "m"}}),
    ]
}

fn version_range_bases() -> Vec<Value> {
    vec![json!({"minimum": 4, "maximum": 4})]
}

fn protocol_error_bases() -> Vec<Value> {
    vec![json!({"class": "runtime", "code": "x", "msg": "m"})]
}

fn session_runtime_identity_bases() -> Vec<Value> {
    vec![json!({"worker_instance_id": "runtime-1", "runtime_generation": "1"})]
}

fn terminal_cursor_bases() -> Vec<Value> {
    vec![json!({"row": 1, "col": 2, "visible": true})]
}

fn output_gap_bases() -> Vec<Value> {
    vec![json!({"start_offset": "0", "end_offset": "10"})]
}

fn session_read_result_bases() -> Vec<Value> {
    vec![json!({
        "text": "one", "source_used": "visible",
        "worker_instance_id": "runtime-1", "runtime_generation": "1",
        "revision": "9", "alternate_screen": false, "lines_requested": 20, "truncated": false,
    })]
}

fn session_screen_result_bases() -> Vec<Value> {
    vec![json!({
        "session_id": "s-1", "worker_id": "worker-1",
        "worker_instance_id": "runtime-1", "runtime_generation": "1",
        "watermark": "9", "dimensions": {"cols": 120, "rows": 40},
        "cursor": {"row": 1, "col": 2, "visible": true},
        "alternate_screen": false, "visible_lines": ["one"],
    })]
}

fn session_output_result_bases() -> Vec<Value> {
    vec![json!({
        "session_id": "s-1",
        "worker_instance_id": "runtime-1", "runtime_generation": "1",
        "history_start_offset": "10", "start_offset": "10", "next_offset": "10",
        "runtime_end_offset": "10", "data_base64": "",
        "gap": {"start_offset": "0", "end_offset": "10"},
        "has_more": false, "timed_out": false,
    })]
}

fn session_info_document() -> Value {
    json!({
        "id": "s-1", "agent": "codex", "agent_base": "codex",
        "capabilities": {"fork": true, "resume": true},
        "cwd": "/workspace/pohunek", "pid": 42424, "state": "running",
        "state_source": "process", "cols": 80, "rows": 24,
        "created_at": "2026-07-08T00:00:00Z", "updated_at": "2026-07-08T00:01:00Z",
    })
}

fn session_wait_result_bases() -> Vec<Value> {
    vec![json!({"reason": "timeout", "session": session_info_document()})]
}

fn session_detection_result_bases() -> Vec<Value> {
    vec![json!({
        "session_id": "s-1", "supported_regions": ["osc_title"],
        "previews": [{"kind": "osc_title", "region": "osc_title", "text": "t"}],
    })]
}

fn detection_region_preview_bases() -> Vec<Value> {
    vec![json!({"kind": "osc_title", "region": "osc_title", "text": "t"})]
}

fn report_native_id_result_bases() -> Vec<Value> {
    vec![json!({"recorded": true})]
}

fn unconfirmed_process_bases() -> Vec<Value> {
    vec![json!({"pid": 42, "start_identity": "7"})]
}

fn turn_event_document() -> Value {
    json!({"session_id": "s-1", "kind": "turn_completed", "at": "2026-08-04T12:00:00Z"})
}

fn turn_progress_document() -> Value {
    json!({"session_id": "s-1", "state": "running", "output_advanced": true})
}

fn session_wait_turn_result_bases() -> Vec<Value> {
    vec![json!({
        "reason": "event", "event": turn_event_document(),
        "cursor": "t1.0.1.d-fixture", "progress": [turn_progress_document()],
    })]
}

fn session_turn_event_bases() -> Vec<Value> {
    vec![turn_event_document()]
}

fn session_turn_progress_bases() -> Vec<Value> {
    vec![turn_progress_document()]
}

fn session_policy_result_bases() -> Vec<Value> {
    vec![json!({"retention": protocol::SessionRetentionPolicy::default()})]
}

fn notification_policy_result_bases() -> Vec<Value> {
    vec![json!({"policy": {
        "attention_dedupe_window_secs": 90,
        "enabled": {
            "agent_blocked": true, "approval_required": true, "turn_completed": true,
            "session_finished": true, "error": true, "system": true,
        },
    }})]
}

const ROWS: &[Row] = &[
    // Relax: decoded only by clients.
    row!(
        ProtocolError,
        Relax,
        [],
        Probe::Documents(protocol_error_bases),
        WIRE_ONLY
    ),
    row!(
        SessionWaitResult,
        Relax,
        [],
        Probe::Documents(session_wait_result_bases),
        WIRE_ONLY
    ),
    row!(
        SessionReadResult,
        Relax,
        [],
        Probe::Documents(session_read_result_bases),
        WIRE_ONLY
    ),
    row!(
        SessionScreenResult,
        Relax,
        [],
        Probe::Documents(session_screen_result_bases),
        WIRE_ONLY
    ),
    row!(
        TerminalCursor,
        Relax,
        [],
        Probe::Documents(terminal_cursor_bases),
        WIRE_ONLY
    ),
    row!(
        SessionRuntimeIdentity,
        Relax,
        [],
        Probe::Documents(session_runtime_identity_bases),
        WIRE_ONLY
    ),
    row!(
        SessionOutputResult,
        Relax,
        [],
        Probe::Documents(session_output_result_bases),
        WIRE_ONLY
    ),
    row!(
        SessionOutputGap,
        Relax,
        [],
        Probe::Documents(output_gap_bases),
        WIRE_ONLY
    ),
    row!(
        SessionDetectionResult,
        Relax,
        [],
        Probe::Documents(session_detection_result_bases),
        WIRE_ONLY
    ),
    row!(
        DetectionRegionPreview,
        Relax,
        [],
        Probe::Documents(detection_region_preview_bases),
        WIRE_ONLY
    ),
    row!(
        SessionReportNativeIdResult,
        Relax,
        [],
        Probe::Documents(report_native_id_result_bases),
        WIRE_ONLY
    ),
    row!(
        UnconfirmedProcess,
        Relax,
        [],
        Probe::Documents(unconfirmed_process_bases),
        WIRE_ONLY
    ),
    row!(
        SessionWaitTurnResult,
        Relax,
        [],
        Probe::Documents(session_wait_turn_result_bases),
        WIRE_ONLY
    ),
    row!(
        SessionTurnEvent,
        Relax,
        [],
        Probe::Documents(session_turn_event_bases),
        WIRE_ONLY
    ),
    row!(
        SessionTurnProgress,
        Relax,
        [],
        Probe::Documents(session_turn_progress_bases),
        WIRE_ONLY
    ),
    row!(
        SessionPolicyResult,
        Relax,
        [],
        Probe::Documents(session_policy_result_bases),
        WIRE_ONLY
    ),
    row!(
        NotificationPolicyResult,
        Relax,
        [],
        Probe::Documents(notification_policy_result_bases),
        WIRE_ONLY
    ),
    // Strict: envelope framing.
    row!(
        Request,
        Strict,
        ["WireRequest"],
        Probe::Documents(request_bases),
        FRAMING
    ),
    row!(
        Response,
        Strict,
        ["WireOk", "WireErr"],
        Probe::Documents(response_bases),
        FRAMING
    ),
    row!(
        ProtocolVersionRange,
        Strict,
        ["WireRange"],
        Probe::Documents(version_range_bases),
        FRAMING
    ),
    // Strict: persisted or sent as params as well as returned.
    row!(SessionCapabilities, Strict, DUAL_USE),
    row!(SessionRetentionPolicy, Strict, DUAL_USE),
    row!(NotificationPolicy, Strict, DUAL_USE),
    row!(NotificationRetentionPolicy, Strict, DUAL_USE),
    row!(NotificationKindPolicy, Strict, DUAL_USE),
    // Strict: signed, cross-validated or identity documents.
    row!(HostGovernanceStatus, Strict, SIGNED),
    row!(
        EnrollmentInfo,
        Strict,
        ["WireEnrollmentInfo"],
        Probe::Key(empty_document),
        SIGNED
    ),
    row!(
        HostOwner,
        Strict,
        ["WireOwner"],
        Probe::Key(principal_owner),
        SIGNED
    ),
    row!(
        TransferProposal,
        Strict,
        ["WireProposal"],
        Probe::Key(empty_document),
        SIGNED
    ),
    row!(
        TransferOutcomeCandidate,
        Strict,
        ["WireCandidate"],
        Probe::Key(empty_document),
        SIGNED
    ),
    row!(
        SignedTransferOutcome,
        Strict,
        ["WireOutcome"],
        Probe::Key(empty_document),
        SIGNED
    ),
    row!(PackageIdentity, Strict, IDENTITY),
    row!(LaunchBinding, Strict, IDENTITY),
    row!(
        BindingProvenance,
        Strict,
        [],
        Probe::Key(builtin_provenance),
        IDENTITY
    ),
    row!(
        PackageTrust,
        Strict,
        [],
        Probe::Key(catalog_trust),
        PARAMS_STRICT
    ),
    // Strict: client params.
    row!(CommandRunParams, Strict, PARAMS_STRICT),
    row!(SessionIdleStopParams, Strict, PARAMS_STRICT),
    row!(IntegrationInstallParams, Strict, PARAMS_STRICT),
    row!(IntegrationStatusParams, Strict, PARAMS_STRICT),
    row!(IntegrationUninstallParams, Strict, PARAMS_STRICT),
    row!(IntegrationDoctorParams, Strict, PARAMS_STRICT),
    row!(NotificationPolicyParams, Strict, PARAMS_STRICT),
    row!(PackageInstallParams, Strict, PARAMS_STRICT),
    row!(PackageLinkParams, Strict, PARAMS_STRICT),
    row!(PackageSetEnabledParams, Strict, PARAMS_STRICT),
    row!(PackageSelectParams, Strict, PARAMS_STRICT),
    row!(PackageUninstallParams, Strict, PARAMS_STRICT),
    row!(PackageBindProfileParams, Strict, PARAMS_STRICT),
    row!(PackageInspectParams, Strict, PARAMS_STRICT),
    row!(PackageDoctorParams, Strict, PARAMS_STRICT),
    row!(SessionScreenParams, Strict, PARAMS_STRICT),
    row!(SessionDetectionParams, Strict, PARAMS_STRICT),
    row!(
        SessionReadParams,
        Strict,
        ["WireParams"],
        Probe::Key(empty_document),
        PARAMS_STRICT
    ),
    row!(
        SessionOutputParams,
        Strict,
        ["WireParams"],
        Probe::Key(empty_document),
        PARAMS_STRICT
    ),
    row!(
        SessionWaitParams,
        Strict,
        ["WireParams"],
        Probe::Key(empty_document),
        PARAMS_STRICT
    ),
    row!(
        SessionReportNativeIdParams,
        Strict,
        ["WireParams"],
        Probe::Key(empty_document),
        PARAMS_STRICT
    ),
    row!(
        SessionWaitTurnParams,
        Strict,
        ["WireParams"],
        Probe::Key(empty_document),
        PARAMS_STRICT
    ),
    row!(SessionPolicyParams, Strict, PARAMS_STRICT),
    row!(StoreRejectedSetAsideParams, Strict, PARAMS_STRICT),
    // Lenient: every other param, result and event type.
    row!(AgentStateEvent, Lenient, LENIENT_EVENT),
    row!(AssistantMaterializeParams, Lenient, LENIENT_PARAMS),
    row!(AssistantMaterializeResult, Lenient, LENIENT_RESULT),
    row!(AttachEvent, Lenient, LENIENT_EVENT),
    row!(BackgroundStateEvent, Lenient, LENIENT_EVENT),
    row!(CommandRunResult, Lenient, LENIENT_RESULT),
    row!(DaemonDoctorResult, Lenient, LENIENT_RESULT),
    row!(DaemonHealthResult, Lenient, LENIENT_RESULT),
    row!(Event, Lenient, LENIENT_EVENT),
    row!(HostDiscoverParams, Lenient, LENIENT_PARAMS),
    row!(HostInspectParams, Lenient, LENIENT_PARAMS),
    row!(HostResolveParams, Lenient, LENIENT_PARAMS),
    row!(IntegrationDoctorResult, Lenient, LENIENT_RESULT),
    row!(IntegrationInstallResult, Lenient, LENIENT_RESULT),
    row!(IntegrationStatusResult, Lenient, LENIENT_RESULT),
    row!(IntegrationUninstallResult, Lenient, LENIENT_RESULT),
    row!(NotificationCreateParams, Lenient, LENIENT_PARAMS),
    row!(NotificationCreateResult, Lenient, LENIENT_RESULT),
    row!(NotificationCreatedEvent, Lenient, LENIENT_EVENT),
    row!(NotificationDeleteParams, Lenient, LENIENT_PARAMS),
    row!(NotificationDeleteResult, Lenient, LENIENT_RESULT),
    row!(NotificationDeletedEvent, Lenient, LENIENT_EVENT),
    row!(NotificationListParams, Lenient, LENIENT_PARAMS),
    row!(NotificationListResult, Lenient, LENIENT_RESULT),
    row!(NotificationRetentionParams, Lenient, LENIENT_PARAMS),
    row!(NotificationRetentionResult, Lenient, LENIENT_RESULT),
    row!(NotificationUpdateParams, Lenient, LENIENT_PARAMS),
    row!(NotificationUpdateResult, Lenient, LENIENT_RESULT),
    row!(NotificationUpdatedEvent, Lenient, LENIENT_EVENT),
    row!(PackageBindProfileResult, Lenient, LENIENT_RESULT),
    row!(PackageChangeResult, Lenient, LENIENT_RESULT),
    row!(PackageDoctorResult, Lenient, LENIENT_RESULT),
    row!(PackageInspectResult, Lenient, LENIENT_RESULT),
    row!(PackageInstallResult, Lenient, LENIENT_RESULT),
    row!(PackageListResult, Lenient, LENIENT_RESULT),
    row!(PackageUninstallResult, Lenient, LENIENT_RESULT),
    row!(ProjectActionParams, Lenient, LENIENT_PARAMS),
    row!(ProjectActionResult, Lenient, LENIENT_RESULT),
    row!(ProjectActionsParams, Lenient, LENIENT_PARAMS),
    row!(ProjectActionsResult, Lenient, LENIENT_RESULT),
    row!(ProjectAddParams, Lenient, LENIENT_PARAMS),
    row!(ProjectListParams, Lenient, LENIENT_PARAMS),
    row!(ProjectPromptParams, Lenient, LENIENT_PARAMS),
    row!(ProjectPromptResult, Lenient, LENIENT_RESULT),
    row!(ProjectRemoveParams, Lenient, LENIENT_PARAMS),
    row!(ProjectRemoveResult, Lenient, LENIENT_RESULT),
    row!(ProjectRenameParams, Lenient, LENIENT_PARAMS),
    row!(ProjectShowParams, Lenient, LENIENT_PARAMS),
    row!(ProjectShowResult, Lenient, LENIENT_RESULT),
    row!(RuntimeInventoryEvent, Lenient, LENIENT_EVENT),
    row!(RuntimeInventoryResult, Lenient, LENIENT_RESULT),
    row!(SessionAttachParams, Lenient, LENIENT_PARAMS),
    row!(SessionAttachResult, Lenient, LENIENT_RESULT),
    row!(SessionDetachParams, Lenient, LENIENT_PARAMS),
    row!(SessionDetachResult, Lenient, LENIENT_RESULT),
    row!(SessionDiffParams, Lenient, LENIENT_PARAMS),
    row!(SessionDiffResult, Lenient, LENIENT_RESULT),
    row!(SessionEvent, Lenient, LENIENT_EVENT),
    row!(SessionForkParams, Lenient, LENIENT_PARAMS),
    row!(SessionForkResult, Lenient, LENIENT_RESULT),
    row!(SessionIdleStopResult, Lenient, LENIENT_RESULT),
    row!(SessionInputParams, Lenient, LENIENT_PARAMS),
    row!(SessionInputResult, Lenient, LENIENT_RESULT),
    row!(SessionListParams, Lenient, LENIENT_PARAMS),
    row!(SessionNativeRecoveredEvent, Lenient, LENIENT_EVENT),
    row!(SessionNewParams, Lenient, LENIENT_PARAMS),
    row!(SessionNewResult, Lenient, LENIENT_RESULT),
    row!(SessionReleaseAgentParams, Lenient, LENIENT_PARAMS),
    row!(SessionReleaseAgentResult, Lenient, LENIENT_RESULT),
    row!(SessionRemoveResult, Lenient, LENIENT_RESULT),
    row!(SessionRenameParams, Lenient, LENIENT_PARAMS),
    row!(SessionRenameResult, Lenient, LENIENT_RESULT),
    row!(SessionReportAgentParams, Lenient, LENIENT_PARAMS),
    row!(SessionReportAgentResult, Lenient, LENIENT_RESULT),
    row!(SessionResizeParams, Lenient, LENIENT_PARAMS),
    row!(SessionResizeResult, Lenient, LENIENT_RESULT),
    row!(SessionResumeParams, Lenient, LENIENT_PARAMS),
    row!(SessionResumeResult, Lenient, LENIENT_RESULT),
    row!(SessionRetentionParams, Lenient, LENIENT_PARAMS),
    row!(SessionRetentionResult, Lenient, LENIENT_RESULT),
    row!(SessionSetMetadataParams, Lenient, LENIENT_PARAMS),
    row!(SessionSetMetadataResult, Lenient, LENIENT_RESULT),
    row!(SessionStopResult, Lenient, LENIENT_RESULT),
    row!(StoreRejectedListResult, Lenient, LENIENT_RESULT),
    row!(StoreRejectedSetAsideResult, Lenient, LENIENT_RESULT),
    row!(SubagentStateEvent, Lenient, LENIENT_EVENT),
    row!(WorktreeRemoveParams, Lenient, LENIENT_PARAMS),
    row!(WorktreeRemoveResult, Lenient, LENIENT_RESULT),
];

fn with_probe_key(mut value: Value) -> Value {
    value[PROBE_KEY] = json!(1);
    value
}

/// Whether `row` refuses an unknown key under one decoder.
fn refuses(row: &Row, decode: fn(Value) -> Result<(), String>) -> bool {
    match row.probe {
        Probe::Key(seed) => decode(with_probe_key(seed()))
            .as_ref()
            .is_err_and(|message| message.contains(PROBE_KEY)),
        Probe::Documents(documents) => {
            let mut refused = Vec::new();
            for base in documents() {
                decode(base.clone()).unwrap_or_else(|error| {
                    panic!(
                        "{}: the base document must decode: {base}: {error}",
                        row.name
                    )
                });
                refused.push(decode(with_probe_key(base)).is_err());
            }
            assert!(
                !refused.is_empty() && refused.windows(2).all(|pair| pair[0] == pair[1]),
                "{}: the documents must agree",
                row.name
            );
            refused[0]
        }
    }
}

#[test]
fn every_row_behaves_as_its_class_says() {
    for row in ROWS {
        let expected = row.class == Class::Strict;
        assert_eq!(
            refuses(row, row.plain),
            expected,
            "{} ({:?}: {}): plain decode",
            row.name,
            row.class,
            row.reason
        );
        assert_eq!(
            refuses(row, row.tolerant),
            expected,
            "{} ({:?}: {}): tolerant decode",
            row.name,
            row.class,
            row.reason
        );
    }
}

#[test]
fn rows_are_unique() {
    let mut seen = BTreeSet::new();
    for row in ROWS {
        assert!(seen.insert(row.name), "{} appears twice", row.name);
    }
}

/// One source file of the protocol crate with comments dropped.
struct Source {
    file: String,
    code: String,
}

fn collect_sources(root: &Path, directory: &Path, sources: &mut Vec<Source>) {
    for entry in fs::read_dir(directory).expect("read the protocol sources") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect_sources(root, &path, sources);
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read a protocol source");
        let code = text
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("//") {
                    ""
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        sources.push(Source {
            file: path
                .strip_prefix(root)
                .expect("a source below the root")
                .display()
                .to_string(),
            code,
        });
    }
}

fn sources() -> Vec<Source> {
    let root = manifest_dir().join("src");
    let mut sources = Vec::new();
    collect_sources(&root, &root, &mut sources);
    sources.sort_by(|left, right| left.file.cmp(&right.file));
    sources
}

/// The identifier that follows `keyword` (`struct` or `enum`) in `text`.
fn declared_name(text: &str) -> Option<String> {
    let mut words =
        text.split(|character: char| !(character.is_alphanumeric() || character == '_'));
    while let Some(word) = words.next() {
        if word == "struct" || word == "enum" {
            return words.next().map(str::to_owned);
        }
    }
    None
}

/// Every `deny_unknown_fields` site: the type that carries it.
fn denying_sites(sources: &[Source]) -> Vec<(String, String)> {
    let mut sites = Vec::new();
    for source in sources {
        let lines: Vec<&str> = source.code.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if !line.contains("deny_unknown_fields") {
                continue;
            }
            let following = lines[index + 1..lines.len().min(index + 16)].join("\n");
            let name = declared_name(&following).unwrap_or_else(|| {
                panic!(
                    "{}:{}: no type follows deny_unknown_fields",
                    source.file,
                    index + 1
                )
            });
            sites.push((source.file.clone(), name));
        }
    }
    sites
}

/// Every hand-written refusal of an unknown key: the type whose
/// `Deserialize` implementation raises it.
fn hand_written_refusals(sources: &[Source]) -> Vec<(String, String)> {
    let mut refusals = Vec::new();
    for source in sources {
        let lines: Vec<&str> = source.code.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if !line.contains("unknown_field(") {
                continue;
            }
            let owner = lines[..index]
                .iter()
                .rev()
                .find_map(|candidate| {
                    candidate
                        .split_once("Deserialize<'de> for ")
                        .map(|(_, rest)| rest.split_whitespace().next().unwrap_or("").to_owned())
                })
                .unwrap_or_else(|| {
                    panic!("{}:{}: no Deserialize impl above", source.file, index + 1)
                });
            refusals.push((source.file.clone(), owner));
        }
    }
    refusals
}

/// Public `*Params`, `*Result` and `*Event` types that implement `Deserialize`.
fn payload_types(sources: &[Source]) -> Vec<String> {
    let mut names = Vec::new();
    for source in sources {
        let lines: Vec<&str> = source.code.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed
                .strip_prefix("pub struct ")
                .or_else(|| trimmed.strip_prefix("pub enum "))
            else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|character| character.is_alphanumeric() || *character == '_')
                .collect();
            if !["Params", "Result", "Event"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
            {
                continue;
            }
            let attributes = lines[..index]
                .iter()
                .rev()
                .take_while(|candidate| {
                    let candidate = candidate.trim();
                    candidate.starts_with('#')
                        || candidate.starts_with(')')
                        || candidate.ends_with(',')
                        || candidate.ends_with('(')
                })
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            let manual = format!("Deserialize<'de> for {name}");
            if attributes.contains("Deserialize") || source.code.contains(&manual) {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[test]
fn every_denying_site_and_payload_type_is_in_the_table() {
    let sources = sources();
    let table: BTreeMap<&str, &Row> = ROWS.iter().map(|row| (row.name, row)).collect();

    let mut expected_mirrors: BTreeMap<&str, usize> = BTreeMap::new();
    for row in ROWS {
        for mirror in row.mirrors {
            *expected_mirrors.entry(mirror).or_default() += 1;
        }
    }
    let mut found_mirrors: BTreeMap<String, usize> = BTreeMap::new();
    for (file, name) in denying_sites(&sources) {
        match table.get(name.as_str()) {
            Some(row) => assert_eq!(
                row.class,
                Class::Strict,
                "{file}: {name} carries deny_unknown_fields but its row is {:?}",
                row.class
            ),
            None => *found_mirrors.entry(name).or_default() += 1,
        }
    }
    let found_mirrors: BTreeMap<&str, usize> = found_mirrors
        .iter()
        .map(|(name, count)| (name.as_str(), *count))
        .collect();
    assert_eq!(
        found_mirrors, expected_mirrors,
        "every deny_unknown_fields site must be a table row or a named mirror of one"
    );

    for (file, owner) in hand_written_refusals(&sources) {
        let row = table
            .get(owner.as_str())
            .unwrap_or_else(|| panic!("{file}: {owner} refuses unknown keys and has no row"));
        assert_eq!(row.class, Class::Strict, "{file}: {owner}");
    }

    let missing: Vec<String> = payload_types(&sources)
        .into_iter()
        .filter(|name| !table.contains_key(name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "payload types without a field-policy row: {missing:?}"
    );
}

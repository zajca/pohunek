//! Protocol 3 edge adapter against golden fixtures recorded from release v0.33.0.
//!
//! The fixtures in `tests/fixtures/compat/v3/` were serialized by the v0.33.0
//! crate itself (see `generator/golden_v3.rs.txt`). The oracle for the expected
//! current shape is independent of the adapter: a blind key rename of the whole
//! fixture. The adapter must agree with it on every fixture, and the current
//! typed (strict) decode of the renamed fixture must reproduce it exactly, so a
//! non-additive change to a current type shows up here as a failing fixture.
//!
//! A client of this build talking to a protocol 3 daemon runs the same adapter
//! the other way round: the golden results and events are what that daemon
//! sends, the golden requests are what it accepts.

use protocol::compat::{
    downgrade_request_params, event_payload, introduced_methods, request_params, result,
    upgrade_event_payload, upgrade_result,
};
use protocol::method::{self, Method};
use protocol::{
    event, AgentStateEvent, AttachEvent, NotificationCreatedEvent, NotificationDeletedEvent,
    NotificationUpdatedEvent, ProtocolVersion, RuntimeInventoryEvent, SessionEvent,
    SessionNativeRecoveredEvent, SessionWarning, SessionWarningKind, SubagentStateEvent,
    MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

const REQUESTS: &str = include_str!("fixtures/compat/v3/requests.json");
const RESULTS: &str = include_str!("fixtures/compat/v3/results.json");
const EVENTS: &str = include_str!("fixtures/compat/v3/events.json");

/// Keys protocol 3 spelled differently, as `(protocol 3, current)`.
const RENAMED_KEYS: &[(&str, &str)] = &[
    ("runtime_id", "worker_instance_id"),
    ("previous_runtime_id", "previous_worker_instance_id"),
];

fn v3() -> ProtocolVersion {
    ProtocolVersion::new(3).expect("nonzero version")
}

struct Fixtures {
    entries: Vec<Value>,
}

fn fixtures(raw: &str, file: &str) -> Fixtures {
    let document: Value = serde_json::from_str(raw).expect("fixture file is valid JSON");
    assert_eq!(document["release"], "v0.33.0", "{file}");
    assert_eq!(document["protocol_version"], 3, "{file}");
    let Value::Array(entries) = document["entries"].clone() else {
        panic!("{file}: entries must be an array");
    };
    Fixtures { entries }
}

/// Renames protocol 3 object keys to their current spelling, everywhere.
fn rename_keys_everywhere(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, inner)| {
                    let renamed = RENAMED_KEYS
                        .iter()
                        .find(|(legacy, _)| legacy == key)
                        .map_or(key.as_str(), |(_, current)| current);
                    (renamed.to_owned(), rename_keys_everywhere(inner))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(rename_keys_everywhere).collect()),
        other => other.clone(),
    }
}

fn contains_key(value: &Value, wanted: &str) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key(wanted) || object.values().any(|inner| contains_key(inner, wanted))
        }
        Value::Array(items) => items.iter().any(|inner| contains_key(inner, wanted)),
        _ => false,
    }
}

fn roundtrip<T: Serialize + DeserializeOwned>(value: &Value) -> Result<Value, String> {
    let typed: T = serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    serde_json::to_value(&typed).map_err(|error| error.to_string())
}

type Roundtrip = fn(&Value) -> Result<Value, String>;

fn params_roundtrip<M: Method>() -> Roundtrip
where
    M::Params: Serialize + DeserializeOwned,
{
    roundtrip::<M::Params>
}

fn output_roundtrip<M: Method>() -> Roundtrip
where
    M::Output: Serialize + DeserializeOwned,
{
    roundtrip::<M::Output>
}

/// `(method name, current params decode, current result decode)` for every method.
fn method_table() -> Vec<(&'static str, Roundtrip, Roundtrip)> {
    macro_rules! table {
        ($($marker:ident),+ $(,)?) => {
            vec![$((
                method::$marker::NAME,
                params_roundtrip::<method::$marker>(),
                output_roundtrip::<method::$marker>(),
            )),+]
        };
    }
    table!(
        DaemonHealth,
        HostGovernanceInspect,
        SessionNew,
        SessionList,
        SessionRuntimeInventory,
        SessionInspect,
        SessionStop,
        SessionResume,
        SessionFork,
        SessionRemove,
        SessionRemoveAcceptingUnconfirmed,
        SessionAttach,
        SessionDetach,
        SessionResize,
        SessionInput,
        SessionScreen,
        SessionDetection,
        SessionOutput,
        SessionRead,
        SessionWait,
        Subscribe,
        SessionReportNativeId,
        SessionReportAgent,
        SessionReleaseAgent,
        SessionSetMetadata,
        SessionRename,
        SessionDiff,
        SessionPolicyGet,
        SessionPolicySet,
        SessionRetentionSweep,
        IntegrationInstall,
        IntegrationStatus,
        IntegrationUninstall,
        IntegrationDoctor,
        HostInspect,
        AssistantMaterialize,
        DaemonDoctor,
        HostDiscover,
        NotificationCreate,
        NotificationList,
        NotificationUpdate,
        NotificationDelete,
        NotificationPolicyGet,
        NotificationPolicySet,
        NotificationRetentionPrune,
        ProjectList,
        ProjectAdd,
        ProjectShow,
        ProjectRename,
        ProjectRemove,
        ProjectPrompt,
        ProjectAction,
        ProjectActions,
        WorktreeRemove,
    )
}

/// `(event name, current payload decode)` for every event.
fn event_table() -> Vec<(&'static str, Roundtrip)> {
    vec![
        (event::AGENT_STATE, roundtrip::<AgentStateEvent>),
        (event::SUBAGENT_STATE, roundtrip::<SubagentStateEvent>),
        (event::ATTACH_OPENED, roundtrip::<AttachEvent>),
        (event::ATTACH_CLOSED, roundtrip::<AttachEvent>),
        (
            event::NOTIFICATION_CREATED,
            roundtrip::<NotificationCreatedEvent>,
        ),
        (
            event::NOTIFICATION_UPDATED,
            roundtrip::<NotificationUpdatedEvent>,
        ),
        (
            event::NOTIFICATION_DELETED,
            roundtrip::<NotificationDeletedEvent>,
        ),
        (event::SESSION_CREATED, roundtrip::<SessionEvent>),
        (event::SESSION_UPDATED, roundtrip::<SessionEvent>),
        (event::SESSION_STOPPED, roundtrip::<SessionEvent>),
        (event::SESSION_REMOVED, roundtrip::<SessionEvent>),
        (
            event::SESSION_RUNTIME_RECONNECTED,
            roundtrip::<SessionEvent>,
        ),
        (event::SESSION_RUNTIME_LOST, roundtrip::<SessionEvent>),
        (event::SESSION_RUNTIME_CONFLICT, roundtrip::<SessionEvent>),
        (
            event::SESSION_RUNTIME_DISCOVERED,
            roundtrip::<RuntimeInventoryEvent>,
        ),
        (
            event::SESSION_NATIVE_RECOVERED,
            roundtrip::<SessionNativeRecoveredEvent>,
        ),
    ]
}

fn entry_name<'a>(entry: &'a Value, key: &str) -> &'a str {
    entry[key]
        .as_str()
        .expect("fixture entry names its subject")
}

#[test]
fn the_window_reaches_the_release_the_fixtures_come_from() {
    assert_eq!(MIN_PROTOCOL_VERSION, v3());
    assert_eq!(PROTOCOL_VERSION.get(), v3().get() + 1);
}

#[test]
fn fixtures_cover_every_current_method_and_event() {
    let requests = fixtures(REQUESTS, "requests.json");
    let results = fixtures(RESULTS, "results.json");
    let events = fixtures(EVENTS, "events.json");
    for spec in method::METHOD_SPECS {
        if introduced_methods(v3()).contains(&spec.name) {
            continue;
        }
        for (kind, file) in [("request", &requests), ("result", &results)] {
            assert!(
                file.entries
                    .iter()
                    .any(|entry| entry["method"] == spec.name),
                "no protocol 3 {kind} fixture for method {}",
                spec.name
            );
        }
    }
    for spec in event::EVENT_SPECS {
        assert!(
            events
                .entries
                .iter()
                .any(|entry| entry["event"] == spec.name),
            "no protocol 3 fixture for event {}",
            spec.name
        );
    }
    for entry in requests.entries.iter().chain(&results.entries) {
        let name = entry_name(entry, "method");
        assert!(
            method::METHOD_SPECS.iter().any(|spec| spec.name == name),
            "fixture method {name} no longer exists; removing a method needs a MIN_PROTOCOL_VERSION bump"
        );
    }
    for entry in &events.entries {
        let name = entry_name(entry, "event");
        assert!(
            event::EVENT_SPECS.iter().any(|spec| spec.name == name),
            "fixture event {name} no longer exists; removing an event needs a MIN_PROTOCOL_VERSION bump"
        );
    }
}

#[test]
fn methods_added_after_protocol_3_are_listed_exactly_and_have_no_fixture() {
    let introduced = introduced_methods(v3());
    assert!(!introduced.is_empty());
    let requests = fixtures(REQUESTS, "requests.json");
    let results = fixtures(RESULTS, "results.json");
    let fixture_methods: Vec<&str> = requests
        .entries
        .iter()
        .chain(&results.entries)
        .map(|entry| entry_name(entry, "method"))
        .collect();
    for name in introduced {
        assert!(
            method::METHOD_SPECS.iter().any(|spec| spec.name == *name),
            "{name} is not a current method"
        );
        assert!(
            !fixture_methods.contains(name),
            "{name} has a protocol 3 fixture, so it is not new in protocol 4"
        );
        assert!(request_params(v3(), name, json!({})).is_err(), "{name}");
        assert!(result(v3(), name, json!({})).is_err(), "{name}");
    }
}

#[test]
fn the_method_and_event_tables_match_the_registries() {
    let mut methods: Vec<&str> = method_table().iter().map(|(name, ..)| *name).collect();
    let mut registered: Vec<&str> = method::METHOD_SPECS
        .iter()
        .map(|spec| spec.name)
        .filter(|name| !introduced_methods(v3()).contains(name))
        .collect();
    methods.sort_unstable();
    registered.sort_unstable();
    assert_eq!(
        methods, registered,
        "update method_table() with the registry, or list a method added after protocol 3 in compat::v3::INTRODUCED_METHODS"
    );
    let mut events: Vec<&str> = event_table().iter().map(|(name, _)| *name).collect();
    let mut registered: Vec<&str> = event::EVENT_SPECS.iter().map(|spec| spec.name).collect();
    events.sort_unstable();
    registered.sort_unstable();
    assert_eq!(events, registered, "update event_table() with the registry");
}

#[test]
fn protocol_3_requests_upgrade_to_what_the_current_types_decode() {
    let table = method_table();
    let mut renamed = 0;
    for entry in &fixtures(REQUESTS, "requests.json").entries {
        let name = entry_name(entry, "method");
        let golden = &entry["params"];
        let upgraded = request_params(v3(), name, golden.clone())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(upgraded, rename_keys_everywhere(golden), "{name}");
        let (_, decode, _) = table
            .iter()
            .find(|(candidate, ..)| *candidate == name)
            .expect("method table covers the fixture");
        assert_eq!(
            decode(&upgraded).unwrap_or_else(|error| panic!("{name}: {error}")),
            upgraded,
            "{name}: strict current decode must reproduce the upgraded request"
        );
        if upgraded != *golden {
            renamed += 1;
        }
    }
    assert!(
        renamed >= 3,
        "output, wait and native-id requests carry the identity"
    );
}

#[test]
fn current_results_downgrade_to_the_protocol_3_golden() {
    let table = method_table();
    let mut renamed = 0;
    for entry in &fixtures(RESULTS, "results.json").entries {
        let name = entry_name(entry, "method");
        let golden = &entry["result"];
        let current = rename_keys_everywhere(golden);
        let (_, _, decode) = table
            .iter()
            .find(|(candidate, ..)| *candidate == name)
            .expect("method table covers the fixture");
        let reserialized = decode(&current).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            reserialized, current,
            "{name}: strict current decode must reproduce the renamed fixture"
        );
        let downgraded =
            result(v3(), name, reserialized).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(&downgraded, golden, "{name}");
        for (_, current_key) in RENAMED_KEYS {
            assert!(
                !contains_key(&downgraded, current_key),
                "{name}: `{current_key}` leaked into a protocol 3 result"
            );
        }
        if current != *golden {
            renamed += 1;
        }
    }
    assert!(
        renamed >= 8,
        "every session-bearing method carries the identity"
    );
}

#[test]
fn current_events_downgrade_to_the_protocol_3_golden() {
    let table = event_table();
    let mut renamed = 0;
    for entry in &fixtures(EVENTS, "events.json").entries {
        let name = entry_name(entry, "event");
        let golden = &entry["payload"];
        let current = rename_keys_everywhere(golden);
        let (_, decode) = table
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .expect("event table covers the fixture");
        let reserialized = decode(&current).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            reserialized, current,
            "{name}: strict current decode must reproduce the renamed fixture"
        );
        let downgraded = event_payload(v3(), name, reserialized)
            .unwrap_or_else(|error| panic!("{name}: {error}"))
            .unwrap_or_else(|| panic!("{name}: protocol 3 defines this event"));
        assert_eq!(&downgraded, golden, "{name}");
        if current != *golden {
            renamed += 1;
        }
    }
    assert!(renamed >= 9, "every identity-bearing event is renamed");
}

/// The session of the first `method` fixture in its current spelling, with a
/// real `NativeRecovery` warning appended; also returns the warnings of the
/// golden, which are what a protocol 3 client must still receive.
fn session_with_native_recovery_warning(method: &str) -> (Value, Value) {
    let results = fixtures(RESULTS, "results.json");
    let golden = results
        .entries
        .iter()
        .find(|entry| entry["method"] == method)
        .unwrap_or_else(|| panic!("{method} fixture"))["result"]
        .clone();
    let mut current = rename_keys_everywhere(&golden);
    let session = if current.is_array() {
        current.get_mut(0).expect("the fixture lists a session")
    } else {
        &mut current
    };
    let warning = serde_json::to_value(SessionWarning {
        kind: SessionWarningKind::NativeRecovery,
        message: "native recovery is unavailable".to_owned(),
        detail: Some("native session id: native-1".to_owned()),
    })
    .expect("serialize the warning");
    session["warnings"]
        .as_array_mut()
        .map(|warnings| warnings.push(warning.clone()))
        .unwrap_or_else(|| session["warnings"] = json!([warning]));
    let golden_session = if golden.is_array() {
        &golden[0]
    } else {
        &golden
    };
    (current, golden_session["warnings"].clone())
}

#[test]
fn a_native_recovery_warning_is_withheld_from_protocol_3_and_kept_on_protocol_4() {
    for method in [method::SESSION_LIST, method::SESSION_NEW] {
        let (current, golden_warnings) = session_with_native_recovery_warning(method);

        let downgraded =
            result(v3(), method, current.clone()).expect("adapter translates the result");
        let session = if downgraded.is_array() {
            &downgraded[0]
        } else {
            &downgraded
        };
        let kinds: Vec<&str> = session
            .get("warnings")
            .and_then(Value::as_array)
            .map(|warnings| warnings.iter().filter_map(|w| w["kind"].as_str()).collect())
            .unwrap_or_default();
        assert!(
            !kinds.contains(&"native_recovery"),
            "{method}: protocol 3 never knew the kind: {kinds:?}"
        );
        assert_eq!(
            session.get("warnings").cloned().unwrap_or(json!([])),
            if golden_warnings.is_null() {
                json!([])
            } else {
                golden_warnings
            },
            "{method}: the kinds protocol 3 defined are untouched"
        );

        assert_eq!(
            result(PROTOCOL_VERSION, method, current.clone()).expect("current is served as is"),
            current,
            "{method}: protocol 4 keeps the warning"
        );
    }
}

#[test]
fn a_session_warning_kind_protocol_3_never_knew_is_withheld_from_a_session_result() {
    let results = fixtures(RESULTS, "results.json");
    let listed = results
        .entries
        .iter()
        .find(|entry| entry["method"] == method::SESSION_LIST)
        .expect("session.list fixture");
    let mut current = rename_keys_everywhere(&listed["result"]);
    let session = current
        .get_mut(0)
        .expect("the fixture lists at least one session");
    let warnings = session["warnings"]
        .as_array_mut()
        .expect("the first fixture session carries warnings");
    let known = warnings.len();
    warnings
        .push(json!({"kind": "native_recovery", "message": "synthetic kind from a later release"}));
    let downgraded =
        result(v3(), method::SESSION_LIST, current).expect("adapter translates the list");
    assert_eq!(
        downgraded[0]["warnings"], listed["result"][0]["warnings"],
        "only the {known} kinds protocol 3 defined remain"
    );
}

#[test]
fn current_requests_downgrade_to_the_protocol_3_golden() {
    let mut renamed = 0;
    for entry in &fixtures(REQUESTS, "requests.json").entries {
        let name = entry_name(entry, "method");
        let golden = &entry["params"];
        let current = rename_keys_everywhere(golden);
        let downgraded = downgrade_request_params(v3(), name, current.clone())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(&downgraded, golden, "{name}");
        for (_, current_key) in RENAMED_KEYS {
            assert!(
                !contains_key(&downgraded, current_key),
                "{name}: `{current_key}` leaked into a protocol 3 request"
            );
        }
        if current != *golden {
            renamed += 1;
        }
    }
    assert!(
        renamed >= 3,
        "output, wait and native-id requests carry the identity"
    );
}

#[test]
fn protocol_3_results_upgrade_to_what_the_current_types_decode() {
    let table = method_table();
    let mut renamed = 0;
    for entry in &fixtures(RESULTS, "results.json").entries {
        let name = entry_name(entry, "method");
        let golden = &entry["result"];
        let upgraded = upgrade_result(v3(), name, golden.clone())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(upgraded, rename_keys_everywhere(golden), "{name}");
        let (_, _, decode) = table
            .iter()
            .find(|(candidate, ..)| *candidate == name)
            .expect("method table covers the fixture");
        assert_eq!(
            decode(&upgraded).unwrap_or_else(|error| panic!("{name}: {error}")),
            upgraded,
            "{name}: strict current decode must reproduce the upgraded result"
        );
        if upgraded != *golden {
            renamed += 1;
        }
    }
    assert!(
        renamed >= 8,
        "every session-bearing method carries the identity"
    );
}

#[test]
fn protocol_3_events_upgrade_to_what_the_current_types_decode() {
    let table = event_table();
    let mut renamed = 0;
    for entry in &fixtures(EVENTS, "events.json").entries {
        let name = entry_name(entry, "event");
        let golden = &entry["payload"];
        let upgraded = upgrade_event_payload(v3(), name, golden.clone())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(upgraded, rename_keys_everywhere(golden), "{name}");
        let (_, decode) = table
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .expect("event table covers the fixture");
        assert_eq!(
            decode(&upgraded).unwrap_or_else(|error| panic!("{name}: {error}")),
            upgraded,
            "{name}: strict current decode must reproduce the upgraded event"
        );
        if upgraded != *golden {
            renamed += 1;
        }
    }
    assert!(renamed >= 9, "every identity-bearing event is renamed");
}

#[test]
fn methods_added_after_protocol_3_are_refused_on_the_client_side_too() {
    for name in introduced_methods(v3()) {
        assert!(
            downgrade_request_params(v3(), name, json!({})).is_err(),
            "{name}"
        );
        assert!(upgrade_result(v3(), name, json!({})).is_err(), "{name}");
    }
}

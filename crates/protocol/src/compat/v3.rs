//! Edge adapter for public protocol version 3.
//!
//! Protocol 3 spelled the worker instance identity `runtime_id`; protocol 4
//! spells it `worker_instance_id` (and `previous_runtime_id` as
//! `previous_worker_instance_id`). The rename touches every place the identity
//! is carried:
//!
//! - `SessionRuntimeIdentity`, nested as `runtime` (`session.output` and
//!   `session.wait` parameters, the `session.input` result, `agent_state` and
//!   `subagent_state` events) or flattened into the result itself
//!   (`session.output`, `session.screen`, `session.read`);
//! - `SessionRuntime`, the `runtime` object of every `SessionInfo` (nested as
//!   `session`, flattened into the `session.new` and `session.fork` results, or
//!   returned bare by `session.inspect` and `session.list`);
//! - `RuntimeInventoryEntry`;
//! - the `session.report_native_id` parameters;
//! - the `session_native_recovered` event.
//!
//! Renames are applied only at those typed locations. Free-form values such as
//! session metadata or notification payloads are never walked, so a user key
//! named `runtime_id` is never rewritten. Anything else protocol 3 could not
//! decode is dropped: events it never defined and warning kinds it never knew.

// Rust guideline compliant 2026-06-26

use serde_json::{Map, Value};

use super::CompatError;
use crate::{event, method};

/// Protocol version this adapter serves.
pub(super) const VERSION: u32 = 3;

/// Spelling of the worker instance key in protocol 4.
const CURRENT_KEY: &str = "worker_instance_id";
/// Spelling of the worker instance key in protocol 3.
const LEGACY_KEY: &str = "runtime_id";
/// Spelling of the replaced worker instance key in protocol 4.
const CURRENT_PREVIOUS_KEY: &str = "previous_worker_instance_id";
/// Spelling of the replaced worker instance key in protocol 3.
const LEGACY_PREVIOUS_KEY: &str = "previous_runtime_id";
/// Object key holding a nested runtime identity or a session runtime.
const RUNTIME_FIELD: &str = "runtime";

/// Event names protocol 3 defined; every other event is withheld from a
/// protocol 3 subscriber.
const KNOWN_EVENTS: &[&str] = &[
    event::AGENT_STATE,
    event::SUBAGENT_STATE,
    event::ATTACH_OPENED,
    event::ATTACH_CLOSED,
    event::NOTIFICATION_CREATED,
    event::NOTIFICATION_UPDATED,
    event::NOTIFICATION_DELETED,
    event::SESSION_CREATED,
    event::SESSION_UPDATED,
    event::SESSION_STOPPED,
    event::SESSION_REMOVED,
    event::SESSION_RUNTIME_RECONNECTED,
    event::SESSION_RUNTIME_LOST,
    event::SESSION_RUNTIME_CONFLICT,
    event::SESSION_RUNTIME_DISCOVERED,
    event::SESSION_NATIVE_RECOVERED,
];

/// `SessionWarningKind` wire values protocol 3 decoded. A session warning of
/// any other kind is withheld, because protocol 3 clients fail to decode the
/// whole session on an unknown kind.
const KNOWN_WARNING_KINDS: &[&str] = &["fetch", "base_branch_fallback", "setup_script", "hook"];

/// Events whose payload carries a `session` object.
const SESSION_EVENTS: &[&str] = &[
    event::SESSION_CREATED,
    event::SESSION_UPDATED,
    event::SESSION_STOPPED,
    event::SESSION_REMOVED,
    event::SESSION_RUNTIME_RECONNECTED,
    event::SESSION_RUNTIME_LOST,
    event::SESSION_RUNTIME_CONFLICT,
];

/// Translates protocol 3 request parameters into the current shape.
pub(super) fn request_params(method: &str, mut params: Value) -> Result<Value, CompatError> {
    match method {
        method::SESSION_OUTPUT | method::SESSION_WAIT => {
            nested_identity(&mut params, "params.runtime", Direction::ToCurrent)?;
        }
        method::SESSION_REPORT_NATIVE_ID => {
            rename_in(&mut params, "params", Direction::ToCurrent)?;
        }
        _ => {}
    }
    Ok(params)
}

/// Translates a current success payload into the protocol 3 shape.
pub(super) fn result(method: &str, mut result: Value) -> Result<Value, CompatError> {
    match method {
        method::SESSION_LIST => {
            if let Value::Array(sessions) = &mut result {
                for session in sessions {
                    session_info(session, "result[]")?;
                }
            }
        }
        // `session.new` and `session.fork` flatten the session into the result.
        method::SESSION_INSPECT | method::SESSION_NEW | method::SESSION_FORK => {
            session_info(&mut result, "result")?;
        }
        method::SESSION_RESUME
        | method::SESSION_RESIZE
        | method::SESSION_SET_METADATA
        | method::SESSION_RENAME
        | method::SESSION_WAIT => {
            if let Some(session) = field(&mut result, "session") {
                session_info(session, "result.session")?;
            }
        }
        method::SESSION_INPUT => {
            nested_identity(&mut result, "result.runtime", Direction::ToLegacy)?;
        }
        method::SESSION_SCREEN | method::SESSION_READ | method::SESSION_OUTPUT => {
            rename_in(&mut result, "result", Direction::ToLegacy)?;
        }
        method::SESSION_RUNTIME_INVENTORY => {
            if let Some(Value::Array(entries)) = field(&mut result, "entries") {
                for entry in entries {
                    rename_in(entry, "result.entries[]", Direction::ToLegacy)?;
                }
            }
        }
        _ => {}
    }
    Ok(result)
}

/// Translates a current event payload into the protocol 3 shape.
pub(super) fn event_payload(name: &str, mut payload: Value) -> Result<Option<Value>, CompatError> {
    if !KNOWN_EVENTS.contains(&name) {
        return Ok(None);
    }
    match name {
        event::AGENT_STATE | event::SUBAGENT_STATE => {
            nested_identity(&mut payload, "payload.runtime", Direction::ToLegacy)?;
        }
        event::SESSION_RUNTIME_DISCOVERED => {
            if let Some(entry) = field(&mut payload, "entry") {
                rename_in(entry, "payload.entry", Direction::ToLegacy)?;
            }
        }
        event::SESSION_NATIVE_RECOVERED => {
            if let Some(session) = field(&mut payload, "session") {
                session_info(session, "payload.session")?;
            }
            if let Value::Object(object) = &mut payload {
                rename_key(object, CURRENT_PREVIOUS_KEY, LEGACY_PREVIOUS_KEY, "payload")?;
                rename_key(object, CURRENT_KEY, LEGACY_KEY, "payload")?;
            }
        }
        _ if SESSION_EVENTS.contains(&name) => {
            if let Some(session) = field(&mut payload, "session") {
                session_info(session, "payload.session")?;
            }
        }
        _ => {}
    }
    Ok(Some(payload))
}

/// Which spelling a rename writes.
#[derive(Clone, Copy)]
enum Direction {
    /// Protocol 3 spelling to the current spelling.
    ToCurrent,
    /// Current spelling to the protocol 3 spelling.
    ToLegacy,
}

impl Direction {
    const fn keys(self) -> (&'static str, &'static str) {
        match self {
            Self::ToCurrent => (LEGACY_KEY, CURRENT_KEY),
            Self::ToLegacy => (CURRENT_KEY, LEGACY_KEY),
        }
    }
}

/// Translates a `SessionInfo`: its runtime identity and its warnings.
fn session_info(session: &mut Value, site: &'static str) -> Result<(), CompatError> {
    let Value::Object(object) = session else {
        return Ok(());
    };
    if let Some(runtime) = object.get_mut(RUNTIME_FIELD) {
        rename_in(runtime, site, Direction::ToLegacy)?;
    }
    if let Some(Value::Array(warnings)) = object.get_mut("warnings") {
        warnings.retain(|warning| {
            warning
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| KNOWN_WARNING_KINDS.contains(&kind))
        });
    }
    Ok(())
}

/// Translates the identity object stored under `runtime` of `container`.
fn nested_identity(
    container: &mut Value,
    site: &'static str,
    direction: Direction,
) -> Result<(), CompatError> {
    match field(container, RUNTIME_FIELD) {
        Some(identity) => rename_in(identity, site, direction),
        None => Ok(()),
    }
}

/// Renames the worker instance key of `value` when it is an object.
///
/// A value that is not an object is left for the strict typed parse to reject.
fn rename_in(
    value: &mut Value,
    site: &'static str,
    direction: Direction,
) -> Result<(), CompatError> {
    let Value::Object(object) = value else {
        return Ok(());
    };
    let (from, to) = direction.keys();
    rename_key(object, from, to, site)
}

/// Moves `from` to `to`, refusing to touch an object that already has `to`.
fn rename_key(
    object: &mut Map<String, Value>,
    from: &'static str,
    to: &'static str,
    site: &'static str,
) -> Result<(), CompatError> {
    if object.contains_key(to) {
        return Err(CompatError::Conflict { site, key: to });
    }
    if let Some(value) = object.remove(from) {
        object.insert(to.to_owned(), value);
    }
    Ok(())
}

fn field<'a>(value: &'a mut Value, key: &str) -> Option<&'a mut Value> {
    match value {
        Value::Object(object) => object.get_mut(key),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{event_payload, request_params, result, KNOWN_EVENTS, KNOWN_WARNING_KINDS};
    use crate::{compat::CompatError, event, method};

    fn session_with_runtime() -> Value {
        json!({
            "id": "s-1",
            "runtime": {"state": "live", "runtime_generation": "1", "worker_instance_id": "w-1"},
            "warnings": []
        })
    }

    #[test]
    fn request_rename_reaches_the_nested_identity_of_output_and_wait() {
        for method_name in [method::SESSION_OUTPUT, method::SESSION_WAIT] {
            let params = json!({
                "session_id": "s-1",
                "runtime": {"runtime_id": "w-1", "runtime_generation": "1"}
            });
            let upgraded = request_params(method_name, params).expect("adapter");
            assert_eq!(
                upgraded["runtime"],
                json!({"worker_instance_id": "w-1", "runtime_generation": "1"}),
                "{method_name}"
            );
        }
    }

    #[test]
    fn request_rename_reaches_the_flat_native_id_report() {
        let upgraded = request_params(
            method::SESSION_REPORT_NATIVE_ID,
            json!({"session_id": "s-1", "runtime_id": "w-1", "agent": "claude"}),
        )
        .expect("adapter");
        assert_eq!(
            upgraded,
            json!({"session_id": "s-1", "worker_instance_id": "w-1", "agent": "claude"})
        );
    }

    #[test]
    fn request_with_the_current_spelling_is_not_a_protocol_3_request() {
        let error = request_params(
            method::SESSION_REPORT_NATIVE_ID,
            json!({"session_id": "s-1", "worker_instance_id": "w-1"}),
        )
        .expect_err("the protocol 4 key does not exist in protocol 3");
        assert_eq!(
            error,
            CompatError::Conflict {
                site: "params",
                key: "worker_instance_id"
            }
        );
    }

    #[test]
    fn request_rename_never_walks_free_form_values() {
        let params = json!({
            "session_id": "s-1",
            "metadata": {"runtime_id": "user-value"}
        });
        let upgraded =
            request_params(method::SESSION_SET_METADATA, params.clone()).expect("adapter");
        assert_eq!(upgraded, params);
    }

    #[test]
    fn request_without_a_runtime_is_untouched() {
        let params = json!({"session_id": "s-1"});
        assert_eq!(
            request_params(method::SESSION_OUTPUT, params.clone()).expect("adapter"),
            params
        );
    }

    #[test]
    fn non_object_runtime_is_left_for_the_typed_parse_to_reject() {
        let params = json!({"session_id": "s-1", "runtime": "w-1"});
        assert_eq!(
            request_params(method::SESSION_OUTPUT, params.clone()).expect("adapter"),
            params
        );
    }

    #[test]
    fn result_rename_reaches_every_session_info_carrier() {
        let carriers = [
            method::SESSION_RESUME,
            method::SESSION_RESIZE,
            method::SESSION_SET_METADATA,
            method::SESSION_RENAME,
            method::SESSION_WAIT,
        ];
        for method_name in carriers {
            let downgraded =
                result(method_name, json!({"session": session_with_runtime()})).expect("adapter");
            assert_eq!(
                downgraded["session"]["runtime"],
                json!({"state": "live", "runtime_generation": "1", "runtime_id": "w-1"}),
                "{method_name}"
            );
        }
        for method_name in [
            method::SESSION_INSPECT,
            method::SESSION_NEW,
            method::SESSION_FORK,
        ] {
            let bare = result(method_name, session_with_runtime()).expect("adapter");
            assert_eq!(bare["runtime"]["runtime_id"], "w-1", "{method_name}");
        }
        let listed = result(
            method::SESSION_LIST,
            json!([session_with_runtime(), session_with_runtime()]),
        )
        .expect("adapter");
        assert_eq!(listed[0]["runtime"]["runtime_id"], "w-1");
        assert_eq!(listed[1]["runtime"]["runtime_id"], "w-1");
    }

    #[test]
    fn result_rename_reaches_flat_and_nested_identities() {
        for method_name in [
            method::SESSION_SCREEN,
            method::SESSION_READ,
            method::SESSION_OUTPUT,
        ] {
            let downgraded = result(
                method_name,
                json!({"worker_instance_id": "w-1", "runtime_generation": "2"}),
            )
            .expect("adapter");
            assert_eq!(
                downgraded,
                json!({"runtime_id": "w-1", "runtime_generation": "2"}),
                "{method_name}"
            );
        }
        let input = result(
            method::SESSION_INPUT,
            json!({"accepted": true, "runtime": {"worker_instance_id": "w-1", "runtime_generation": "2"}}),
        )
        .expect("adapter");
        assert_eq!(input["runtime"]["runtime_id"], "w-1");
    }

    #[test]
    fn result_never_walks_free_form_values_or_other_methods() {
        let free_form = json!({"session": {"metadata": {"worker_instance_id": "user-value"}}});
        assert_eq!(
            result(method::SESSION_NEW, free_form.clone()).expect("adapter"),
            free_form
        );
        let other = json!({"worker_instance_id": "kept"});
        assert_eq!(
            result(method::NOTIFICATION_LIST, other.clone()).expect("adapter"),
            other
        );
    }

    #[test]
    fn result_with_both_spellings_cannot_be_expressed() {
        let error = result(
            method::SESSION_READ,
            json!({"worker_instance_id": "w-1", "runtime_id": "other"}),
        )
        .expect_err("two identities in one object");
        assert_eq!(
            error,
            CompatError::Conflict {
                site: "result",
                key: "runtime_id"
            }
        );
    }

    #[test]
    fn warning_kinds_unknown_to_protocol_3_are_withheld() {
        let mut session = session_with_runtime();
        session["warnings"] = json!([
            {"kind": "fetch", "message": "a"},
            {"kind": "kind_from_the_future", "message": "b"},
            {"kind": "hook", "message": "c"}
        ]);
        let downgraded = result(method::SESSION_INSPECT, session).expect("adapter");
        let kinds: Vec<&str> = downgraded["warnings"]
            .as_array()
            .expect("warnings array")
            .iter()
            .filter_map(|warning| warning["kind"].as_str())
            .collect();
        assert_eq!(kinds, ["fetch", "hook"]);
    }

    #[test]
    fn every_known_warning_kind_survives() {
        let warnings: Vec<Value> = KNOWN_WARNING_KINDS
            .iter()
            .map(|kind| json!({"kind": kind, "message": "m"}))
            .collect();
        let mut session = session_with_runtime();
        session["warnings"] = Value::Array(warnings.clone());
        let downgraded = result(method::SESSION_INSPECT, session).expect("adapter");
        assert_eq!(downgraded["warnings"], Value::Array(warnings));
    }

    #[test]
    fn inventory_entries_are_renamed() {
        let downgraded = result(
            method::SESSION_RUNTIME_INVENTORY,
            json!({"entries": [{"runtime_slot": "a", "worker_instance_id": "w-1", "status": "managed"}]}),
        )
        .expect("adapter");
        assert_eq!(downgraded["entries"][0]["runtime_id"], "w-1");
    }

    #[test]
    fn event_rename_reaches_every_carrier() {
        let identity = json!({"worker_instance_id": "w-1", "runtime_generation": "1"});
        for name in [event::AGENT_STATE, event::SUBAGENT_STATE] {
            let payload = event_payload(name, json!({"runtime": identity.clone()}))
                .expect("adapter")
                .expect("known event");
            assert_eq!(payload["runtime"]["runtime_id"], "w-1", "{name}");
        }
        for name in super::SESSION_EVENTS {
            let payload = event_payload(name, json!({"session": session_with_runtime()}))
                .expect("adapter")
                .expect("known event");
            assert_eq!(payload["session"]["runtime"]["runtime_id"], "w-1", "{name}");
        }
        let discovered = event_payload(
            event::SESSION_RUNTIME_DISCOVERED,
            json!({"entry": {"runtime_slot": "a", "worker_instance_id": "w-1", "status": "managed"}}),
        )
        .expect("adapter")
        .expect("known event");
        assert_eq!(discovered["entry"]["runtime_id"], "w-1");
        let recovered = event_payload(
            event::SESSION_NATIVE_RECOVERED,
            json!({
                "session": session_with_runtime(),
                "previous_worker_instance_id": "w-0",
                "worker_instance_id": "w-1"
            }),
        )
        .expect("adapter")
        .expect("known event");
        assert_eq!(recovered["session"]["runtime"]["runtime_id"], "w-1");
        assert_eq!(recovered["previous_runtime_id"], "w-0");
        assert_eq!(recovered["runtime_id"], "w-1");
        assert!(recovered.get("worker_instance_id").is_none());
    }

    #[test]
    fn events_unknown_to_protocol_3_are_withheld() {
        assert_eq!(
            event_payload("event_from_the_future", json!({"a": 1})).expect("adapter"),
            None
        );
    }

    #[test]
    fn known_events_are_a_subset_of_the_current_events() {
        for name in KNOWN_EVENTS {
            assert!(
                event::EVENT_SPECS.iter().any(|spec| spec.name == *name),
                "{name} is not a current event; removing an event needs a MIN_PROTOCOL_VERSION bump"
            );
        }
    }

    #[test]
    fn every_current_event_is_either_known_or_deliberately_withheld() {
        // Pins the withheld set: adding an event must be a conscious decision
        // about whether protocol 3 subscribers receive it.
        let withheld: Vec<&str> = event::EVENT_SPECS
            .iter()
            .map(|spec| spec.name)
            .filter(|name| !KNOWN_EVENTS.contains(name))
            .collect();
        assert_eq!(withheld, Vec::<&str>::new());
    }
}

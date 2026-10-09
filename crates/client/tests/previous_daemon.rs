//! A client of this build against a daemon of the previous release.
//!
//! The stub daemon behaves like the v0.33.0 daemon, which negotiates protocol
//! 3 only. It accepts a request only when its parameters equal one recorded
//! from that release (`tests/fixtures/compat/v3/requests.json`) and answers with
//! the recorded result of the method. The fixtures were serialized by the
//! release's own code, so neither side of the exchange is hand-written. The
//! expected current shape is an independent blind key rename of the recorded
//! payload, not the adapter under test.
//!
//! The method and family lists are derived from the method registry, so a
//! method or family added without a recording and without an entry in the
//! adapter's introduced-method list fails here.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pohunek_client::protocol::compat::introduced_methods;
use pohunek_client::protocol::method::{self, Method as _};
use pohunek_client::protocol::{
    self, ErrorClass, Event, ProtocolError, ProtocolVersion, ProtocolVersionRange, Request,
    Response, SessionOutputParams, SessionReadParams, SessionScreenParams, SessionWaitParams,
    MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use pohunek_client::{next_request_id, Client, ClientError, ClientOptions, OriginSource};
use pohunek_test_support::wait::guard;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const REQUESTS: &str = include_str!("../../protocol/tests/fixtures/compat/v3/requests.json");
const RESULTS: &str = include_str!("../../protocol/tests/fixtures/compat/v3/results.json");
const EVENTS: &str = include_str!("../../protocol/tests/fixtures/compat/v3/events.json");

/// Route label of the stub host in client errors.
const HOST: &str = "netbird:previous-release";

/// Keys protocol 3 spelled differently, as `(protocol 3, current)`.
const RENAMED_KEYS: &[(&str, &str)] = &[
    ("runtime_id", "worker_instance_id"),
    ("previous_runtime_id", "previous_worker_instance_id"),
];

fn previous() -> ProtocolVersion {
    MIN_PROTOCOL_VERSION
}

fn exact(version: ProtocolVersion) -> ProtocolVersionRange {
    ProtocolVersionRange::new(version, version).expect("an exact range is ordered")
}

fn no_origin_options() -> ClientOptions {
    ClientOptions::default().with_origin_source(OriginSource::Omitted)
}

fn entries(raw: &str, file: &str) -> Vec<Value> {
    let document: Value = serde_json::from_str(raw).expect("fixture file is valid JSON");
    assert_eq!(document["release"], "v0.33.0", "{file}");
    assert_eq!(
        document["protocol_version"],
        previous().get(),
        "{file} records the previous protocol version"
    );
    document["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("{file}: entries must be an array"))
        .clone()
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

/// What the stub observed.
#[derive(Debug, Default)]
struct Seen {
    /// `(connection number, method)` of every accepted request line, in order.
    exchanges: Vec<(usize, String)>,
    /// Requests the previous release would have rejected.
    violations: Vec<String>,
    connections: usize,
    /// Version range each accepted request advertised.
    ranges: Vec<(String, ProtocolVersionRange)>,
}

impl Seen {
    fn methods(&self) -> Vec<&str> {
        self.exchanges
            .iter()
            .map(|(_, method)| method.as_str())
            .collect()
    }
}

/// How the stub deviates from the recordings.
#[derive(Debug, Default, Clone)]
struct Script {
    /// Replaces the recorded result of a method.
    result_overrides: BTreeMap<String, Value>,
    /// Also negotiates the current version, like a daemon of the current
    /// release; replies stay the recorded ones. Shared so a test can roll the
    /// daemon back while connections are open.
    serves_current: Arc<AtomicBool>,
}

struct PreviousDaemon {
    script_flag: Arc<AtomicBool>,
    addr: SocketAddr,
    seen: Arc<Mutex<Seen>>,
    task: JoinHandle<()>,
}

impl Drop for PreviousDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PreviousDaemon {
    async fn start() -> Self {
        Self::start_with(Script::default()).await
    }

    async fn start_with(script: Script) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind the stub daemon");
        let addr = listener.local_addr().expect("stub address");
        let script_flag = Arc::clone(&script.serves_current);
        let seen = Arc::new(Mutex::new(Seen::default()));
        let shared = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let connection = {
                    let mut seen = shared.lock().expect("stub state");
                    seen.connections += 1;
                    seen.connections
                };
                tokio::spawn(serve(
                    stream,
                    connection,
                    Arc::clone(&shared),
                    script.clone(),
                ));
            }
        });
        Self {
            script_flag,
            addr,
            seen,
            task,
        }
    }

    async fn client(&self) -> Client {
        Client::connect_trusted_tcp_addr_with_options(HOST, self.addr, no_origin_options())
            .await
            .expect("connect to the stub daemon")
    }

    fn methods(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("stub state")
            .methods()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// Makes the daemon serve the current version too, or only the previous one.
    fn serve_current(&self, enabled: bool) {
        self.script_flag.store(enabled, Ordering::SeqCst);
    }

    fn violations(&self) -> Vec<String> {
        self.seen.lock().expect("stub state").violations.clone()
    }
}

fn violation(seen: &Mutex<Seen>, text: String) {
    seen.lock().expect("stub state").violations.push(text);
}

async fn write_line(stream: &mut BufReader<TcpStream>, line: &str) {
    // One write per line: a split write meets Nagle's algorithm and the peer's
    // delayed acknowledgement on a reused connection.
    let framed = format!("{line}\n");
    stream
        .get_mut()
        .write_all(framed.as_bytes())
        .await
        .expect("write a stub line");
}

async fn serve(stream: TcpStream, connection: usize, seen: Arc<Mutex<Seen>>, script: Script) {
    let requests = entries(REQUESTS, "requests.json");
    let results = entries(RESULTS, "results.json");
    let events = entries(EVENTS, "events.json");
    let mut stream = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match stream.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let request: Request = serde_json::from_str(&line).expect("the client sends a request");
        let own = if script.serves_current.load(Ordering::SeqCst) {
            protocol::SUPPORTED_PROTOCOL_VERSIONS
        } else {
            exact(previous())
        };
        let reply = |response: Response| serde_json::to_string(&response).expect("serialize");
        let version = match protocol::negotiate(request.version_range(), own) {
            Ok(version) => version,
            Err(error) => {
                let response = Response::err(previous(), request.id(), error).expect("response");
                write_line(&mut stream, &reply(response)).await;
                continue;
            }
        };
        seen.lock()
            .expect("stub state")
            .exchanges
            .push((connection, request.method().to_owned()));
        seen.lock()
            .expect("stub state")
            .ranges
            .push((request.method().to_owned(), request.version_range()));

        let recorded = requests
            .iter()
            .filter(|entry| entry["method"] == request.method())
            .collect::<Vec<_>>();
        if recorded.is_empty() {
            violation(
                &seen,
                format!("{} is not defined in protocol 3", request.method()),
            );
            let response = Response::err(
                version,
                request.id(),
                ProtocolError::method_not_found(request.method()),
            )
            .expect("response");
            write_line(&mut stream, &reply(response)).await;
            continue;
        }
        if !recorded
            .iter()
            .any(|entry| entry["params"] == *request.params())
        {
            violation(
                &seen,
                format!(
                    "{} carried parameters protocol 3 never recorded: {}",
                    request.method(),
                    request.params()
                ),
            );
            let response = Response::err(
                version,
                request.id(),
                ProtocolError::bad_request("parameters are not valid for protocol 3"),
            )
            .expect("response");
            write_line(&mut stream, &reply(response)).await;
            continue;
        }
        let result = script
            .result_overrides
            .get(request.method())
            .cloned()
            .or_else(|| {
                results
                    .iter()
                    .find(|entry| entry["method"] == request.method())
                    .map(|entry| entry["result"].clone())
            })
            .expect("every recorded request has a recorded result");
        let response = Response::ok(version, request.id(), result).expect("response");
        write_line(&mut stream, &reply(response)).await;

        if request.method() == method::SUBSCRIBE {
            for entry in &events {
                let name = entry["event"].as_str().expect("event name");
                let event = Event::new(version, name, entry["payload"].clone()).expect("event");
                write_line(
                    &mut stream,
                    &serde_json::to_string(&event).expect("serialize event"),
                )
                .await;
            }
            return;
        }
    }
}

/// Every recorded request entry as `(method, current-shape params, current-shape result)`.
fn recorded_exchanges() -> Vec<(String, Value, Value)> {
    let results = entries(RESULTS, "results.json");
    entries(REQUESTS, "requests.json")
        .iter()
        .map(|entry| {
            let name = entry["method"].as_str().expect("method name").to_owned();
            let result = results
                .iter()
                .find(|candidate| candidate["method"] == name)
                .unwrap_or_else(|| panic!("no recorded result for {name}"));
            (
                name,
                rename_keys_everywhere(&entry["params"]),
                rename_keys_everywhere(&result["result"]),
            )
        })
        .collect()
}

fn request_for(name: &str, params: Value) -> Request {
    Request::new(next_request_id(name), name, params).expect("valid request")
}

/// A `session.list` request recorded from the previous release, in current spelling.
fn session_list_request() -> Request {
    let (name, params, _) = recorded_exchanges()
        .into_iter()
        .find(|(name, ..)| name == method::SESSION_LIST)
        .expect("a session.list recording");
    request_for(&name, params)
}

fn registry_methods() -> BTreeSet<&'static str> {
    method::METHOD_SPECS.iter().map(|spec| spec.name).collect()
}

fn family(name: &str) -> &str {
    name.split('.').next().expect("a method name is not empty")
}

fn assert_current_shape(value: &Value) {
    for (legacy, _) in RENAMED_KEYS {
        assert!(
            !contains_key(value, legacy),
            "`{legacy}` leaked into a current-shape payload: {value}"
        );
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

#[tokio::test]
async fn every_registered_method_is_recorded_or_new_in_the_current_protocol() {
    let recorded: BTreeSet<String> = recorded_exchanges()
        .into_iter()
        .map(|(name, ..)| name)
        .collect();
    let introduced: BTreeSet<&str> = introduced_methods(previous()).iter().copied().collect();
    for name in registry_methods() {
        assert!(
            recorded.contains(name) ^ introduced.contains(name),
            "{name} must have exactly one of a protocol 3 recording or an introduced-method entry"
        );
    }
}

/// Runs every recorded request through `client_for` and checks the answer.
async fn every_recorded_method_reaches_the_previous_daemon(fresh_connection_per_request: bool) {
    let daemon = PreviousDaemon::start().await;
    let mut shared = daemon.client().await;
    let mut exercised = BTreeSet::new();
    for (name, params, expected) in recorded_exchanges() {
        if name == method::SUBSCRIBE {
            // A subscription turns the connection into an event stream; its
            // own test covers it.
            exercised.insert(name);
            continue;
        }
        let request = request_for(&name, params);
        let outcome = if fresh_connection_per_request {
            daemon.client().await.request(&request).await
        } else {
            shared.request(&request).await
        };
        let value = outcome.unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(value, expected, "{name}");
        assert_current_shape(&value);
        exercised.insert(name);
    }
    assert_eq!(
        daemon.violations(),
        Vec::<String>::new(),
        "the previous release rejects what the client sent"
    );

    let exercised_families: BTreeSet<&str> = exercised.iter().map(|name| family(name)).collect();
    let introduced: BTreeSet<&str> = introduced_methods(previous()).iter().copied().collect();
    let registry_families: BTreeSet<&str> = registry_methods()
        .into_iter()
        .filter(|name| !introduced.contains(name))
        .map(family)
        .collect();
    assert_eq!(
        exercised_families, registry_families,
        "every method family of the registry is exercised against the previous daemon"
    );
}

#[tokio::test]
async fn every_method_family_works_on_one_connection_to_a_previous_daemon() {
    guard(
        "the warm connection run",
        every_recorded_method_reaches_the_previous_daemon(false),
    )
    .await;
}

#[tokio::test]
async fn every_method_family_works_on_a_cold_connection_to_a_previous_daemon() {
    guard(
        "the cold connection run",
        every_recorded_method_reaches_the_previous_daemon(true),
    )
    .await;
}

#[tokio::test]
async fn a_request_that_is_the_same_in_both_versions_is_sent_without_a_probe() {
    guard("the independent request", async {
        let daemon = PreviousDaemon::start().await;
        let mut client = daemon.client().await;
        client
            .request(&session_list_request())
            .await
            .expect("session.list");
        assert_eq!(daemon.methods(), [method::SESSION_LIST]);
        assert_eq!(client.selected_version(), Some(previous()));
        assert_eq!(daemon.violations(), Vec::<String>::new());
    })
    .await;
}

#[tokio::test]
async fn a_version_dependent_request_learns_the_version_before_it_is_sent() {
    guard("the dependent request", async {
        let daemon = PreviousDaemon::start().await;
        let mut client = daemon.client().await;
        let (_, params, expected) = recorded_exchanges()
            .into_iter()
            .find(|(name, params, _)| {
                name == method::SESSION_OUTPUT && params.get("runtime").is_some()
            })
            .expect("a runtime-bearing session.output recording");
        let value = client
            .request(&request_for(method::SESSION_OUTPUT, params))
            .await
            .expect("session.output");
        assert_eq!(value, expected);
        assert_eq!(
            daemon.methods(),
            [method::DAEMON_HEALTH, method::SESSION_OUTPUT],
            "the version is learned first, then the request is sent in protocol 3"
        );
        assert_eq!(daemon.violations(), Vec::<String>::new());
    })
    .await;
}

#[tokio::test]
async fn typed_calls_decode_the_translated_results() {
    guard("the typed calls", async {
        let daemon = PreviousDaemon::start().await;
        let mut client = daemon.client().await;
        let exchanges = recorded_exchanges();
        let find = |name: &str, with_runtime: Option<bool>| {
            exchanges
                .iter()
                .find(|(candidate, params, _)| {
                    candidate == name
                        && with_runtime
                            .is_none_or(|wanted| params.get("runtime").is_some() == wanted)
                })
                .unwrap_or_else(|| panic!("a {name} recording"))
                .clone()
        };

        let (_, params, expected) = find(method::SESSION_SCREEN, None);
        let screen = client
            .session_screen(serde_json::from_value::<SessionScreenParams>(params).expect("params"))
            .await
            .expect("session.screen");
        assert_eq!(serde_json::to_value(&screen).expect("encode"), expected);

        let (_, params, expected) = find(method::SESSION_READ, None);
        let read = client
            .session_read(serde_json::from_value::<SessionReadParams>(params).expect("params"))
            .await
            .expect("session.read");
        assert_eq!(serde_json::to_value(&read).expect("encode"), expected);

        let (_, params, expected) = find(method::SESSION_OUTPUT, Some(false));
        let output = client
            .session_output(serde_json::from_value::<SessionOutputParams>(params).expect("params"))
            .await
            .expect("session.output");
        assert_eq!(serde_json::to_value(&output).expect("encode"), expected);

        // A waiting output read and a wait use a dedicated connection that
        // inherits the version this connection already selected.
        let (_, params, expected) = find(method::SESSION_OUTPUT, Some(true));
        let waited = client
            .session_output(serde_json::from_value::<SessionOutputParams>(params).expect("params"))
            .await
            .expect("waiting session.output");
        assert_eq!(serde_json::to_value(&waited).expect("encode"), expected);

        let (_, params, expected) = find(method::SESSION_WAIT, None);
        let wait = client
            .session_wait(serde_json::from_value::<SessionWaitParams>(params).expect("params"))
            .await
            .expect("session.wait");
        assert_eq!(serde_json::to_value(&wait).expect("encode"), expected);

        assert_eq!(daemon.violations(), Vec::<String>::new());
        let seen = daemon.seen.lock().expect("stub state");
        let probes = seen
            .exchanges
            .iter()
            .filter(|(_, name)| name == method::DAEMON_HEALTH)
            .count();
        assert_eq!(
            probes, 0,
            "the first response selects the version, so no connection probes"
        );
    })
    .await;
}

#[tokio::test]
async fn a_dedicated_first_call_learns_the_version_on_its_own_connection() {
    guard("the dedicated first call", async {
        let daemon = PreviousDaemon::start().await;
        let mut client = daemon.client().await;
        let (_, params, expected) = recorded_exchanges()
            .into_iter()
            .find(|(name, ..)| name == method::SESSION_WAIT)
            .expect("a session.wait recording");
        let wait = client
            .session_wait(serde_json::from_value::<SessionWaitParams>(params).expect("params"))
            .await
            .expect("session.wait");
        assert_eq!(serde_json::to_value(&wait).expect("encode"), expected);
        assert_eq!(
            daemon.methods(),
            [method::DAEMON_HEALTH, method::SESSION_WAIT]
        );
        assert_eq!(daemon.violations(), Vec::<String>::new());
    })
    .await;
}

#[tokio::test]
async fn subscription_events_arrive_in_the_current_shape() {
    guard("the subscription", async {
        let daemon = PreviousDaemon::start().await;
        let recorded = entries(EVENTS, "events.json");

        let request = request_for(method::SUBSCRIBE, Value::Null);
        let mut subscription = daemon
            .client()
            .await
            .subscribe(&request)
            .await
            .expect("subscribe");
        for entry in &recorded {
            let event = subscription
                .next_event()
                .await
                .expect("an event")
                .expect("the stub streams every recording");
            assert_eq!(event.event(), entry["event"].as_str().expect("name"));
            assert_eq!(event.version(), PROTOCOL_VERSION);
            assert_eq!(
                event.payload(),
                &rename_keys_everywhere(&entry["payload"]),
                "{}",
                event.event()
            );
        }
        assert!(subscription
            .next_event()
            .await
            .expect("clean close")
            .is_none());

        let mut lines = daemon
            .client()
            .await
            .subscribe(&request_for(method::SUBSCRIBE, Value::Null))
            .await
            .expect("subscribe");
        for entry in &recorded {
            let line = lines
                .next_line()
                .await
                .expect("a line")
                .expect("the stub streams every recording");
            let event: Event = serde_json::from_str(&line).expect("a current-shape event line");
            assert_eq!(event.version(), PROTOCOL_VERSION);
            assert_eq!(
                event.payload(),
                &rename_keys_everywhere(&entry["payload"]),
                "{}",
                event.event()
            );
        }
        assert_eq!(daemon.violations(), Vec::<String>::new());
    })
    .await;
}

#[tokio::test]
async fn a_method_the_previous_release_never_defined_fails_before_it_is_sent() {
    guard("the introduced methods", async {
        let introduced = introduced_methods(previous());
        assert!(
            !introduced.is_empty(),
            "the adapter lists introduced methods"
        );
        for name in introduced {
            let daemon = PreviousDaemon::start().await;
            let mut client = daemon.client().await;
            let error = client
                .request(&request_for(name, json!(null)))
                .await
                .expect_err("the previous daemon does not define the method");
            match &error {
                ClientError::DaemonProtocolTooOld {
                    host,
                    method,
                    daemon_version,
                    required_version,
                } => {
                    assert_eq!(host.as_deref(), Some(HOST), "{name}");
                    assert_eq!(method, name);
                    assert_eq!(*daemon_version, previous());
                    assert_eq!(*required_version, PROTOCOL_VERSION);
                }
                other => panic!("{name}: expected DaemonProtocolTooOld, got {other:?}"),
            }
            assert_eq!(
                daemon.methods(),
                [method::DAEMON_HEALTH],
                "{name}: only the version probe reaches the previous daemon"
            );
            assert_eq!(daemon.violations(), Vec::<String>::new());
        }
    })
    .await;
}

#[tokio::test]
async fn the_typed_error_names_the_host_the_versions_and_the_upgrade() {
    guard("the typed error", async {
        let daemon = PreviousDaemon::start().await;
        let mut client = daemon.client().await;
        let error = client
            .call::<method::PackageList>(())
            .await
            .expect_err("package.list is new in the current protocol");
        let structured = error.to_protocol_error();
        assert_eq!(structured.class, ErrorClass::Daemon);
        assert_eq!(structured.code, "daemon_protocol_too_old");
        assert_eq!(
            structured.msg,
            format!(
                "host '{HOST}' runs protocol {}, but `{}` needs protocol {}",
                previous(),
                method::PackageList::NAME,
                PROTOCOL_VERSION
            )
        );
        let recover = structured.recover.expect("an actionable hint");
        assert!(recover.contains(HOST), "{recover}");
        assert!(recover.contains(&PROTOCOL_VERSION.to_string()), "{recover}");
        assert_eq!(error.to_string(), structured.msg);
        // The connection stays usable for methods both versions define.
        client
            .request(&session_list_request())
            .await
            .expect("session.list still works");
    })
    .await;
}

#[tokio::test]
async fn a_result_the_previous_release_could_not_have_sent_is_refused() {
    guard("the untranslatable result", async {
        // Protocol 3 spells the identity `runtime_id`; a result that already
        // carries the current key next to it is not a protocol 3 result.
        let script = Script {
            result_overrides: BTreeMap::from([(
                method::SESSION_READ.to_owned(),
                json!({"runtime_id": "w-1", "worker_instance_id": "w-2"}),
            )]),
            ..Script::default()
        };
        let daemon = PreviousDaemon::start_with(script).await;
        let mut client = daemon.client().await;
        let (_, params, _) = recorded_exchanges()
            .into_iter()
            .find(|(name, ..)| name == method::SESSION_READ)
            .expect("a session.read recording");
        let error = client
            .request(&request_for(method::SESSION_READ, params))
            .await
            .expect_err("the payload is refused, not passed through");
        assert!(
            matches!(error, ClientError::VersionTranslation { .. }),
            "{error:?}"
        );
        let structured = error.to_protocol_error();
        assert_eq!(structured.class, ErrorClass::Daemon);
        assert_eq!(structured.code, "version_translation_failed");
        assert!(structured.msg.contains(HOST), "{}", structured.msg);
        assert!(structured.recover.is_some());
    })
    .await;
}

#[tokio::test]
async fn a_request_is_never_written_after_its_version_probe_timed_out() {
    guard("the timed-out probe", async {
        // A daemon that reads the version probe and never answers it.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind the silent daemon");
        let addr = listener.local_addr().expect("silent daemon address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept the client");
            let mut stream = BufReader::new(stream);
            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                match stream.read_line(&mut line).await {
                    Ok(0) | Err(_) => return lines,
                    Ok(_) => lines.push(line),
                }
            }
        });

        let options =
            no_origin_options().with_request_timeout(std::time::Duration::from_millis(50));
        let mut client = Client::connect_trusted_tcp_addr_with_options(HOST, addr, options)
            .await
            .expect("connect to the silent daemon");
        let uninstall = request_for(method::PACKAGE_UNINSTALL, json!({"digest": "x"}));
        let error = client
            .request(&uninstall)
            .await
            .expect_err("the probe is never answered");
        assert!(
            matches!(
                error,
                ClientError::RemoteProtocol { .. } | ClientError::RequestTimeout { .. }
            ),
            "{error:?}"
        );
        // The connection is torn: a retry is refused without writing anything.
        let retry = client
            .request(&uninstall)
            .await
            .expect_err("a poisoned connection refuses further requests");
        assert!(matches!(retry, ClientError::Framing(_)), "{retry:?}");
        drop(client);

        let lines = server.await.expect("silent daemon task");
        assert_eq!(lines.len(), 1, "only the probe was written: {lines:?}");
        assert!(lines[0].contains(method::DAEMON_HEALTH), "{}", lines[0]);
    })
    .await;
}

/// A recorded request whose parameters differ between the two versions.
fn runtime_bearing_output() -> (Value, Value) {
    let (_, params, expected) = recorded_exchanges()
        .into_iter()
        .find(|(name, params, _)| name == method::SESSION_OUTPUT && params.get("runtime").is_some())
        .expect("a runtime-bearing session.output recording");
    (params, expected)
}

#[tokio::test]
async fn a_previous_only_request_settles_the_connection_on_the_previous_version() {
    guard("the previous-only request", async {
        // A daemon that also serves the current version would select it for a
        // probe that offered the whole window.
        let daemon = PreviousDaemon::start_with(Script {
            serves_current: Arc::new(AtomicBool::new(true)),
            ..Script::default()
        })
        .await;
        let mut client = daemon.client().await;
        let (params, expected) = runtime_bearing_output();
        // Current-spelling parameters would need a probe; the request itself only
        // allows the previous version, so the probe must offer only that.
        let request = request_for(method::SESSION_OUTPUT, params).with_exact_version(previous());
        assert_eq!(
            client
                .request(&request)
                .await
                .expect("served as protocol 3"),
            expected
        );
        assert_eq!(client.selected_version(), Some(previous()));
        assert_eq!(daemon.violations(), Vec::<String>::new());
    })
    .await;
}

#[tokio::test]
async fn a_current_only_request_against_a_previous_daemon_is_refused_before_sending() {
    guard("the current-only request", async {
        let daemon = PreviousDaemon::start().await;
        let (params, _) = runtime_bearing_output();
        let current_only = request_for(method::SESSION_OUTPUT, params.clone())
            .with_exact_version(PROTOCOL_VERSION);

        // Cold: the probe offers only the current version, which the previous
        // daemon refuses; the request itself is never sent.
        let mut cold = daemon.client().await;
        let error = cold
            .request(&current_only)
            .await
            .expect_err("a previous daemon cannot serve a current-only request");
        assert_eq!(
            error.to_protocol_error().code,
            "version_mismatch",
            "{error:?}"
        );
        assert_eq!(daemon.methods(), Vec::<String>::new());

        // Warm: the connection already selected the previous version.
        let mut warm = daemon.client().await;
        warm.request(&session_list_request())
            .await
            .expect("session.list");
        let error = warm
            .request(&current_only)
            .await
            .expect_err("refused client-side");
        match error {
            ClientError::DaemonProtocolTooOld {
                daemon_version,
                required_version,
                ..
            } => {
                assert_eq!(daemon_version, previous());
                assert_eq!(required_version, PROTOCOL_VERSION);
            }
            other => panic!("expected DaemonProtocolTooOld, got {other:?}"),
        }
        assert_eq!(daemon.methods(), [method::SESSION_LIST]);
        assert_eq!(daemon.violations(), Vec::<String>::new());
    })
    .await;
}

#[tokio::test]
async fn a_dedicated_mutation_is_refused_by_a_daemon_rolled_back_after_the_parent_selected() {
    guard("the rolled-back daemon", async {
        let daemon = PreviousDaemon::start_with(Script {
            serves_current: Arc::new(AtomicBool::new(true)),
            ..Script::default()
        })
        .await;
        let mut client = daemon.client().await;
        client
            .request(&session_list_request())
            .await
            .expect("the parent selects the current version");
        assert_eq!(client.selected_version(), Some(PROTOCOL_VERSION));

        // The daemon restarts as the previous release. The waiting input runs on a
        // dedicated connection that inherits the parent's version.
        daemon.serve_current(false);
        let (_, params, _) = recorded_exchanges()
            .into_iter()
            .find(|(name, ..)| name == method::SESSION_INPUT)
            .expect("a session.input recording");
        let params: protocol::SessionInputParams =
            serde_json::from_value(params).expect("waiting input params");
        assert!(params.wait.is_some(), "the recording waits for the result");
        let error = client
            .session_input(params)
            .await
            .expect_err("the rolled-back daemon refuses the pinned request");
        assert_eq!(
            error.to_protocol_error().code,
            "version_mismatch",
            "{error:?}"
        );
        assert_eq!(
            daemon.methods(),
            [method::SESSION_LIST],
            "the input was never executed"
        );
    })
    .await;
}

#[tokio::test]
async fn host_discovery_keeps_the_whole_window_after_the_connection_selected_a_version() {
    guard("the discovery range", async {
        let daemon = PreviousDaemon::start_with(Script {
            serves_current: Arc::new(AtomicBool::new(true)),
            ..Script::default()
        })
        .await;
        let mut client = daemon.client().await;
        assert_eq!(
            client.handshake().await.expect("handshake"),
            PROTOCOL_VERSION
        );
        let (name, params, _) = recorded_exchanges()
            .into_iter()
            .find(|(name, ..)| name == method::HOST_DISCOVER)
            .expect("a host.discover recording");
        client
            .request(&request_for(&name, params))
            .await
            .expect("host.discover");
        client
            .request(&session_list_request())
            .await
            .expect("session.list");

        let seen = daemon.seen.lock().expect("stub state");
        let range_of = |wanted: &str| {
            let (_, range) = seen
                .ranges
                .iter()
                .find(|(name, _)| name == wanted)
                .unwrap_or_else(|| panic!("no {wanted} request"));
            *range
        };
        assert_eq!(
            range_of(method::HOST_DISCOVER),
            protocol::CLIENT_PROTOCOL_VERSIONS,
            "the daemon probes peers for the client's whole window"
        );
        assert_eq!(
            range_of(method::SESSION_LIST),
            exact(PROTOCOL_VERSION),
            "every other request is pinned to the selected version"
        );
        assert_eq!(seen.violations, Vec::<String>::new());
    })
    .await;
}

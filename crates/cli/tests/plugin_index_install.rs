//! End-to-end contracts of `pohunek plugin install` through the plugin index.
//!
//! Every test starts a real `pohunekd` beside a throwaway trust anchor (and,
//! where the scenario needs one, the index channel file), serves a signed
//! index and archives from a loopback HTTP server thread, and drives the real
//! `pohunek` binary. The CLI fetches and selects; the daemon verifies. Signing
//! keys are generated inside each test process and never leave it.

// Rust guideline compliant 2026-11-06

#![cfg(unix)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer as _, SigningKey};
use package::{
    build_archive, index_key_record_signing_message, index_signing_message, read_archive,
    revocation_signing_message, ArchiveEntry, CatalogSignature, IndexBody, IndexChannel,
    IndexEntry, IndexEntryKind, IndexEnvelope, IndexKeyRecord, Limits, PackageDigest,
    RevocationBody, SignedRevocations, INDEX_SCHEMA_VERSION, INDEX_SCHEMA_VERSION_2,
};
use pohunek_test_support::bin_exe;
use pohunek_test_support::catalog::{anchor_for, host_platform, key_id, test_key, ANY_CORE};
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde_json::Value;

use super::plugin_catalog_install::{Anchor, Channel, Install};

/// Package id and runtime id of the indexed fixture package.
const PACKAGE_ID: &str = "pohunek.runtime.idxfix";
const RUNTIME: &str = "idxfix";

/// Program the fixture runtime launches; it only has to exist.
const PROGRAM: &str = "/bin/sh";

/// Path of the index document on the loopback server.
const INDEX_PATH: &str = "/official-index-v1.json";

/// One day in seconds.
const DAY: u64 = 24 * 60 * 60;

/// How long a fixture index lives; below the verifier's lifetime cap.
const INDEX_LIFETIME: u64 = 7 * DAY;

/// How far before now a fixture key record opens.
const KEY_BACK_DATE: u64 = 1_000;

/// How long a fixture key record stays valid; below the verifier's cap.
const KEY_WINDOW: u64 = 100 * DAY;

/// Index body cap and archive cap of the CLI, plus one byte.
const OVER_INDEX_CAP: usize = 1024 * 1024 + 1;
const OVER_ARCHIVE_CAP: usize = 8 * 1024 * 1024 + 1;

/// A host no test server answers on; the documentation range of RFC 5737.
const FOREIGN_URL: &str = "http://198.51.100.1/x";

const DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

/// A runtime package descriptor for `package` serving `runtime`.
fn runtime_document_of(package: &str, runtime: &str, version: &str) -> String {
    format!(
        r#"schema = 1
id = "{package}"
version = "{version}"
runtime_api = 1

[runtime]
id = "{runtime}"
name = "Idx fixture"
program = "{PROGRAM}"
args = []
detect_manifest = "detect.toml"
prompt_arg = true

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["--session", "{{reference}}"]

[fork]
supported = false

[native_reference]
strategy = "assigned"
launch_args = ["--session-id", "{{reference}}"]

[native_reference.existence]
check = "none"
"#
    )
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_secs()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String");
        text
    })
}

/// A package archive: its bytes and digest.
#[derive(Clone)]
struct Archive {
    bytes: Vec<u8>,
    digest: PackageDigest,
}

fn archive(version: &str) -> Archive {
    archive_of(PACKAGE_ID, RUNTIME, version)
}

/// An archive of the runtime package `package` serving `runtime`.
fn archive_of(package: &str, runtime: &str, version: &str) -> Archive {
    let entries = [
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: runtime_document_of(package, runtime, version).into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "detect.toml".to_owned(),
            contents: DETECT_MANIFEST.as_bytes().to_vec(),
            executable: false,
        },
    ];
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    Archive { bytes, digest }
}

/// What the signed index lists for one archive.
struct Listing {
    archive: Archive,
    package: &'static str,
    runtime: &'static str,
    version: &'static str,
    url: String,
    platform: String,
    core: &'static str,
}

impl Listing {
    fn new(version: &'static str, url: String) -> Self {
        Self::of(PACKAGE_ID, RUNTIME, version, url)
    }

    /// A listing of `package` serving `runtime`.
    fn of(
        package: &'static str,
        runtime: &'static str,
        version: &'static str,
        url: String,
    ) -> Self {
        Self {
            archive: archive_of(package, runtime, version),
            package,
            runtime,
            version,
            url,
            platform: host_platform(),
            core: ANY_CORE,
        }
    }

    fn entry(&self) -> IndexEntry {
        IndexEntry {
            package_id: PackageId::parse(self.package).expect("package id"),
            kind: None,
            runtime_id: Some(RuntimeId::parse(self.runtime).expect("runtime id")),
            version: PackageVersion::parse(self.version).expect("version"),
            digest: self.archive.digest.clone(),
            url: self.url.clone(),
            platforms: vec![self.platform.clone()],
            core: self.core.to_owned(),
        }
    }
}

/// The endorser root R and the index key K it endorses.
struct Keys {
    root: SigningKey,
    key: SigningKey,
}

impl Keys {
    fn new() -> Self {
        Self {
            root: test_key(),
            key: test_key(),
        }
    }

    fn record(&self, subject: &SigningKey) -> IndexKeyRecord {
        let start = now() - KEY_BACK_DATE;
        let mut record = IndexKeyRecord {
            key_id: key_id(subject),
            public_key: hex(subject.verifying_key().as_bytes()),
            scope: "official".to_owned(),
            not_before: start,
            not_after: start + KEY_WINDOW,
            endorsed_by: key_id(&self.root),
            signature: String::new(),
        };
        record.signature = hex(&self
            .root
            .sign(&index_key_record_signing_message(&record))
            .to_bytes());
        record
    }

    /// An index document signed by `signer` at `sequence` listing `entries`.
    fn document(&self, signer: &SigningKey, sequence: u64, entries: Vec<IndexEntry>) -> Vec<u8> {
        self.document_of(INDEX_SCHEMA_VERSION, signer, sequence, entries)
    }

    /// A schema 2 document signed by K, current as of now.
    fn signed_v2(&self, entries: Vec<IndexEntry>) -> Vec<u8> {
        self.document_of(INDEX_SCHEMA_VERSION_2, &self.key, now(), entries)
    }

    /// An index document of `schema` signed by `signer` at `sequence`. The
    /// embedded revocation list keeps its own schema 1 format.
    fn document_of(
        &self,
        schema: u64,
        signer: &SigningKey,
        sequence: u64,
        entries: Vec<IndexEntry>,
    ) -> Vec<u8> {
        self.document_living(schema, signer, sequence, INDEX_LIFETIME, entries)
    }

    /// A schema 1 document signed by K, current as of now, that expires
    /// `lifetime` seconds after its sequence.
    fn signed_living(&self, lifetime: u64, entries: Vec<IndexEntry>) -> Vec<u8> {
        self.document_living(INDEX_SCHEMA_VERSION, &self.key, now(), lifetime, entries)
    }

    fn document_living(
        &self,
        schema: u64,
        signer: &SigningKey,
        sequence: u64,
        lifetime: u64,
        entries: Vec<IndexEntry>,
    ) -> Vec<u8> {
        let body = IndexBody {
            schema_version: schema,
            scope: "official".to_owned(),
            sequence,
            expires_at: sequence + lifetime,
            entries,
        };
        let message = index_signing_message(&body).expect("index message");
        let revocations = RevocationBody {
            schema_version: INDEX_SCHEMA_VERSION,
            sequence: 1,
            revoked_key_ids: Vec::new(),
        };
        let revocation_message = revocation_signing_message(&revocations).expect("revocations");
        serde_json::to_vec(&IndexEnvelope {
            schema_version: schema,
            index: body,
            key_records: vec![self.record(&self.key)],
            signatures: vec![CatalogSignature {
                key_id: key_id(signer),
                signature: hex(&signer.sign(&message).to_bytes()),
            }],
            revocations: SignedRevocations {
                signature: CatalogSignature {
                    key_id: key_id(&self.root),
                    signature: hex(&self.root.sign(&revocation_message).to_bytes()),
                },
                revocations,
            },
        })
        .expect("serialize the index")
    }

    /// A document signed by K, current as of now.
    fn signed(&self, entries: Vec<IndexEntry>) -> Vec<u8> {
        self.document(&self.key, now(), entries)
    }

    fn anchor(&self) -> Anchor {
        Anchor::File {
            bytes: anchor_for(&self.root),
            mode: 0o644,
        }
    }

    fn channel(&self) -> Vec<u8> {
        IndexChannel::new(key_id(&self.root))
            .to_bytes()
            .expect("channel bytes")
    }
}

/// How the loopback server answers one path.
#[derive(Clone)]
enum Route {
    /// 200 with these bytes.
    Body(Vec<u8>),
    /// This status and an empty body.
    Status(u16),
    /// 302 to this URL.
    Redirect(String),
    /// 200 without a length, streaming this many zero bytes.
    Endless(usize),
}

#[derive(Default)]
struct ServerState {
    routes: Mutex<HashMap<String, Route>>,
    requests: Mutex<Vec<String>>,
}

/// A loopback HTTP server on a thread; it records the path of every request.
struct Server {
    addr: SocketAddr,
    state: Arc<ServerState>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the loopback server");
        let addr = listener.local_addr().expect("server address");
        let state = Arc::new(ServerState::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let state = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(stream) = stream {
                        serve(stream, &state);
                    }
                }
            })
        };
        Self {
            addr,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn set(&self, path: &str, route: Route) {
        self.state
            .routes
            .lock()
            .expect("routes")
            .insert(path.to_owned(), route);
    }

    fn requests(&self) -> Vec<String> {
        self.state.requests.lock().expect("requests").clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the blocking accept so the thread sees the stop flag.
        let _wake = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("server thread");
        }
    }
}

fn serve(stream: TcpStream, state: &ServerState) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            Ok(_) if header == "\r\n" => break,
            Ok(_) => {}
        }
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_owned();
    state.requests.lock().expect("requests").push(path.clone());
    let route = state.routes.lock().expect("routes").get(&path).cloned();
    let mut stream = stream;
    // A client that hangs up early is part of the over-cap scenarios.
    let _outcome = match route {
        Some(Route::Body(body)) => write_response(&mut stream, "200 OK", &[], Some(&body)),
        Some(Route::Status(code)) => {
            write_response(&mut stream, &format!("{code} Status"), &[], Some(b""))
        }
        Some(Route::Redirect(target)) => write_response(
            &mut stream,
            "302 Found",
            &[("Location", target.as_str())],
            Some(b""),
        ),
        Some(Route::Endless(length)) => {
            let head = write_response(&mut stream, "200 OK", &[], None);
            head.and_then(|()| stream.write_all(&vec![0_u8; length]))
        }
        None => write_response(&mut stream, "404 Not Found", &[], Some(b"")),
    };
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    if let Some(body) = body {
        let _ = write!(head, "Content-Length: {}\r\n", body.len());
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some(body) = body {
        stream.write_all(body)?;
    }
    stream.flush()
}

/// A daemon with the index channel, the signing keys and a loopback server.
struct Rig {
    install: Install,
    keys: Keys,
    server: Server,
}

impl Rig {
    async fn start() -> Self {
        let keys = Keys::new();
        let channel = keys.channel();
        let install = Install::start_with_channel(
            &keys.anchor(),
            Some(Channel {
                bytes: &channel,
                mode: 0o644,
            }),
        )
        .await;
        Self {
            install,
            keys,
            server: Server::start(),
        }
    }

    /// Serves `listings` as the signed index and each archive at `/<version>.tar.zst`.
    fn publish(&self, listings: &[Listing]) {
        let entries: Vec<_> = listings.iter().map(Listing::entry).collect();
        self.server
            .set(INDEX_PATH, Route::Body(self.keys.signed(entries.clone())));
        // A schema 2 document names the kind of every entry.
        let kinded = entries
            .into_iter()
            .map(|mut entry| {
                entry.kind = Some(IndexEntryKind::Runtime);
                entry
            })
            .collect();
        self.server
            .set(INDEX_PATH_V2, Route::Body(self.keys.signed_v2(kinded)));
        for listing in listings {
            self.server.set(
                &archive_path(listing.version),
                Route::Body(listing.archive.bytes.clone()),
            );
        }
    }

    /// A listing whose archive is served by this rig's server.
    fn listing(&self, version: &'static str) -> Listing {
        Listing::new(version, self.server.url(&archive_path(version)))
    }

    fn index_url(&self) -> String {
        self.server.url(INDEX_PATH)
    }

    /// `pohunek plugin install <arguments> --index-url <server> --yes --json`.
    async fn install(&self, source: &str, extra: &[&str]) -> (i32, Value) {
        let url = self.index_url();
        let mut arguments = vec!["plugin", "install", source, "--index-url", &url];
        arguments.extend_from_slice(extra);
        self.install.json(&arguments).await
    }

    /// Delete the stored index copy so the next install must fetch.
    ///
    /// The CLI sidecar lives under `<XDG_STATE_HOME>/pohunek/plugins/index`
    /// (`state_dir` already joins `pohunek`), so the helper must include it.
    fn clear_stored(&self) {
        let state_home = self.install.env.state_home();
        let index_dir = state_home.join("pohunek").join("plugins").join("index");
        assert!(fs::remove_dir_all(&index_dir).is_ok() || !index_dir.exists());
    }

    /// Requests the test server saw (paths, in order).
    fn request_count(&self) -> usize {
        self.server.state.requests.lock().expect("requests").len()
    }

    /// Clear the server request history between checks.
    fn clear_requests(&self) {
        self.server.state.requests.lock().expect("requests").clear();
    }

    fn staging_leftovers(&self) -> Vec<String> {
        fs::read_dir(self.install.env.tmp_dir())
            .expect("read the private TMPDIR")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("pohunek-plugin-"))
            .collect()
    }

    async fn assert_nothing_installed(&self) {
        assert!(
            self.install.packages().await.is_empty(),
            "a refused install records nothing"
        );
        assert!(
            self.staging_leftovers().is_empty(),
            "the staging directory is removed"
        );
    }
}

/// Loopback port the closed-address scenario connects to.
///
/// Port 0 is not a destination: no socket is ever bound to it, so a connect
/// fails at once on every OS and in every network namespace, whatever the
/// privileged-port range or the rig's listeners are (Linux refuses the
/// connection, macOS reports an unassignable address). A port that a test holds
/// bound without listening is not used because macOS drops, instead of
/// refusing, a connect to it and the request times out.
const CLOSED_PORT: u16 = 0;

fn archive_path(version: &str) -> String {
    format!("/{version}.tar.zst")
}

fn err_code(document: &Value) -> &str {
    document["err"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("an error document: {document}"))
}

/// Runs `pohunek <arguments>` in the rig's environment without `--json`.
async fn text(rig: &Rig, arguments: &[&str]) -> (i32, String, String) {
    let output = rig
        .install
        .env
        .tokio_command(bin_exe("pohunek"))
        .args(arguments)
        .output()
        .await
        .expect("run the pohunek binary");
    (
        output.status.code().expect("exit code"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Path of the schema 2 index document on the loopback server.
const INDEX_PATH_V2: &str = "/official-index-v2.json";

/// Environment variables that replace the official index URLs in a test build.
const OFFICIAL_URL_V1_VARIABLE: &str = "POHUNEK_TEST_OFFICIAL_INDEX_URL";
const OFFICIAL_URL_V2_VARIABLE: &str = "POHUNEK_TEST_OFFICIAL_INDEX_URL_V2";

/// Seconds a stored copy is aged to count as older than 24 hours.
const STALE_AGE: u64 = 25 * DAY;

/// How long the short-lived fixture index lives, and the pause granularity
/// while a test waits for it to expire.
const SHORT_LIFETIME: u64 = 3;
const EXPIRY_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// What one CLI run printed.
struct Run {
    code: i32,
    document: Value,
    stderr: String,
}

impl Rig {
    /// Official index URLs that point at this rig's server.
    fn official_urls(&self) -> [(&'static str, String); 2] {
        [
            (OFFICIAL_URL_V1_VARIABLE, self.server.url(INDEX_PATH)),
            (OFFICIAL_URL_V2_VARIABLE, self.server.url(INDEX_PATH_V2)),
        ]
    }

    /// Official index URLs that nothing answers on.
    fn dead_urls() -> [(&'static str, String); 2] {
        [
            (
                OFFICIAL_URL_V1_VARIABLE,
                format!("http://127.0.0.1:{CLOSED_PORT}{INDEX_PATH}"),
            ),
            (
                OFFICIAL_URL_V2_VARIABLE,
                format!("http://127.0.0.1:{CLOSED_PORT}{INDEX_PATH_V2}"),
            ),
        ]
    }

    /// `pohunek <arguments> --json` with `env` added to the environment.
    async fn run(&self, arguments: &[&str], env: &[(&str, String)]) -> Run {
        let mut all = arguments.to_vec();
        all.push("--json");
        let extra: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let output = self.install.run_with_env(&all, &extra).await;
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout of {all:?} is one JSON document ({error}): {} / {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        Run {
            code: output.status.code().expect("exit code"),
            document,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// `plugin index refresh --index-url <url> [extra]`.
    async fn refresh_from(&self, url: &str, extra: &[&str]) -> (i32, Value) {
        let mut arguments = vec!["plugin", "index", "refresh", "--index-url", url];
        arguments.extend_from_slice(extra);
        self.install.json(&arguments).await
    }

    /// A refresh from the schema 1 document that must succeed.
    async fn refresh(&self) -> Value {
        let (code, document) = self.refresh_from(&self.index_url(), &[]).await;
        assert_eq!(code, 0, "{document}");
        document
    }

    /// The plugin root of the rig's daemon.
    fn plugins_dir(&self) -> PathBuf {
        self.install
            .env
            .state_home()
            .join("pohunek")
            .join("plugins")
    }

    /// Rewrites the stored-at clock of every stored schema in the sidecar to
    /// `age` seconds ago. The CLI and the daemon read the real clock, so the
    /// age is set on disk.
    fn age_stored(&self, age: u64) {
        let sidecar_path = self.plugins_dir().join("index").join("stored.json");
        let raw = fs::read(&sidecar_path).expect("read the sidecar");
        let mut sidecar: Value = serde_json::from_slice(&raw).expect("the sidecar is JSON");
        for key in ["official-v1", "official-v2"] {
            if sidecar.get(key).is_some() {
                sidecar[key] = serde_json::json!(now() - age);
            }
        }
        fs::write(&sidecar_path, serde_json::to_vec(&sidecar).expect("json"))
            .expect("write the sidecar");
        fs::set_permissions(&sidecar_path, fs::Permissions::from_mode(0o600))
            .expect("sidecar mode");
    }

    /// The `plugin_index` check of `pohunek doctor`: exit code, status, detail.
    async fn doctor_index(&self) -> (i32, String, String) {
        let (code, document) = self.install.json(&["doctor"]).await;
        let check = document["ok"]["checks"]
            .as_array()
            .unwrap_or_else(|| panic!("doctor lists checks: {document}"))
            .iter()
            .find(|check| check["name"] == "plugin_index")
            .unwrap_or_else(|| panic!("doctor reports plugin_index: {document}"))
            .clone();
        (
            code,
            check["status"].as_str().expect("status").to_owned(),
            check["detail"].as_str().expect("detail").to_owned(),
        )
    }

    /// Whether the host launches a session of `runtime`.
    async fn serves(&self, runtime: &str) -> bool {
        let cwd = self
            .install
            .env
            .cwd()
            .to_str()
            .expect("utf-8 cwd")
            .to_owned();
        let (code, document) = self
            .install
            .json(&["session", "new", "--agent", runtime, "--cwd", &cwd])
            .await;
        if code != 0 {
            return false;
        }
        let session = document["ok"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("a session id in {document}"))
            .to_owned();
        let (code, document) = self.install.json(&["session", "rm", &session]).await;
        assert_eq!(code, 0, "{document}");
        true
    }

    /// The `index_high_water` mark the daemon persisted.
    fn index_high_water(&self) -> u64 {
        let bytes = fs::read(self.plugins_dir().join("index-state.json")).expect("read the state");
        let state: Value = serde_json::from_slice(&bytes).expect("the state is JSON");
        state["index_high_water"]
            .as_u64()
            .expect("a high-water mark")
    }

    /// Versions of the installed packages, selected ones marked.
    async fn selected_versions(&self) -> Vec<String> {
        self.install
            .packages()
            .await
            .iter()
            .filter(|package| package["selected"] == true)
            .map(|package| {
                package["package"]["version"]
                    .as_str()
                    .expect("version")
                    .to_owned()
            })
            .collect()
    }

    /// Blocks until `expires_at` of a short-lived document has passed.
    async fn wait_for_expiry(&self, sequence: u64) {
        while now() < sequence + SHORT_LIFETIME {
            // timing-allowed: #1614 the daemon checks expiry against the wall clock, so a signed document can only expire in real time
            tokio::time::sleep(EXPIRY_POLL).await;
        }
    }
}

#[tokio::test]
async fn install_by_id_takes_the_highest_version_and_a_pinned_version_that_one() {
    let rig = Rig::start().await;
    let low = rig.listing("0.1.0");
    let high = rig.listing("0.2.0");
    let unordered = rig.listing("2.0");
    rig.publish(&[low, high, unordered]);

    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["status"], "installed", "{document}");
    let packages = rig.install.packages().await;
    assert_eq!(packages.len(), 1, "{packages:?}");
    assert_eq!(packages[0]["package"]["version"], "0.2.0");
    assert_eq!(packages[0]["origin"], "official");
    assert_eq!(packages[0]["enabled"], true);
    assert_eq!(packages[0]["selected"], true);
    assert!(rig.staging_leftovers().is_empty(), "staging is removed");

    let (code, document) = rig
        .install(&format!("{PACKAGE_ID}@0.1.0"), &["--yes"])
        .await;
    assert_eq!(code, 0, "{document}");
    let packages = rig.install.packages().await;
    let versions: Vec<_> = packages
        .iter()
        .map(|package| package["package"]["version"].as_str().expect("version"))
        .collect();
    assert!(
        versions.contains(&"0.1.0") && versions.contains(&"0.2.0"),
        "{versions:?}"
    );
    let selected: Vec<_> = packages
        .iter()
        .filter(|package| package["selected"] == true)
        .collect();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0]["package"]["version"], "0.2.0");
}

/// Scenario 4 (24 h cadence): installs within 24 h of a refresh fetch no index
/// (the stored copy resolves the version); once the stored copy is older than
/// 24 h the next install refreshes it with exactly one index fetch. The clock
/// is not injected: the sidecar's `stored_at` is aged on disk.
#[tokio::test]
async fn scenario_4_stored_copy_fresh_24h_no_fetch_then_refresh_once() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0"), rig.listing("0.2.0")]);
    rig.clear_stored();
    rig.clear_requests();

    // `plugin index refresh --index-url` fetches the document once and the
    // daemon stores it with its sidecar.
    let index_url = rig.index_url();
    let (code, document) = rig
        .install
        .json(&["plugin", "index", "refresh", "--index-url", &index_url])
        .await;
    assert_eq!(code, 0, "{document}");
    let refresh_requests = rig.server.requests();
    assert_eq!(
        refresh_requests.first(),
        Some(&INDEX_PATH.to_owned()),
        "the refresh fetches the index once: {refresh_requests:?}"
    );
    assert_eq!(refresh_requests.len(), 1, "{refresh_requests:?}");
    let index_dir = rig
        .install
        .env
        .state_home()
        .join("pohunek")
        .join("plugins")
        .join("index");
    assert!(index_dir.join("stored.json").exists(), "sidecar exists");
    assert!(
        index_dir.join("official-v1.json").exists(),
        "stored document exists"
    );
    rig.clear_requests();

    // Within 24 h: no index fetch, only the archive.
    let (code, document) = rig
        .install
        .json(&["plugin", "install", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 0, "{document}");
    let first_requests = rig.server.requests();
    assert!(
        !first_requests.contains(&INDEX_PATH.to_owned()),
        "no index fetch within 24h: {first_requests:?}"
    );
    assert_eq!(first_requests.len(), 1, "{first_requests:?}");

    rig.clear_requests();
    let (code, document) = rig
        .install
        .json(&["plugin", "install", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 0, "{document}");
    let second_requests = rig.server.requests();
    assert!(
        !second_requests.contains(&INDEX_PATH.to_owned()),
        "no index fetch within 24h: {second_requests:?}"
    );
    assert_eq!(second_requests.len(), 1, "{second_requests:?}");

    // The clock is not injected: the sidecar's `stored_at` is aged on disk.
    // The stale copy makes the install refresh once from the official URLs
    // (the loopback server): the schema 2 document, then the archive.
    rig.age_stored(STALE_AGE);
    rig.clear_requests();
    let run = rig
        .run(
            &["plugin", "install", PACKAGE_ID, "--yes"],
            &rig.official_urls(),
        )
        .await;
    assert_eq!(run.code, 0, "{} / {}", run.document, run.stderr);
    let third_requests = rig.server.requests();
    assert_eq!(
        third_requests,
        vec![INDEX_PATH_V2.to_owned(), archive_path("0.2.0")],
        "a stale stored copy is refreshed exactly once"
    );
    // An explicit refresh also fetches exactly once.
    rig.clear_requests();
    let (code, document) = rig
        .install
        .json(&["plugin", "index", "refresh", "--index-url", &index_url])
        .await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(rig.server.requests(), vec![INDEX_PATH.to_owned()]);
}

#[tokio::test]
async fn an_archive_that_differs_from_the_listed_digest_is_never_sent_to_the_daemon() {
    let rig = Rig::start().await;
    let listing = rig.listing("0.1.0");
    rig.publish(std::slice::from_ref(&listing));
    rig.server
        .set(&archive_path("0.1.0"), Route::Body(archive("0.9.9").bytes));

    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_archive_digest_mismatch");
    rig.assert_nothing_installed().await;
}

#[tokio::test]
async fn the_source_grammar_and_the_url_policy_are_enforced_before_any_fetch() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0")]);
    let url = rig.index_url();
    let digest = format!("sha256:{}", "ab".repeat(32));

    // A file name without a trust flag is never looked up by id.
    let (code, document) = rig
        .install
        .json(&["plugin", "install", "muse.tar.zst"])
        .await;
    assert_eq!(code, 2, "{document}");
    assert_eq!(err_code(&document), "plugin_trust_required");
    let existing = rig.install.write("some-package", b"not an archive");
    let (code, document) = rig
        .install
        .json(&["plugin", "install", existing.to_str().expect("utf-8 path")])
        .await;
    assert_eq!(code, 2, "{document}");
    assert_eq!(err_code(&document), "plugin_trust_required");

    // A source that is neither a package reference nor a file.
    for source in ["Not A Package", "pohunek.runtime.idxfix@", "@0.1.0"] {
        let (code, document) = rig
            .install
            .json(&["plugin", "install", source, "--index-url", &url])
            .await;
        assert_eq!(code, 2, "{source}: {document}");
        assert_eq!(err_code(&document), "plugin_source_invalid", "{source}");
    }

    // --index-url conflicts with every trust flag; install takes at most one.
    for flag in [
        vec!["--sha256", digest.as_str()],
        vec!["--catalog", "catalog.json"],
        vec!["--index", "index.json"],
    ] {
        let mut arguments = vec!["plugin", "install", "x.tar", "--index-url", "https://e"];
        arguments.extend(flag);
        let (code, document) = rig.install.json(&arguments).await;
        assert_eq!(code, 2, "{document}");
        assert_eq!(err_code(&document), "cli_usage");
    }
    let (code, document) = rig
        .install
        .json(&[
            "plugin",
            "install",
            "x.tar",
            "--sha256",
            &digest,
            "--index",
            "index.json",
        ])
        .await;
    assert_eq!(code, 2, "{document}");
    assert_eq!(err_code(&document), "cli_usage");

    // Update still requires a trust flag.
    let (code, document) = rig
        .install
        .json(&["plugin", "update", PACKAGE_ID, "x.tar"])
        .await;
    assert_eq!(code, 2, "{document}");
    assert_eq!(err_code(&document), "cli_usage");

    // Plain http to a foreign host is refused without a request.
    let (code, document) = rig
        .install
        .json(&["plugin", "install", PACKAGE_ID, "--index-url", FOREIGN_URL])
        .await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_url_refused");
    for refused in ["ftp://127.0.0.1/index.json", "http://user:pw@127.0.0.1/x"] {
        let (code, document) = rig
            .install
            .json(&["plugin", "install", PACKAGE_ID, "--index-url", refused])
            .await;
        assert_eq!(code, 1, "{refused}: {document}");
        assert_eq!(err_code(&document), "plugin_index_url_refused", "{refused}");
    }
    assert!(
        rig.server.requests().is_empty(),
        "{:?}",
        rig.server.requests()
    );
    rig.assert_nothing_installed().await;
}

#[tokio::test]
async fn an_entry_url_or_redirect_to_a_refused_target_stops_before_the_request() {
    let rig = Rig::start().await;
    let mut listing = rig.listing("0.1.0");
    listing.url = FOREIGN_URL.to_owned();
    rig.publish(std::slice::from_ref(&listing));

    rig.clear_stored();
    rig.clear_requests();
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_url_refused");
    // The refused fetch is attempted once; there is no fallback retry on a
    // fatal (non-miss) error.
    assert_eq!(rig.request_count(), 1, "{:?}", rig.server.requests());
    assert_eq!(rig.server.requests(), vec![INDEX_PATH]);

    // A redirect of the index to a foreign host is refused before the hop.
    rig.server
        .set(INDEX_PATH, Route::Redirect(FOREIGN_URL.to_owned()));
    rig.clear_stored();
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_url_refused");

    // A redirect of the archive to a foreign host is refused before the hop and
    // is not reported as an unavailable archive.
    rig.publish(&[rig.listing("0.1.0")]);
    rig.server.set(
        &archive_path("0.1.0"),
        Route::Redirect(FOREIGN_URL.to_owned()),
    );
    rig.clear_stored();
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_url_refused");
    rig.assert_nothing_installed().await;

    // Redirects between loopback URLs are followed, for the index and the archive.
    let moved = rig.listing("0.1.0");
    rig.publish(std::slice::from_ref(&moved));
    let signed = rig.keys.signed(vec![Listing::new(
        "0.1.0",
        rig.server.url("/archive-moved"),
    )
    .entry()]);
    rig.server.set("/index-moved", Route::Body(signed));
    rig.server
        .set(INDEX_PATH, Route::Redirect(rig.server.url("/index-moved")));
    rig.server.set(
        "/archive-moved",
        Route::Redirect(rig.server.url(&archive_path("0.1.0"))),
    );
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(rig.install.packages().await.len(), 1);
}

#[tokio::test]
async fn a_chain_of_ten_redirects_is_followed_and_an_eleventh_hop_is_refused() {
    let rig = Rig::start().await;
    let listing = rig.listing("0.1.0");
    rig.publish(std::slice::from_ref(&listing));
    let signed = rig.keys.signed(vec![listing.entry()]);
    rig.server.set("/hop-end", Route::Body(signed));

    // INDEX_PATH redirects to /hop-1, which redirects to /hop-2, and so on:
    // `hops` redirects in all, the last one landing on the index body.
    let chain = |hops: usize| {
        for hop in 0..hops {
            let from = if hop == 0 {
                INDEX_PATH.to_owned()
            } else {
                format!("/hop-{hop}")
            };
            let to = if hop + 1 == hops {
                "/hop-end".to_owned()
            } else {
                format!("/hop-{}", hop + 1)
            };
            rig.server.set(&from, Route::Redirect(rig.server.url(&to)));
        }
    };

    chain(10);
    rig.clear_stored();
    // The second hop must be observed after refresh: requests are counted
    // fresh between the two chains so the redirect limit is measured alone.
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 0, "{document}");
    rig.clear_requests();

    // The 11-hop case must be observed in the install fetch: clear the
    // stored copy so `stage_from_stored` misses and the fetch (refresh +
    // stage) runs and hits the redirect limit.
    rig.clear_stored();
    rig.clear_requests();
    chain(11);
    let (code, document) = rig.install(PACKAGE_ID, &["--yes", "--no-enable"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_unavailable");
    assert!(
        document["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains("too many redirects")),
        "{document}"
    );
}

#[tokio::test]
async fn oversized_and_unavailable_downloads_fail_without_a_daemon_call() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0")]);

    rig.server.set(INDEX_PATH, Route::Endless(OVER_INDEX_CAP));
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_download_too_large");
    assert_eq!(document["err"]["class"], "transport", "{document}");

    rig.server.set(INDEX_PATH, Route::Status(404));
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_unavailable");

    // A connection that cannot be made is the same failure as a status error.
    let unreachable = format!("http://127.0.0.1:{CLOSED_PORT}{INDEX_PATH}");
    let (code, document) = rig
        .install
        .json(&[
            "plugin",
            "install",
            PACKAGE_ID,
            "--index-url",
            &unreachable,
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_unavailable");
    assert_eq!(document["err"]["class"], "transport", "{document}");
    assert!(
        document["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains("cannot be reached")),
        "{document}"
    );

    rig.publish(&[rig.listing("0.1.0")]);
    rig.server
        .set(&archive_path("0.1.0"), Route::Endless(OVER_ARCHIVE_CAP));
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_download_too_large");

    rig.server.set(&archive_path("0.1.0"), Route::Status(404));
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_archive_unavailable");
    assert_eq!(document["err"]["class"], "transport", "{document}");
    rig.assert_nothing_installed().await;
}

#[tokio::test]
async fn a_daemon_without_the_channel_refuses_index_installs_before_any_fetch() {
    let keys = Keys::new();
    let install = Install::start_with_channel(&keys.anchor(), None).await;
    let server = Server::start();
    let archive = archive("0.1.0");
    let listing = Listing::new("0.1.0", server.url(&archive_path("0.1.0")));
    server.set(INDEX_PATH, Route::Body(keys.signed(vec![listing.entry()])));
    let index_file = install.write("index.json", &keys.signed(vec![listing.entry()]));
    let archive_file = install.write("0.1.0.tar.zst", &archive.bytes);

    let url = server.url(INDEX_PATH);
    let (code, document) = install
        .json(&[
            "plugin",
            "install",
            PACKAGE_ID,
            "--index-url",
            &url,
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_unsupported");
    assert!(server.requests().is_empty(), "{:?}", server.requests());

    let (code, document) = install
        .json(&[
            "plugin",
            "install",
            archive_file.to_str().expect("utf-8 path"),
            "--index",
            index_file.to_str().expect("utf-8 path"),
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_unsupported");
    assert!(install.packages().await.is_empty());

    let (code, document) = install
        .json(&[
            "plugin",
            "update",
            PACKAGE_ID,
            archive_file.to_str().expect("utf-8 path"),
            "--index",
            index_file.to_str().expect("utf-8 path"),
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_unsupported");
    assert!(install.packages().await.is_empty());
}

#[tokio::test]
async fn an_index_the_cli_cannot_use_is_reported_before_the_archive_is_requested() {
    let rig = Rig::start().await;
    let listing = rig.listing("0.1.0");
    rig.publish(std::slice::from_ref(&listing));

    // A duplicate key is rejected by the strict parser both sides share.
    let valid = String::from_utf8(rig.keys.signed(vec![listing.entry()])).expect("utf-8");
    let duplicated = valid.replacen('{', "{\"schema_version\":1,", 1);
    rig.server
        .set(INDEX_PATH, Route::Body(duplicated.into_bytes()));
    rig.clear_stored();
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_invalid");

    // An expired index.
    rig.server.set(
        INDEX_PATH,
        Route::Body(
            rig.keys
                .document(&rig.keys.key, now() - 8 * DAY, vec![listing.entry()]),
        ),
    );
    rig.clear_stored();
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_expired");
    assert!(
        !rig.server.requests().contains(&archive_path("0.1.0")),
        "{:?}",
        rig.server.requests()
    );

    // Entries for another package, another platform, or another core.
    let mut foreign_platform = rig.listing("0.1.0");
    foreign_platform.platform = "m68k-unknown-linux-gnu".to_owned();
    let mut future_core = rig.listing("0.2.0");
    future_core.core = ">=999.0.0";
    rig.server.set(
        INDEX_PATH,
        Route::Body(
            rig.keys
                .signed(vec![foreign_platform.entry(), future_core.entry()]),
        ),
    );
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_package_not_found");
    let (code, document) = rig.install("pohunek.runtime.other", &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_package_not_found");

    rig.publish(std::slice::from_ref(&listing));
    let (code, document) = rig
        .install(&format!("{PACKAGE_ID}@9.9.9"), &["--yes"])
        .await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_version_not_found");
    assert!(
        !rig.server.requests().contains(&archive_path("0.1.0")),
        "{:?}",
        rig.server.requests()
    );
    rig.assert_nothing_installed().await;
}

#[tokio::test]
async fn the_review_names_the_resolved_version_and_the_pinned_rerun_installs_it() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0"), rig.listing("0.2.0")]);
    let url = rig.index_url();

    let (code, stdout, stderr) = text(
        &rig,
        &["plugin", "install", PACKAGE_ID, "--index-url", &url],
    )
    .await;
    assert_eq!(code, 1, "{stdout} {stderr}");
    let pinned = format!("pohunek plugin install {PACKAGE_ID}@0.2.0 --yes");
    let digest = archive("0.2.0").digest;
    assert!(stdout.contains(&format!("{PACKAGE_ID}@0.2.0")), "{stdout}");
    assert!(stdout.contains(digest.as_str()), "{stdout}");
    assert!(stderr.contains(&pinned), "{stderr}");
    assert!(
        stderr.contains(&format!("--index-url '{url}'")),
        "the repeat keeps the index the review used: {stderr}"
    );
    rig.assert_nothing_installed().await;

    let (code, document) = rig.install(PACKAGE_ID, &[]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "consent_required");
    assert!(
        document["err"]["recover"]
            .as_str()
            .expect("recover")
            .contains(&pinned),
        "{document}"
    );

    // The index moves on to a newer version; the pinned rerun installs the
    // reviewed one.
    rig.publish(&[
        rig.listing("0.1.0"),
        rig.listing("0.2.0"),
        rig.listing("0.3.0"),
    ]);
    let (code, document) = rig
        .install(&format!("{PACKAGE_ID}@0.2.0"), &["--yes"])
        .await;
    assert_eq!(code, 0, "{document}");
    let packages = rig.install.packages().await;
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0]["package"]["version"], "0.2.0");
    assert!(rig.staging_leftovers().is_empty());
}

#[tokio::test]
async fn the_daemon_refuses_an_index_its_anchor_does_not_endorse() {
    let rig = Rig::start().await;
    let listing = rig.listing("0.1.0");
    let outsider = test_key();
    rig.server.set(
        INDEX_PATH,
        Route::Body(rig.keys.document(&outsider, now(), vec![listing.entry()])),
    );
    rig.server.set(
        &archive_path("0.1.0"),
        Route::Body(listing.archive.bytes.clone()),
    );

    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert!(
        err_code(&document).starts_with("package_"),
        "the refusal is the daemon's: {document}"
    );
    assert!(
        rig.server.requests().contains(&archive_path("0.1.0")),
        "the CLI forwards what it fetched: {:?}",
        rig.server.requests()
    );
    rig.assert_nothing_installed().await;
}

#[tokio::test]
async fn an_index_file_authorizes_install_and_update_of_a_local_archive() {
    let rig = Rig::start().await;
    let first = rig.listing("0.1.0");
    let second = rig.listing("0.2.0");
    let index_file = rig.install.write(
        "index.json",
        &rig.keys.signed(vec![first.entry(), second.entry()]),
    );
    let first_file = rig.install.write("0.1.0.tar.zst", &first.archive.bytes);
    let second_file = rig.install.write("0.2.0.tar.zst", &second.archive.bytes);
    let path = |file: &PathBuf| file.to_str().expect("utf-8 path").to_owned();

    let (code, document) = rig
        .install
        .json(&[
            "plugin",
            "install",
            &path(&first_file),
            "--index",
            &path(&index_file),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(
        document["ok"]["package"]["origin"], "official",
        "{document}"
    );

    let (code, document) = rig
        .install
        .json(&[
            "plugin",
            "update",
            PACKAGE_ID,
            &path(&second_file),
            "--index",
            &path(&index_file),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{document}");
    let packages = rig.install.packages().await;
    let selected: Vec<_> = packages
        .iter()
        .filter(|package| package["selected"] == true)
        .collect();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0]["package"]["version"], "0.2.0");
    assert!(
        rig.server.requests().is_empty(),
        "no network for a local archive"
    );
}

/// Package id of the indexed context fixture package.
const CONTEXT_PACKAGE_ID: &str = "pohunek.context.idxctx";

/// A context package archive: a descriptor that exports a directory of files
/// and nothing else.
fn context_archive(version: &str) -> Archive {
    let descriptor = format!(
        "schema = 1\nid = \"{CONTEXT_PACKAGE_ID}\"\nversion = \"{version}\"\nruntime_api = 1\nrequires = [\"package-context-v1\"]\n\n[context]\ndir = \"bundle\"\n"
    );
    let entries = [
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: descriptor.into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "bundle/index.md".to_owned(),
            contents: b"# Index\n".to_vec(),
            executable: false,
        },
    ];
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    Archive { bytes, digest }
}

impl Rig {
    /// A `context` entry for `archive` served at `/<version>.tar.zst`.
    fn context_entry(&self, archive: &Archive, version: &str) -> IndexEntry {
        IndexEntry {
            package_id: PackageId::parse(CONTEXT_PACKAGE_ID).expect("package id"),
            kind: Some(IndexEntryKind::Context),
            runtime_id: None,
            version: PackageVersion::parse(version).expect("version"),
            digest: archive.digest.clone(),
            url: self.server.url(&archive_path(version)),
            platforms: vec![host_platform()],
            core: ANY_CORE.to_owned(),
        }
    }

    /// Serves `entries` as a schema 2 index and `archive` at its version path.
    fn publish_v2(&self, entries: Vec<IndexEntry>, archives: &[(&str, &Archive)]) {
        self.server
            .set(INDEX_PATH, Route::Body(self.keys.signed_v2(entries)));
        for (version, archive) in archives {
            self.server
                .set(&archive_path(version), Route::Body(archive.bytes.clone()));
        }
    }
}

#[tokio::test]
async fn a_context_package_installs_through_a_schema_2_index() {
    let rig = Rig::start().await;
    let archive = context_archive("0.1.0");
    let entry = rig.context_entry(&archive, "0.1.0");
    rig.publish_v2(vec![entry], &[("0.1.0", &archive)]);

    let (code, document) = rig.install(CONTEXT_PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["status"], "installed", "{document}");
    let packages = rig.install.packages().await;
    assert_eq!(packages.len(), 1, "{packages:?}");
    assert_eq!(packages[0]["package"]["id"], CONTEXT_PACKAGE_ID);
    assert_eq!(packages[0]["origin"], "official");
    assert_eq!(packages[0]["selected"], true);
    assert!(rig.staging_leftovers().is_empty(), "staging is removed");
}

#[tokio::test]
async fn a_context_package_is_not_resolved_from_a_schema_1_index() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0")]);

    let (code, document) = rig.install(CONTEXT_PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_package_not_found");
    rig.assert_nothing_installed().await;
}

#[tokio::test]
async fn a_runtime_entry_never_authorizes_a_context_archive() {
    let rig = Rig::start().await;
    let context = context_archive("0.1.0");
    // A runtime entry that lists the context archive's digest.
    let listing = rig.listing("0.1.0");
    let mut entry = listing.entry();
    entry.kind = Some(IndexEntryKind::Runtime);
    entry.digest = context.digest.clone();
    rig.publish_v2(vec![entry], &[("0.1.0", &context)]);

    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "package_untrusted", "{document}");
    rig.assert_nothing_installed().await;
}

#[tokio::test]
async fn a_context_entry_never_authorizes_a_runtime_archive() {
    let rig = Rig::start().await;
    let runtime = archive("0.1.0");
    let mut entry = rig.context_entry(&runtime, "0.1.0");
    entry.digest = runtime.digest.clone();
    rig.publish_v2(vec![entry], &[("0.1.0", &runtime)]);

    let (code, document) = rig.install(CONTEXT_PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "package_untrusted", "{document}");
    rig.assert_nothing_installed().await;
}

/// A daemon without the index channel refuses a by-id install before any
/// fetch, with or without `--index-url`.
#[tokio::test]
async fn a_daemon_without_the_channel_refuses_a_by_id_install_before_any_fetch() {
    let keys = Keys::new();
    let install = Install::start_with_channel(&keys.anchor(), None).await;
    let server = Server::start();
    let urls = [
        (OFFICIAL_URL_V1_VARIABLE, server.url(INDEX_PATH)),
        (OFFICIAL_URL_V2_VARIABLE, server.url(INDEX_PATH_V2)),
    ];
    let env: Vec<(&str, &str)> = urls.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let url = server.url(INDEX_PATH);
    for extra in [&["--index-url", url.as_str()][..], &[][..]] {
        let mut arguments = vec!["plugin", "install", PACKAGE_ID, "--yes"];
        arguments.extend_from_slice(extra);
        arguments.push("--json");
        let output = install.run_with_env(&arguments, &env).await;
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
        assert_eq!(err_code(&document), "plugin_index_unsupported");
        assert!(server.requests().is_empty(), "{:?}", server.requests());
    }
}

#[tokio::test]
async fn a_schema_2_index_still_installs_runtime_packages() {
    let rig = Rig::start().await;
    let listing = rig.listing("0.1.0");
    let mut entry = listing.entry();
    entry.kind = Some(IndexEntryKind::Runtime);
    rig.publish_v2(vec![entry], &[("0.1.0", &listing.archive)]);

    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(rig.install.packages().await.len(), 1);
}

/// What a fake daemon received.
#[derive(Clone, Default)]
struct Received {
    methods: Arc<Mutex<Vec<String>>>,
    installs: Arc<Mutex<Vec<Value>>>,
}

impl Received {
    fn methods(&self) -> Vec<String> {
        self.methods.lock().expect("methods").clone()
    }

    fn installs(&self) -> Vec<Value> {
        self.installs.lock().expect("installs").clone()
    }
}

/// Serves a daemon on `socket` until the test process ends: `host.inspect`
/// reports `capabilities` on top of a bare host, `package.list` is empty,
/// `package.install` and every other method answer an error. Every method it
/// receives is recorded.
fn serve_fake_daemon(socket: &std::path::Path, capabilities: Value) -> Received {
    use protocol::{ErrorClass, ProtocolError, Request, Response, PROTOCOL_VERSION};

    let received = Received::default();
    let listener = std::os::unix::net::UnixListener::bind(socket).expect("bind the daemon socket");
    let shared = received.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let received = shared.clone();
            let capabilities = capabilities.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let request: Request =
                        serde_json::from_str(line.trim_end()).expect("parse a request");
                    received
                        .methods
                        .lock()
                        .expect("methods")
                        .push(request.method().to_owned());
                    let result = match request.method() {
                        protocol::method::DAEMON_HEALTH => Ok(serde_json::json!({
                            "status": "ok",
                            "daemon_version": "fixture",
                            "protocol_version": PROTOCOL_VERSION.get()
                        })),
                        protocol::method::HOST_INSPECT => {
                            let mut host = serde_json::json!({
                                "daemon_version": "fixture",
                                "protocol_version": PROTOCOL_VERSION.get(),
                                "supported_agents": [],
                                "runtimes": [],
                                "git_available": false,
                                "worktree_supported": false,
                                "terminal_read_supported": false,
                                "output_read_supported": false,
                                "session_wait_supported": false
                            });
                            for (name, value) in capabilities.as_object().expect("capabilities") {
                                host[name] = value.clone();
                            }
                            Ok(host)
                        }
                        protocol::method::PACKAGE_LIST => {
                            Ok(serde_json::json!({"generation": 0, "packages": []}))
                        }
                        other => {
                            if other == protocol::method::PACKAGE_INSTALL {
                                received
                                    .installs
                                    .lock()
                                    .expect("installs")
                                    .push(request.params().clone());
                            }
                            Err(ProtocolError::new(
                                ErrorClass::Runtime,
                                "fixture_request_refused",
                                format!("the fixture daemon refuses {other}"),
                                None,
                            ))
                        }
                    };
                    let response = match result {
                        Ok(value) => Response::ok(PROTOCOL_VERSION, request.id(), value),
                        Err(error) => Response::err(PROTOCOL_VERSION, request.id(), error),
                    }
                    .expect("valid fixture response");
                    writeln!(
                        reader.get_mut(),
                        "{}",
                        serde_json::to_string(&response).expect("serialize the response")
                    )
                    .expect("write the fixture response");
                }
            });
        }
    });
    received
}

/// A private environment whose control socket is served by a fake daemon.
fn fake_daemon_env(capabilities: Value) -> (pohunek_test_support::env::TestEnv, Received) {
    let env = pohunek_test_support::env::TestEnv::new().expect("private environment");
    let runtime = env.runtime_dir().join("pohunek");
    fs::create_dir(&runtime).expect("create the socket directory");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))
        .expect("make the socket directory private");
    let received = serve_fake_daemon(&runtime.join("daemon.sock"), capabilities);
    (env, received)
}

/// A daemon that reads the index but has no `plugin.index.apply` has no stored
/// copy to read or refresh: a by-id install fetches the official index
/// directly and hands the archive to `package.install`.
#[test]
fn a_daemon_without_apply_support_installs_through_the_direct_fetch() {
    let (env, received) = fake_daemon_env(serde_json::json!({
        "package_install_index_supported": true,
        "package_index_apply_supported": false
    }));
    let keys = Keys::new();
    let server = Server::start();
    let listing = Listing::new("0.1.0", server.url(&archive_path("0.1.0")));
    server.set(INDEX_PATH, Route::Body(keys.signed(vec![listing.entry()])));
    server.set(&archive_path("0.1.0"), Route::Body(listing.archive.bytes));

    let output = env
        .command(bin_exe("pohunek"))
        .args(["plugin", "install", PACKAGE_ID, "--yes", "--json"])
        .env(OFFICIAL_URL_V1_VARIABLE, server.url(INDEX_PATH))
        .env(OFFICIAL_URL_V2_VARIABLE, server.url(INDEX_PATH_V2))
        .output()
        .expect("run pohunek plugin install");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(err_code(&document), "fixture_request_refused", "{document}");
    let methods = received.methods();
    assert!(
        methods
            .iter()
            .any(|m| m == protocol::method::PACKAGE_INSTALL),
        "the CLI reached package.install: {methods:?}"
    );
    assert!(
        !methods
            .iter()
            .any(|m| m == protocol::method::PLUGIN_INDEX_APPLY),
        "no stored copy is refreshed on a daemon without apply support: {methods:?}"
    );
    let installs = received.installs();
    assert_eq!(installs[0]["trust"]["kind"], "index", "{installs:?}");
    assert_eq!(
        server.requests(),
        vec![INDEX_PATH.to_owned(), archive_path("0.1.0")],
        "exactly the index and the archive are fetched"
    );
}

/// A daemon that reads the index but not context entries refuses a context
/// install before the CLI fetches anything.
#[test]
fn a_daemon_without_the_context_flag_refuses_a_context_install_before_any_fetch() {
    let (env, _received) = fake_daemon_env(serde_json::json!({
        "package_install_index_supported": true
    }));
    let server = Server::start();

    let output = env
        .command(bin_exe("pohunek"))
        .args([
            "plugin",
            "install",
            CONTEXT_PACKAGE_ID,
            "--index-url",
            &server.url(INDEX_PATH),
            "--yes",
            "--json",
        ])
        .output()
        .expect("run pohunek plugin install");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(
        err_code(&document),
        "plugin_index_context_unsupported",
        "{document}"
    );
    assert!(
        server.requests().is_empty(),
        "nothing is fetched: {:?}",
        server.requests()
    );
}

/// Package ids and runtime ids of the local community fixtures.
const LOCAL_PACKAGE_ID: &str = "community.idxloc";
const LOCAL_RUNTIME: &str = "idxloc";
const RESERVED_PACKAGE_ID: &str = "pohunek.runtime.idxres";
const RESERVED_RUNTIME: &str = "idxres";

/// A by-id install with no stored copy and an unreachable official index
/// reports the refresh failure, not an invalid index.
#[tokio::test]
async fn a_failed_refresh_without_a_stored_copy_reports_the_refresh_error() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("1.0.0")]);
    rig.clear_stored();
    rig.clear_requests();

    let run = rig
        .run(
            &["plugin", "install", PACKAGE_ID, "--yes"],
            &Rig::dead_urls(),
        )
        .await;

    assert_eq!(run.code, 1, "{}", run.document);
    assert_eq!(err_code(&run.document), "plugin_index_unavailable");
    assert!(
        rig.server.requests().is_empty(),
        "{:?}",
        rig.server.requests()
    );
    rig.assert_nothing_installed().await;
}

/// `--index-url` replaces the stored copy for one install: the given index is
/// fetched and its version installed although a stored copy lists another.
#[tokio::test]
async fn an_index_url_install_bypasses_the_stored_copy() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0")]);
    rig.refresh().await;
    let newer = rig.listing("0.2.0");
    rig.server.set(
        "/other-index.json",
        Route::Body(rig.keys.signed(vec![newer.entry()])),
    );
    rig.server.set(
        &archive_path("0.2.0"),
        Route::Body(newer.archive.bytes.clone()),
    );
    rig.clear_requests();

    let other = rig.server.url("/other-index.json");
    let (code, document) = rig
        .install
        .json(&[
            "plugin",
            "install",
            PACKAGE_ID,
            "--index-url",
            &other,
            "--yes",
        ])
        .await;

    assert_eq!(code, 0, "{document}");
    assert_eq!(
        rig.server.requests(),
        vec!["/other-index.json".to_owned(), archive_path("0.2.0")],
        "the given index is fetched, the stored copy is not used"
    );
    assert_eq!(rig.selected_versions().await, ["0.2.0"]);
}

/// A stale stored copy whose refresh fails still serves the install, and the
/// CLI says why the copy is stale.
#[tokio::test]
async fn an_unreachable_index_falls_back_to_the_unexpired_stored_copy() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0")]);
    rig.refresh().await;
    rig.age_stored(STALE_AGE);
    rig.clear_requests();

    let run = rig
        .run(
            &["plugin", "install", PACKAGE_ID, "--yes"],
            &Rig::dead_urls(),
        )
        .await;

    assert_eq!(run.code, 0, "{} / {}", run.document, run.stderr);
    assert!(
        run.stderr
            .contains("warning: could not refresh the stored plugin index"),
        "{}",
        run.stderr
    );
    assert_eq!(rig.server.requests(), vec![archive_path("0.1.0")]);
    assert_eq!(rig.selected_versions().await, ["0.1.0"]);
}

/// Contract scenario 1: a refresh stores the document, doctor reports the
/// index as current and the daemon's high-water mark is the document sequence.
#[tokio::test]
async fn a_refresh_leaves_doctor_current_and_records_the_sequence() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0")]);

    let document = rig.refresh().await;

    let sequence = document["ok"]["sequence"].as_u64().expect("sequence");
    let (code, status, detail) = rig.doctor_index().await;
    assert_eq!(
        (status.as_str(), detail.as_str()),
        ("ok", "current"),
        "{code}"
    );
    assert_eq!(rig.index_high_water(), sequence);
}

/// Contract scenario 3: an index that expired after it was stored, with the
/// official URL unreachable, fails the install with the refresh error; doctor
/// only warns and the installed package keeps launching.
#[tokio::test]
async fn an_expired_stored_copy_and_an_unreachable_index_fail_with_the_refresh_error() {
    let rig = Rig::start().await;
    let listing = rig.listing("0.1.0");
    rig.publish(std::slice::from_ref(&listing));
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 0, "{document}");
    rig.server.set(
        INDEX_PATH,
        Route::Body(
            rig.keys
                .signed_living(SHORT_LIFETIME, vec![listing.entry()]),
        ),
    );
    let sequence = rig.refresh().await["ok"]["sequence"]
        .as_u64()
        .expect("sequence");
    rig.wait_for_expiry(sequence).await;

    let (code, status, detail) = rig.doctor_index().await;
    assert_eq!(status, "warn", "{detail}");
    assert!(detail.starts_with("package_index_expired"), "{detail}");
    assert_eq!(code, 0, "an expired index is a warning");
    let run = rig
        .run(
            &["plugin", "install", PACKAGE_ID, "--yes"],
            &Rig::dead_urls(),
        )
        .await;
    assert_eq!(run.code, 1, "{}", run.document);
    assert_eq!(err_code(&run.document), "plugin_index_unavailable");
    assert!(rig.serves(RUNTIME).await, "the installed package launches");
    assert_eq!(rig.selected_versions().await, ["0.1.0"]);
}

/// An expired stored copy is never fresh, however recently it was stored: the
/// install refreshes with exactly one index fetch instead of failing on it.
#[tokio::test]
async fn an_expired_stored_copy_younger_than_a_day_is_refreshed() {
    let rig = Rig::start().await;
    let listing = rig.listing("0.1.0");
    rig.server.set(
        INDEX_PATH,
        Route::Body(
            rig.keys
                .signed_living(SHORT_LIFETIME, vec![listing.entry()]),
        ),
    );
    let sequence = rig.refresh().await["ok"]["sequence"]
        .as_u64()
        .expect("sequence");
    rig.wait_for_expiry(sequence).await;
    rig.publish(&[rig.listing("0.1.0"), rig.listing("0.2.0")]);
    rig.clear_requests();

    let run = rig
        .run(
            &["plugin", "install", PACKAGE_ID, "--yes"],
            &rig.official_urls(),
        )
        .await;

    assert_eq!(run.code, 0, "{} / {}", run.document, run.stderr);
    assert_eq!(
        rig.server.requests(),
        vec![INDEX_PATH_V2.to_owned(), archive_path("0.2.0")],
        "one index fetch, then the archive"
    );
    assert_eq!(rig.selected_versions().await, ["0.2.0"]);
}

/// The schema 2 form of a runtime listing.
fn runtime_entry_of_kind(listing: &Listing) -> IndexEntry {
    let mut entry = listing.entry();
    entry.kind = Some(IndexEntryKind::Runtime);
    entry
}

/// A stored schema 1 copy cannot list a context package: the context install
/// refreshes the schema 2 document instead of consulting it.
#[tokio::test]
async fn a_context_install_ignores_a_fresh_schema_1_stored_copy() {
    let rig = Rig::start().await;
    let runtime = rig.listing("0.1.0");
    rig.publish(std::slice::from_ref(&runtime));
    let context = context_archive("0.3.0");
    rig.server.set(
        INDEX_PATH_V2,
        Route::Body(rig.keys.signed_v2(vec![
            runtime_entry_of_kind(&runtime),
            rig.context_entry(&context, "0.3.0"),
        ])),
    );
    rig.server
        .set(&archive_path("0.3.0"), Route::Body(context.bytes.clone()));
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 0, "{document}");
    rig.refresh().await;
    let sidecar = fs::read(rig.plugins_dir().join("index").join("stored.json")).expect("sidecar");
    let sidecar: Value = serde_json::from_slice(&sidecar).expect("sidecar JSON");
    assert!(sidecar.get("official-v1").is_some(), "{sidecar}");
    assert!(sidecar.get("official-v2").is_none(), "{sidecar}");
    rig.clear_requests();

    let run = rig
        .run(
            &["plugin", "install", CONTEXT_PACKAGE_ID, "--yes"],
            &rig.official_urls(),
        )
        .await;

    assert_eq!(run.code, 0, "{} / {}", run.document, run.stderr);
    assert_eq!(
        rig.server.requests(),
        vec![INDEX_PATH_V2.to_owned(), archive_path("0.3.0")]
    );
}

/// A schema 1 install raises the marks the stored schema 2 copy shares; the
/// copy is then below them, so the context install refreshes instead of
/// staging a document the daemon would refuse.
#[tokio::test]
async fn a_context_install_refreshes_a_stored_copy_below_the_marks() {
    let rig = Rig::start().await;
    let runtime = rig.listing("0.1.0");
    rig.publish(std::slice::from_ref(&runtime));
    let context = context_archive("0.3.0");
    let entries = vec![
        runtime_entry_of_kind(&runtime),
        rig.context_entry(&context, "0.3.0"),
    ];
    rig.server.set(
        INDEX_PATH_V2,
        Route::Body(rig.keys.document_of(
            INDEX_SCHEMA_VERSION_2,
            &rig.keys.key,
            now() - 100,
            entries.clone(),
        )),
    );
    rig.server
        .set(&archive_path("0.3.0"), Route::Body(context.bytes.clone()));
    let refreshed = rig
        .run(&["plugin", "index", "refresh"], &rig.official_urls())
        .await;
    assert_eq!(refreshed.code, 0, "{}", refreshed.document);
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 0, "{document}");
    rig.server
        .set(INDEX_PATH_V2, Route::Body(rig.keys.signed_v2(entries)));
    rig.clear_requests();

    let run = rig
        .run(
            &["plugin", "install", CONTEXT_PACKAGE_ID, "--yes"],
            &rig.official_urls(),
        )
        .await;

    assert_eq!(run.code, 0, "{} / {}", run.document, run.stderr);
    assert_eq!(
        rig.server.requests(),
        vec![INDEX_PATH_V2.to_owned(), archive_path("0.3.0")]
    );
}

/// A fresh stored copy that does not list the package makes the install
/// refresh once and install what the refreshed index lists.
#[tokio::test]
async fn a_fresh_stored_copy_without_the_package_is_refreshed_once() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0")]);
    rig.server
        .set(INDEX_PATH, Route::Body(rig.keys.signed(Vec::new())));
    rig.refresh().await;
    // Younger than a day, yet older than the schema 2 copy the refresh stores.
    rig.age_stored(DAY / 24);
    rig.clear_requests();

    let run = rig
        .run(
            &["plugin", "install", PACKAGE_ID, "--yes"],
            &rig.official_urls(),
        )
        .await;

    assert_eq!(run.code, 0, "{} / {}", run.document, run.stderr);
    assert_eq!(
        rig.server.requests(),
        vec![INDEX_PATH_V2.to_owned(), archive_path("0.1.0")]
    );
    assert_eq!(rig.selected_versions().await, ["0.1.0"]);
}

/// Contract scenario 5: an installed package that a later index no longer
/// lists fails doctor with `package_revoked` but stays installed and served.
#[tokio::test]
async fn delisting_an_installed_package_fails_doctor_and_keeps_it_serving() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0")]);
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 0, "{document}");
    rig.server
        .set(INDEX_PATH, Route::Body(rig.keys.signed(Vec::new())));
    rig.refresh().await;

    let (code, status, detail) = rig.doctor_index().await;

    assert_eq!(status, "fail", "{detail}");
    assert!(detail.starts_with("package_revoked:"), "{detail}");
    assert_ne!(code, 0, "a failed check fails doctor");
    assert!(rig.serves(RUNTIME).await, "the delisted package launches");
    let packages = rig.install.packages().await;
    assert_eq!(packages.len(), 1, "{packages:?}");
    assert_eq!(packages[0]["enabled"], true);
    assert_eq!(packages[0]["selected"], true);
}

/// Contract scenario 6: an index that reserves the runtime id of a package
/// installed earlier warns in doctor and leaves it serving, while a new local
/// package, archive or directory, cannot claim the reserved id.
#[tokio::test]
async fn a_reserved_runtime_id_warns_and_refuses_new_local_claims() {
    let rig = Rig::start().await;
    let local = archive_of("community.idxres", RESERVED_RUNTIME, "1.0.0");
    let local_file = rig.install.write("local.tar.zst", &local.bytes);
    let path = |file: &PathBuf| file.to_str().expect("utf-8 path").to_owned();
    let (code, document) = rig
        .install
        .json(&[
            "plugin",
            "install",
            &path(&local_file),
            "--sha256",
            local.digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{document}");
    assert!(rig.serves(RESERVED_RUNTIME).await);
    let official = Listing::of(
        RESERVED_PACKAGE_ID,
        RESERVED_RUNTIME,
        "1.0.0",
        rig.server.url(&archive_path("1.0.0")),
    );
    rig.server.set(
        INDEX_PATH,
        Route::Body(rig.keys.signed(vec![official.entry()])),
    );
    rig.refresh().await;

    let (code, status, detail) = rig.doctor_index().await;

    assert_eq!(status, "warn", "{detail}");
    assert!(
        detail.starts_with("runtime_id_reserved_by_official:"),
        "{detail}"
    );
    assert_eq!(code, 0, "a warning keeps doctor green");
    assert!(
        rig.serves(RESERVED_RUNTIME).await,
        "the package installed before the reservation keeps launching"
    );

    let claim = archive_of("community.idxres2", RESERVED_RUNTIME, "1.0.0");
    let claim_file = rig.install.write("claim.tar.zst", &claim.bytes);
    let (code, document) = rig
        .install
        .json(&[
            "plugin",
            "install",
            &path(&claim_file),
            "--sha256",
            claim.digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "package_runtime_not_claimable");

    let directory = rig.install.env.root().join("claim-dir");
    fs::create_dir_all(&directory).expect("create the package directory");
    fs::write(
        directory.join("runtime.toml"),
        runtime_document_of("community.idxres3", RESERVED_RUNTIME, "1.0.0"),
    )
    .expect("write the descriptor");
    fs::write(directory.join("detect.toml"), DETECT_MANIFEST).expect("write the detect manifest");
    let (code, document) = rig
        .install
        .json(&[
            "plugin",
            "link",
            directory.to_str().expect("utf-8 path"),
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "package_runtime_not_claimable");
    assert_eq!(rig.install.packages().await.len(), 1);
}

/// Contract scenario 7: a damaged stored document fails the doctor check,
/// leaves the daemon answering, and the next refresh repairs it. The daemon reads
/// the stored copy only for doctor and apply, so no start-up path exists to cover.
#[tokio::test]
async fn a_damaged_stored_document_fails_doctor_until_the_next_refresh() {
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0")]);
    rig.refresh().await;
    let stored = rig.plugins_dir().join("index").join("official-v1.json");
    fs::write(&stored, b"corrupt").expect("corrupt the stored document");
    fs::set_permissions(&stored, fs::Permissions::from_mode(0o600)).expect("mode");

    let (_code, status, detail) = rig.doctor_index().await;
    assert_eq!(status, "fail", "{detail}");
    assert!(detail.starts_with("package_index_invalid:"), "{detail}");
    assert!(rig.install.packages().await.is_empty());

    rig.refresh().await;
    let (_code, status, detail) = rig.doctor_index().await;
    assert_eq!((status.as_str(), detail.as_str()), ("ok", "current"));
}

/// Plain mode bits of `path`.
fn mode_of(path: &std::path::Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

/// The files `--repair-state` preserved a damaged state file as.
fn damaged_copies(rig: &Rig) -> Vec<PathBuf> {
    fs::read_dir(rig.plugins_dir())
        .expect("read the plugin root")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("index-state.json.damaged-"))
        })
        .collect()
}

/// A damaged `index-state.json` refuses a plain refresh with its own code;
/// `--repair-state` preserves the damaged bytes with the same mode and
/// replaces the file, after which plain refreshes work again.
#[tokio::test]
async fn repair_state_replaces_a_damaged_state_file() {
    const DAMAGED: &[u8] = b"{not json";
    let rig = Rig::start().await;
    rig.publish(&[rig.listing("0.1.0")]);
    let sequence = rig.refresh().await["ok"]["sequence"]
        .as_u64()
        .expect("sequence");
    let state = rig.plugins_dir().join("index-state.json");
    let mode = mode_of(&state);
    fs::write(&state, DAMAGED).expect("corrupt the state");
    fs::set_permissions(&state, fs::Permissions::from_mode(mode)).expect("mode");

    let (code, document) = rig.refresh_from(&rig.index_url(), &[]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "package_index_state_unreadable");
    assert!(damaged_copies(&rig).is_empty());

    let (code, document) = rig
        .refresh_from(&rig.index_url(), &["--repair-state"])
        .await;
    assert_eq!(code, 0, "{document}");
    let copies = damaged_copies(&rig);
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(fs::read(&copies[0]).expect("read the copy"), DAMAGED);
    assert_eq!(mode_of(&copies[0]), mode, "the copy keeps the file's mode");
    assert!(rig.index_high_water() >= sequence);

    let (code, document) = rig.refresh_from(&rig.index_url(), &[]).await;
    assert_eq!(code, 0, "{document}");
}

/// `--repair-state` never accepts a document below the stored verified copy:
/// the damaged file stays where it is.
#[tokio::test]
async fn repair_state_refuses_a_document_below_the_stored_copy() {
    const DAMAGED: &[u8] = b"{not json";
    let rig = Rig::start().await;
    let listing = rig.listing("0.1.0");
    rig.publish(std::slice::from_ref(&listing));
    let sequence = rig.refresh().await["ok"]["sequence"]
        .as_u64()
        .expect("sequence");
    let state = rig.plugins_dir().join("index-state.json");
    let mode = mode_of(&state);
    fs::write(&state, DAMAGED).expect("corrupt the state");
    fs::set_permissions(&state, fs::Permissions::from_mode(mode)).expect("mode");
    rig.server.set(
        INDEX_PATH,
        Route::Body(
            rig.keys
                .document(&rig.keys.key, sequence - 1_000, vec![listing.entry()]),
        ),
    );

    let (code, document) = rig
        .refresh_from(&rig.index_url(), &["--repair-state"])
        .await;

    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "package_index_rollback");
    assert!(damaged_copies(&rig).is_empty(), "nothing was moved aside");
    assert_eq!(fs::read(&state).expect("read the state"), DAMAGED);
}

/// A `--sha256` install and update need no index channel: a host without the
/// channel file updates a local archive to a newer version.
#[tokio::test]
async fn a_sha256_update_works_on_a_host_without_the_index_channel() {
    let keys = Keys::new();
    let install = Install::start_with_channel(&keys.anchor(), None).await;
    let first = archive_of(LOCAL_PACKAGE_ID, LOCAL_RUNTIME, "0.1.0");
    let second = archive_of(LOCAL_PACKAGE_ID, LOCAL_RUNTIME, "0.2.0");
    let first_file = install.write("0.1.0.tar.zst", &first.bytes);
    let second_file = install.write("0.2.0.tar.zst", &second.bytes);
    let path = |file: &PathBuf| file.to_str().expect("utf-8 path").to_owned();

    let (code, document) = install
        .json(&[
            "plugin",
            "install",
            &path(&first_file),
            "--sha256",
            first.digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{document}");
    let (code, document) = install
        .json(&[
            "plugin",
            "update",
            LOCAL_PACKAGE_ID,
            &path(&second_file),
            "--sha256",
            second.digest.as_str(),
            "--yes",
        ])
        .await;

    assert_eq!(code, 0, "{document}");
    let selected: Vec<_> = install
        .packages()
        .await
        .into_iter()
        .filter(|package| package["selected"] == true)
        .collect();
    assert_eq!(selected.len(), 1, "{selected:?}");
    assert_eq!(selected[0]["package"]["version"], "0.2.0");
}

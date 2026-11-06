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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer as _, SigningKey};
use package::{
    build_archive, index_key_record_signing_message, index_signing_message, read_archive,
    revocation_signing_message, ArchiveEntry, CatalogSignature, IndexBody, IndexChannel,
    IndexEntry, IndexEnvelope, IndexKeyRecord, Limits, PackageDigest, RevocationBody,
    SignedRevocations, INDEX_SCHEMA_VERSION,
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

fn runtime_document(version: &str) -> String {
    format!(
        r#"schema = 1
id = "{PACKAGE_ID}"
version = "{version}"
runtime_api = 1

[runtime]
id = "{RUNTIME}"
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
    let entries = [
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: runtime_document(version).into_bytes(),
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
    version: &'static str,
    url: String,
    platform: String,
    core: &'static str,
}

impl Listing {
    fn new(version: &'static str, url: String) -> Self {
        Self {
            archive: archive(version),
            version,
            url,
            platform: host_platform(),
            core: ANY_CORE,
        }
    }

    fn entry(&self) -> IndexEntry {
        IndexEntry {
            package_id: PackageId::parse(PACKAGE_ID).expect("package id"),
            runtime_id: RuntimeId::parse(RUNTIME).expect("runtime id"),
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
        let body = IndexBody {
            schema_version: INDEX_SCHEMA_VERSION,
            scope: "official".to_owned(),
            sequence,
            expires_at: sequence + INDEX_LIFETIME,
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
            schema_version: INDEX_SCHEMA_VERSION,
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
        let entries = listings.iter().map(Listing::entry).collect();
        self.server
            .set(INDEX_PATH, Route::Body(self.keys.signed(entries)));
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

    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(err_code(&document), "plugin_index_url_refused");
    assert_eq!(rig.server.requests(), [INDEX_PATH]);

    // A redirect of the index to a foreign host is refused before the hop.
    rig.server
        .set(INDEX_PATH, Route::Redirect(FOREIGN_URL.to_owned()));
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
    let (code, document) = rig.install(PACKAGE_ID, &["--yes"]).await;
    assert_eq!(code, 0, "{document}");

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
    // The server listens on 127.0.0.1 only, so nothing answers on `[::1]` at
    // its port: the connect is refused, or fails outright on a host without IPv6.
    let unreachable = format!("http://[::1]:{}{INDEX_PATH}", rig.server.addr.port());
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

//! A signed schema 2 plugin index that lists the assistant context package,
//! and a loopback server that serves it.
//!
//! The signing keys are generated inside the test process and never leave it.
//! The endorser root is the key whose anchor and channel file the staged
//! release carries, so the installed daemon verifies the index with them.

// Rust guideline compliant 2026-11-11

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer as _, SigningKey};
use package::{
    build_archive, index_key_record_signing_message, index_signing_message, read_archive,
    revocation_signing_message, ArchiveEntry, CatalogSignature, IndexBody, IndexEntry,
    IndexEntryKind, IndexEnvelope, IndexKeyRecord, Limits, PackageDigest, RevocationBody,
    SignedRevocations, INDEX_SCHEMA_VERSION, INDEX_SCHEMA_VERSION_2,
};
use pohunek_test_support::catalog::{host_platform, key_id, ANY_CORE};
use protocol::{PackageId, PackageVersion};

/// Package id of the assistant context package.
pub(crate) const ASSISTANT_PACKAGE: &str = "pohunek.context.assistant";

/// Path of the index document on the loopback server.
pub(crate) const INDEX_PATH: &str = "/official-index-v2.json";

/// Version the fixture package carries.
pub(crate) const ASSISTANT_VERSION: &str = "0.42.0";

/// The prompt contract the assistant launch reads from the package: the
/// `Safety` and `Navigation` sections the launch parses.
const PROMPT_CONTRACT: &str = "---\ntype: PromptTemplate\nid: assistant/system\ntitle: Assistant system prompt contract\ndescription: Safety and navigation contract of the fixture assistant package.\nsource_kind: manual\nintents: [setup]\n---\n\n# Assistant\n\n## Safety\n\n- Never print, store, or infer secret values.\n\n## Navigation\n\n1. Read `index.md` first.\n";

const DAY: u64 = 24 * 60 * 60;

/// How long a fixture index lives; below the verifier's lifetime cap.
const INDEX_LIFETIME: u64 = 7 * DAY;

/// How far before now a fixture key record opens.
const KEY_BACK_DATE: u64 = 1_000;

/// How long a fixture key record stays valid; below the verifier's cap.
const KEY_WINDOW: u64 = 100 * DAY;

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

/// A context package archive: its bytes and digest.
#[derive(Clone)]
pub(crate) struct Archive {
    pub(crate) bytes: Vec<u8>,
    pub(crate) digest: PackageDigest,
}

/// An assistant package archive of `version` whose bundle holds `marker`, so
/// two archives of one version have different digests.
pub(crate) fn assistant_archive(version: &str, marker: &str) -> Archive {
    let descriptor = format!(
        "schema = 1\nid = \"{ASSISTANT_PACKAGE}\"\nversion = \"{version}\"\nruntime_api = 1\nrequires = [\"package-context-v1\"]\n\n[context]\ndir = \"bundle\"\n"
    );
    let entries = [
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: descriptor.into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "bundle/index.md".to_owned(),
            contents: format!("# Assistant\n\n{marker}\n").into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "bundle/assistant/system.md".to_owned(),
            contents: PROMPT_CONTRACT.as_bytes().to_vec(),
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

/// The endorser root R and the index key K it endorses.
pub(crate) struct Keys {
    pub(crate) root: SigningKey,
    pub(crate) key: SigningKey,
}

impl Keys {
    /// Keys with `root` as the endorser root.
    pub(crate) fn with_root(root: SigningKey) -> Self {
        Self {
            root,
            key: pohunek_test_support::catalog::test_key(),
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

    /// A schema 2 document signed by `signer`, valid from `issued_at`.
    pub(crate) fn document(
        &self,
        signer: &SigningKey,
        issued_at: u64,
        entries: Vec<IndexEntry>,
    ) -> Vec<u8> {
        let body = IndexBody {
            schema_version: INDEX_SCHEMA_VERSION_2,
            scope: "official".to_owned(),
            sequence: issued_at,
            expires_at: issued_at + INDEX_LIFETIME,
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
            schema_version: INDEX_SCHEMA_VERSION_2,
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

    /// A document signed by K, issued now.
    pub(crate) fn signed(&self, entries: Vec<IndexEntry>) -> Vec<u8> {
        self.document(&self.key, now(), entries)
    }

    /// A document signed by K, issued `age` seconds ago.
    pub(crate) fn signed_ago(&self, age: u64, entries: Vec<IndexEntry>) -> Vec<u8> {
        self.document(&self.key, now() - age, entries)
    }
}

/// The `context` entry that lists `archive` as the assistant package, served
/// at `url`.
pub(crate) fn assistant_entry(archive: &Archive, version: &str, url: String) -> IndexEntry {
    IndexEntry {
        package_id: PackageId::parse(ASSISTANT_PACKAGE).expect("package id"),
        kind: Some(IndexEntryKind::Context),
        runtime_id: None,
        version: PackageVersion::parse(version).expect("version"),
        digest: archive.digest.clone(),
        url,
        platforms: vec![host_platform()],
        core: ANY_CORE.to_owned(),
    }
}

/// Path of the archive of `version` on the loopback server.
pub(crate) fn archive_path(version: &str) -> String {
    format!("/{version}.tar.zst")
}

#[derive(Default)]
struct ServerState {
    routes: Mutex<HashMap<String, Vec<u8>>>,
    requests: Mutex<Vec<String>>,
}

/// A loopback HTTP server on a thread; it answers 404 for an unknown path and
/// records the path of every request.
pub(crate) struct Server {
    addr: SocketAddr,
    state: Arc<ServerState>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    pub(crate) fn start() -> Self {
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

    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub(crate) fn set(&self, path: &str, body: Vec<u8>) {
        self.state
            .routes
            .lock()
            .expect("routes")
            .insert(path.to_owned(), body);
    }

    pub(crate) fn remove(&self, path: &str) {
        self.state.routes.lock().expect("routes").remove(path);
    }

    pub(crate) fn requests(&self) -> Vec<String> {
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
    let body = state.routes.lock().expect("routes").get(&path).cloned();
    let mut stream = stream;
    let (status, bytes) = match body {
        Some(bytes) => ("200 OK", bytes),
        None => ("404 Not Found", Vec::new()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        bytes.len()
    );
    // A client that hangs up early is not an error of the server.
    let _outcome = stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(&bytes))
        .and_then(|()| stream.flush());
}

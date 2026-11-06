//! Scenarios for the plugin index owner ceremony (`catalog keygen`,
//! `index endorse`, `index revoke`, `index channel`) driven through the real
//! `xtask` binary in a hermetic environment.
//!
//! The expected results are derived outside the code under test: a key id is
//! the SHA-256 of the public key (`sha2` here), a record or revocation list is
//! accepted only when the package crate's `verify_index` accepts an index
//! document assembled around it, and the channel bytes are the package crate's
//! own serialization. The 366-day cap, the 14-day floor and the 64-id cap are
//! restated from the design as literals, so a changed constant fails here.
//!
//! - keygen writes an owner-private key file, prints only the key id and the
//!   public key, and refuses an existing path, a symbolic link and a missing
//!   parent;
//! - endorse writes a record the index verifier accepts and refuses windows
//!   outside 14..=366 days, an empty or elapsed window, another scope, a
//!   group-readable key file, an existing output and self-endorsement;
//! - revoke chains documents with a rising sequence that never drop an id, and
//!   refuses a forged or tampered previous document, more than 64 ids and the
//!   endorser's own id;
//! - channel writes the package crate's bytes for a root of the anchor and
//!   refuses any other id.

// Rust guideline compliant 2026-11-06

use std::fmt::Write as _;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use package::{
    index_signing_message, parse_index_channel, revocation_signing_message, verify_index,
    CatalogSignature, IndexBody, IndexChannel, IndexEnvelope, IndexKeyRecord, IndexVerifierState,
    KeyId, RootKey, SignedRevocations,
};
use pohunek_test_support::env::TestEnv;
use sha2::{Digest as _, Sha256};

const DAY: u64 = 24 * 60 * 60;

/// A hermetic checkout marker plus a scratch area for key and document files.
struct Ceremony {
    env: TestEnv,
}

/// A key created by `catalog keygen`.
struct OwnerKey {
    file: PathBuf,
    key_id: String,
    public_key: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("write to a String");
        text
    })
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o7777
}

impl Ceremony {
    fn new() -> Self {
        let env = TestEnv::new().expect("hermetic test environment");
        // Marks the working directory as a checkout, which the binary requires.
        let marker = env.cwd().join("crates/xtask/Cargo.toml");
        std::fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir");
        std::fs::write(marker, "").expect("marker");
        Self { env }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.env.root().join(name)
    }

    fn xtask(&self, args: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("xtask"))
            .current_dir(self.env.cwd())
            .args(args)
            .output()
            .expect("run xtask")
    }

    fn xtask_ok(&self, args: &[&str]) -> String {
        let output = self.xtask(args);
        assert!(
            output.status.success(),
            "xtask {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf-8 stdout")
    }

    /// Requires the command to fail with `reason` on stderr and to leave
    /// `output` absent when it did not exist before.
    fn assert_refused(&self, args: &[&str], output: &Path, reason: &str) {
        let existed = output.exists();
        let result = self.xtask(args);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success(), "xtask {args:?} must be refused");
        assert!(stderr.contains(reason), "expected {reason:?} in: {stderr}");
        assert_eq!(output.exists(), existed, "a refusal changed the output");
    }

    fn keygen(&self, name: &str) -> OwnerKey {
        let file = self.path(name);
        let stdout = self.xtask_ok(&[
            "catalog",
            "keygen",
            "--output",
            file.to_str().expect("utf-8"),
        ]);
        let mut lines = stdout.lines();
        let key_id = lines
            .next()
            .and_then(|line| line.strip_prefix("key_id="))
            .expect("key_id line")
            .to_owned();
        let public_key = lines
            .next()
            .and_then(|line| line.strip_prefix("public_key="))
            .expect("public_key line")
            .to_owned();
        assert_eq!(lines.next(), None, "keygen printed more than two lines");
        OwnerKey {
            file,
            key_id,
            public_key,
        }
    }

    fn endorse(
        &self,
        endorser: &OwnerKey,
        key: &OwnerKey,
        window: (u64, u64),
        output: &Path,
    ) -> Output {
        self.xtask(&[
            "index",
            "endorse",
            "--endorser-key-file",
            endorser.file.to_str().expect("utf-8"),
            "--key-file",
            key.file.to_str().expect("utf-8"),
            "--scope",
            "official",
            "--not-before",
            &window.0.to_string(),
            "--not-after",
            &window.1.to_string(),
            "--output",
            output.to_str().expect("utf-8"),
        ])
    }

    fn revoke(
        &self,
        endorser: &OwnerKey,
        sequence: u64,
        ids: &[&str],
        previous: Option<&Path>,
        output: &Path,
    ) -> Output {
        let mut args = vec![
            "index".to_owned(),
            "revoke".to_owned(),
            "--endorser-key-file".to_owned(),
            endorser.file.display().to_string(),
            "--sequence".to_owned(),
            sequence.to_string(),
        ];
        for id in ids {
            args.push("--key-id".to_owned());
            args.push((*id).to_owned());
        }
        if let Some(previous) = previous {
            args.push("--previous".to_owned());
            args.push(previous.display().to_string());
        }
        args.push("--output".to_owned());
        args.push(output.display().to_string());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.xtask(&args)
    }

    /// The endorsement `endorser` signs for `key` over `window`, written by
    /// the command, checked to succeed.
    fn endorsed(
        &self,
        endorser: &OwnerKey,
        key: &OwnerKey,
        window: (u64, u64),
        name: &str,
    ) -> PathBuf {
        let record = self.path(name);
        let output = self.endorse(endorser, key, window, &record);
        assert!(
            output.status.success(),
            "endorse failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        record
    }

    fn revoked(
        &self,
        endorser: &OwnerKey,
        sequence: u64,
        ids: &[&str],
        previous: Option<&Path>,
        name: &str,
    ) -> PathBuf {
        let document = self.path(name);
        let output = self.revoke(endorser, sequence, ids, previous, &document);
        assert!(
            output.status.success(),
            "revoke failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        document
    }
}

fn signing_key_of(file: &Path) -> SigningKey {
    let text = std::fs::read_to_string(file).expect("key file");
    let seed: Vec<u8> = text
        .trim_end()
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).expect("ascii"), 16).expect("hex"))
        .collect();
    SigningKey::from_bytes(&seed.try_into().expect("32-byte seed"))
}

fn read_record(path: &Path) -> IndexKeyRecord {
    serde_json::from_slice(&std::fs::read(path).expect("record")).expect("record JSON")
}

fn read_revocations(path: &Path) -> SignedRevocations {
    serde_json::from_slice(&std::fs::read(path).expect("revocations")).expect("revocations JSON")
}

/// Runs the package crate's index verifier over a document that carries
/// `record` and `revocations` and is signed by `index_key`; the endorser is
/// the root `endorser`.
fn verify_with_package(
    endorser: &OwnerKey,
    index_key: &OwnerKey,
    record_file: &Path,
    revocations_file: &Path,
    at: u64,
) -> Result<(), package::IndexError> {
    let record = read_record(record_file);
    let body = IndexBody {
        schema_version: 1,
        scope: "official".to_owned(),
        sequence: at,
        expires_at: at + DAY,
        entries: Vec::new(),
    };
    let signer = signing_key_of(&index_key.file);
    let signature = signer.sign(&index_signing_message(&body).expect("index message"));
    let envelope = IndexEnvelope {
        schema_version: 1,
        index: body,
        key_records: vec![record],
        signatures: vec![CatalogSignature {
            key_id: KeyId::parse(&index_key.key_id).expect("key id"),
            signature: hex(&signature.to_bytes()),
        }],
        revocations: read_revocations(revocations_file),
    };
    let public = hex_to_32(&endorser.public_key);
    let root = RootKey::new(public, 0, u64::MAX).expect("root key");
    verify_index(
        &serde_json::to_vec(&envelope).expect("envelope"),
        &root,
        &std::collections::BTreeSet::new(),
        &IndexVerifierState::default(),
        at,
    )
    .map(|_verified| ())
}

fn hex_to_32(text: &str) -> [u8; 32] {
    let bytes: Vec<u8> = text
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).expect("ascii"), 16).expect("hex"))
        .collect();
    bytes.try_into().expect("32 bytes")
}

#[test]
fn keygen_writes_an_owner_private_key_and_prints_only_its_public_half() {
    let ceremony = Ceremony::new();
    let file = ceremony.path("official-index.key");
    let output = ceremony.xtask(&[
        "catalog",
        "keygen",
        "--output",
        file.to_str().expect("utf-8"),
    ]);
    assert!(output.status.success());

    assert_eq!(mode_of(&file), 0o600);
    let content = std::fs::read_to_string(&file).expect("key file");
    assert_eq!(content.len(), 65, "64 hex characters and a newline");
    assert!(content.ends_with('\n'));
    let seed = content.trim_end();
    assert!(seed.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));

    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(!stdout.contains(seed), "stdout prints the seed");
    assert!(!stderr.contains(seed), "stderr prints the seed");
    let public = SigningKey::from_bytes(&hex_to_32(seed)).verifying_key();
    let expected_id = hex(&Sha256::digest(public.as_bytes()));
    assert_eq!(
        stdout,
        format!(
            "key_id={expected_id}\npublic_key={}\n",
            hex(public.as_bytes())
        )
    );

    let second = ceremony.keygen("second.key");
    assert_ne!(second.key_id, expected_id, "two keys share a seed");
}

#[test]
fn keygen_refuses_an_existing_path_a_symbolic_link_and_a_missing_parent() {
    let ceremony = Ceremony::new();
    let existing = ceremony.path("existing.key");
    std::fs::write(&existing, "keep me\n").expect("write");
    ceremony.assert_refused(
        &[
            "catalog",
            "keygen",
            "--output",
            existing.to_str().expect("utf-8"),
        ],
        &existing,
        "failed to access",
    );
    assert_eq!(std::fs::read(&existing).expect("read"), b"keep me\n");

    let target = ceremony.path("target.key");
    std::fs::write(&target, "target\n").expect("write");
    let link = ceremony.path("link.key");
    symlink(&target, &link).expect("symlink");
    ceremony.assert_refused(
        &[
            "catalog",
            "keygen",
            "--output",
            link.to_str().expect("utf-8"),
        ],
        &link,
        "failed to access",
    );
    assert_eq!(std::fs::read(&target).expect("read"), b"target\n");

    let dangling = ceremony.path("dangling.key");
    let absent = ceremony.path("absent.key");
    symlink(&absent, &dangling).expect("symlink");
    ceremony.assert_refused(
        &[
            "catalog",
            "keygen",
            "--output",
            dangling.to_str().expect("utf-8"),
        ],
        &absent,
        "failed to access",
    );

    let orphan = ceremony.path("no-such-dir/orphan.key");
    ceremony.assert_refused(
        &[
            "catalog",
            "keygen",
            "--output",
            orphan.to_str().expect("utf-8"),
        ],
        &orphan,
        "failed to access",
    );
}

#[test]
fn an_endorsed_record_and_a_first_revocation_list_pass_the_index_verifier() {
    let ceremony = Ceremony::new();
    let primary = ceremony.keygen("primary.key");
    let index_key = ceremony.keygen("index.key");
    let start = now() - 60;
    let record_file = ceremony.path("official.record.json");

    let output = ceremony.endorse(
        &primary,
        &index_key,
        (start, start + 365 * DAY),
        &record_file,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf-8"),
        format!(
            "key_id={}\nendorsed_by={}\n",
            index_key.key_id, primary.key_id
        )
    );
    assert_eq!(mode_of(&record_file), 0o644);

    // The mode holds under the strict umask of the ceremony shell.
    let strict = ceremony.path("strict.record.json");
    let status = ceremony
        .env
        .command("/bin/sh")
        .current_dir(ceremony.env.cwd())
        .arg("-c")
        .arg("umask 077; exec \"$0\" index endorse --endorser-key-file \"$1\" --key-file \"$2\" --scope official --not-before \"$3\" --not-after \"$4\" --output \"$5\"")
        .arg(pohunek_test_support::bin_exe("xtask"))
        .arg(&primary.file)
        .arg(&index_key.file)
        .arg(start.to_string())
        .arg((start + 365 * DAY).to_string())
        .arg(&strict)
        .status()
        .expect("run xtask under umask 077");
    assert!(status.success());
    assert_eq!(mode_of(&strict), 0o644);

    let record = read_record(&record_file);
    assert_eq!(record.key_id.as_str(), index_key.key_id);
    assert_eq!(record.public_key, index_key.public_key);
    assert_eq!(record.scope, "official");
    assert_eq!(record.not_before, start);
    assert_eq!(record.not_after, start + 365 * DAY);
    assert_eq!(record.endorsed_by.as_str(), primary.key_id);

    let revocations = ceremony.revoked(&primary, 1, &[], None, "revocations.json");
    assert_eq!(mode_of(&revocations), 0o644);
    verify_with_package(&primary, &index_key, &record_file, &revocations, now())
        .expect("the daemon's verifier accepts the ceremony output");

    // A different endorser's key does not verify the same record.
    let stranger = ceremony.keygen("stranger.key");
    assert!(verify_with_package(&stranger, &index_key, &record_file, &revocations, now()).is_err());
}

#[test]
fn endorse_refuses_windows_outside_the_floor_and_the_cap() {
    let ceremony = Ceremony::new();
    let primary = ceremony.keygen("primary.key");
    let index_key = ceremony.keygen("index.key");
    let start = now() - 60;

    let accepted = [14 * DAY, 366 * DAY];
    for (n, window) in accepted.into_iter().enumerate() {
        let record = ceremony.path(&format!("accepted-{n}.json"));
        let output = ceremony.endorse(&primary, &index_key, (start, start + window), &record);
        assert!(
            output.status.success(),
            "window {window}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let refused: [((u64, u64), &str); 6] = [
        ((start, start + 367 * DAY), "longer than"),
        (
            (start, start + 14 * DAY - 1),
            "shorter than one index lifetime",
        ),
        ((start, start), "is empty"),
        ((start + DAY, start), "is empty"),
        ((start - 400 * DAY, start - 386 * DAY), "already ended"),
        ((start - 400 * DAY, start - 33 * DAY), "longer than"),
    ];
    for (n, (window, reason)) in refused.into_iter().enumerate() {
        let record = ceremony.path(&format!("refused-{n}.json"));
        let output = ceremony.endorse(&primary, &index_key, window, &record);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "window {window:?} was accepted");
        assert!(stderr.contains(reason), "{window:?}: {stderr}");
        assert!(!record.exists(), "window {window:?} wrote a record");
    }

    let record = ceremony.path("scope.json");
    let output = ceremony.xtask(&[
        "index",
        "endorse",
        "--endorser-key-file",
        primary.file.to_str().expect("utf-8"),
        "--key-file",
        index_key.file.to_str().expect("utf-8"),
        "--scope",
        "verified",
        "--not-before",
        &start.to_string(),
        "--not-after",
        &(start + 365 * DAY).to_string(),
        "--output",
        record.to_str().expect("utf-8"),
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--scope must be `official`"));
    assert!(!record.exists());
}

#[test]
fn endorse_refuses_a_group_readable_key_file_and_writes_nothing() {
    let ceremony = Ceremony::new();
    let primary = ceremony.keygen("primary.key");
    let index_key = ceremony.keygen("index.key");
    let start = now() - 60;
    let window = (start, start + 365 * DAY);

    for unsafe_key in [&primary, &index_key] {
        let seed = std::fs::read_to_string(&unsafe_key.file).expect("seed");
        std::fs::set_permissions(&unsafe_key.file, std::fs::Permissions::from_mode(0o640))
            .expect("chmod");
        let record = ceremony.path("unsafe.json");
        let output = ceremony.endorse(&primary, &index_key, window, &record);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            stderr.contains("grants group or other access; run `chmod 600` on it"),
            "{stderr}"
        );
        assert!(
            stderr.contains(unsafe_key.file.to_str().expect("utf-8")),
            "{stderr}"
        );
        assert!(
            !stderr.contains(seed.trim_end()),
            "the message prints the seed"
        );
        assert!(!record.exists(), "an unsafe key file wrote a record");
        std::fs::set_permissions(&unsafe_key.file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod");
    }
}

#[test]
fn endorse_refuses_an_existing_output_and_a_self_endorsement() {
    let ceremony = Ceremony::new();
    let primary = ceremony.keygen("primary.key");
    let index_key = ceremony.keygen("index.key");
    let start = now() - 60;
    let window = (start, start + 365 * DAY);

    let existing = ceremony.path("existing.json");
    std::fs::write(&existing, "do not touch\n").expect("write");
    let output = ceremony.endorse(&primary, &index_key, window, &existing);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&existing).expect("read"), b"do not touch\n");

    let target = ceremony.path("target.json");
    std::fs::write(&target, "target\n").expect("write");
    let link = ceremony.path("link.json");
    symlink(&target, &link).expect("symlink");
    let output = ceremony.endorse(&primary, &index_key, window, &link);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&target).expect("read"), b"target\n");

    let own = ceremony.path("own.json");
    let output = ceremony.endorse(&primary, &primary, window, &own);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot endorse itself"));
    assert!(!own.exists());
}

#[test]
fn a_revocation_chain_rises_in_sequence_and_never_drops_an_id() {
    let ceremony = Ceremony::new();
    let primary = ceremony.keygen("primary.key");
    let index_key = ceremony.keygen("index.key");
    let other = ceremony.keygen("other.key");
    let start = now() - 60;
    let record = ceremony.endorsed(
        &primary,
        &index_key,
        (start, start + 365 * DAY),
        "record.json",
    );

    let first = ceremony.revoked(&primary, 1, &[&other.key_id], None, "first.json");
    let document = read_revocations(&first);
    assert_eq!(document.revocations.sequence, 1);
    assert_eq!(document.revocations.revoked_key_ids.len(), 1);
    assert_eq!(document.signature.key_id.as_str(), primary.key_id);
    let signature = hex_to_64(&document.signature.signature);
    VerifyingKey::from_bytes(&hex_to_32(&primary.public_key))
        .expect("key")
        .verify_strict(
            &revocation_signing_message(&document.revocations).expect("message"),
            &ed25519_dalek::Signature::from_bytes(&signature),
        )
        .expect("the first list verifies with the endorser");
    verify_with_package(&primary, &index_key, &record, &first, now()).expect("verifier accepts");

    // Sequence 2 keeping the previous id and adding a second one; ids are
    // sorted and deduplicated.
    let mut both = [other.key_id.as_str(), index_key.key_id.as_str()];
    both.sort_unstable();
    let second = ceremony.revoked(
        &primary,
        2,
        &[both[1], both[0], both[1]],
        Some(&first),
        "second.json",
    );
    let ids: Vec<String> = read_revocations(&second)
        .revocations
        .revoked_key_ids
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect();
    assert_eq!(ids, both, "the list is sorted and deduplicated");

    // Dropping the previous id, a sequence that does not rise and a missing
    // --previous are refused and write nothing.
    let cases: [(&str, u64, Vec<&str>, Option<&Path>); 6] = [
        (
            "shrink.json",
            2,
            vec![index_key.key_id.as_str()],
            Some(&first),
        ),
        ("same.json", 1, vec![other.key_id.as_str()], Some(&first)),
        ("lower.json", 1, both.to_vec(), Some(&second)),
        ("equal.json", 2, both.to_vec(), Some(&second)),
        ("no-previous.json", 2, vec![other.key_id.as_str()], None),
        ("zero.json", 0, vec![other.key_id.as_str()], None),
    ];
    for (name, sequence, ids, previous) in cases {
        let output_file = ceremony.path(name);
        let output = ceremony.revoke(&primary, sequence, &ids, previous, &output_file);
        assert!(
            !output.status.success(),
            "{name}: sequence {sequence} {ids:?} was accepted"
        );
        assert!(!output_file.exists(), "{name} wrote a document");
    }
    let shrink = ceremony.path("shrink-message.json");
    let output = ceremony.revoke(&primary, 2, &[&index_key.key_id], Some(&first), &shrink);
    assert!(String::from_utf8_lossy(&output.stderr).contains("never shrinks"));
}

fn hex_to_64(text: &str) -> [u8; 64] {
    let bytes: Vec<u8> = text
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).expect("ascii"), 16).expect("hex"))
        .collect();
    bytes.try_into().expect("64 bytes")
}

#[test]
fn revoke_refuses_a_forged_previous_document_the_id_cap_and_the_endorser_itself() {
    let ceremony = Ceremony::new();
    let primary = ceremony.keygen("primary.key");
    let impostor = ceremony.keygen("impostor.key");
    let victim = ceremony.keygen("victim.key");

    // A previous list signed by another key.
    let forged = ceremony.revoked(&impostor, 1, &[&victim.key_id], None, "forged.json");
    let next = ceremony.path("next.json");
    let output = ceremony.revoke(&primary, 2, &[&victim.key_id], Some(&forged), &next);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not signed by the endorser"));
    assert!(!next.exists());

    // A genuine list whose body was edited after signing.
    let genuine = ceremony.revoked(&primary, 1, &[&victim.key_id], None, "genuine.json");
    let tampered = ceremony.path("tampered.json");
    let text = std::fs::read_to_string(&genuine).expect("read");
    std::fs::write(
        &tampered,
        text.replace("\"sequence\": 1", "\"sequence\": 7"),
    )
    .expect("write");
    let output = ceremony.revoke(&primary, 8, &[&victim.key_id], Some(&tampered), &next);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not verify"));
    assert!(!next.exists());

    // Not a revocation document at all.
    let junk = ceremony.path("junk.json");
    std::fs::write(&junk, "{\"hello\": 1}\n").expect("write");
    let output = ceremony.revoke(&primary, 2, &[&victim.key_id], Some(&junk), &next);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("is not a revocation document"));
    assert!(!next.exists());

    // The endorser never revokes itself.
    let output = ceremony.revoke(&primary, 1, &[&primary.key_id], None, &next);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot revoke itself"));
    assert!(!next.exists());

    // 64 distinct ids are accepted, 65 are not. Ids need not belong to a key
    // the ceremony created.
    let ids: Vec<String> = (0_u8..65).map(|n| hex(&Sha256::digest([n]))).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let full = ceremony.revoked(&primary, 1, &refs[..64], None, "full.json");
    assert_eq!(
        read_revocations(&full).revocations.revoked_key_ids.len(),
        64
    );
    let over = ceremony.path("over.json");
    let output = ceremony.revoke(&primary, 1, &refs, None, &over);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("more than 64"));
    assert!(!over.exists());

    // An existing output is never overwritten.
    let existing = ceremony.path("existing.json");
    std::fs::write(&existing, "do not touch\n").expect("write");
    let output = ceremony.revoke(&primary, 1, &[], None, &existing);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&existing).expect("read"), b"do not touch\n");

    // A malformed id is a usage error.
    let output = ceremony.revoke(&primary, 1, &["NOT-HEX"], None, &next);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--key-id is not 64 lowercase hex"));
    assert!(!next.exists());
}

#[test]
fn the_channel_names_a_root_of_the_anchor_and_matches_the_package_bytes() {
    let ceremony = Ceremony::new();
    let primary = ceremony.keygen("primary.key");
    let other = ceremony.keygen("other.key");
    let public = ceremony.path("primary.pub");
    std::fs::write(&public, format!("{}\n", primary.public_key)).expect("write");
    let anchor = ceremony.path("anchor.json");
    ceremony.xtask_ok(&[
        "catalog",
        "anchor",
        "--root",
        &format!("{}:0:4000000000", public.display()),
        "--output",
        anchor.to_str().expect("utf-8"),
    ]);

    let channel = ceremony.path("plugin-index-channel.json");
    ceremony.xtask_ok(&[
        "index",
        "channel",
        "--anchor",
        anchor.to_str().expect("utf-8"),
        "--endorser-key-id",
        &primary.key_id,
        "--output",
        channel.to_str().expect("utf-8"),
    ]);
    let bytes = std::fs::read(&channel).expect("channel");
    let expected = IndexChannel::new(KeyId::parse(&primary.key_id).expect("key id"))
        .to_bytes()
        .expect("channel bytes");
    assert_eq!(bytes, expected);
    assert_eq!(
        parse_index_channel(&bytes)
            .expect("channel parses")
            .index_endorser_key_id()
            .as_str(),
        primary.key_id
    );
    assert_eq!(mode_of(&channel), 0o644);

    // An id that is not a root, a malformed id and an existing output.
    let refused = ceremony.path("refused.json");
    for id in [other.key_id.as_str(), "not-a-key-id"] {
        ceremony.assert_refused(
            &[
                "index",
                "channel",
                "--anchor",
                anchor.to_str().expect("utf-8"),
                "--endorser-key-id",
                id,
                "--output",
                refused.to_str().expect("utf-8"),
            ],
            &refused,
            if id == "not-a-key-id" {
                "--endorser-key-id is not 64 lowercase hex"
            } else {
                "not a root of the trust anchor"
            },
        );
    }
    ceremony.assert_refused(
        &[
            "index",
            "channel",
            "--anchor",
            anchor.to_str().expect("utf-8"),
            "--endorser-key-id",
            &primary.key_id,
            "--output",
            channel.to_str().expect("utf-8"),
        ],
        &channel,
        "failed to access",
    );
    assert_eq!(std::fs::read(&channel).expect("channel"), expected);
}

#[test]
fn the_channel_refuses_a_root_that_the_same_anchor_revokes() {
    let ceremony = Ceremony::new();
    let primary = ceremony.keygen("primary.key");
    let public = ceremony.path("primary.pub");
    std::fs::write(&public, format!("{}\n", primary.public_key)).expect("write");
    let anchor = ceremony.path("revoking-anchor.json");
    ceremony.xtask_ok(&[
        "catalog",
        "anchor",
        "--root",
        &format!("{}:0:4000000000", public.display()),
        "--revoked-key-id",
        &primary.key_id,
        "--output",
        anchor.to_str().expect("utf-8"),
    ]);
    let channel = ceremony.path("revoked-channel.json");
    ceremony.assert_refused(
        &[
            "index",
            "channel",
            "--anchor",
            anchor.to_str().expect("utf-8"),
            "--endorser-key-id",
            &primary.key_id,
            "--output",
            channel.to_str().expect("utf-8"),
        ],
        &channel,
        "revoked by the trust anchor",
    );
}

//! Scenarios for the plugin index tooling (`index build`, `message`, `attach`,
//! `verify`, `throwaway`) driven through the real `xtask` binary in a hermetic
//! environment.
//!
//! The expected results are derived outside the code under test: the signed
//! message is recomputed with the package crate's re-serializing
//! `index_signing_message`, the index key signs in the test, and acceptance is
//! decided by the package crate's `verify_index`. Lifetimes, the domain
//! prefix and the signature file shape are restated as literals.
//!
//! - the fixtures tree builds, is signed by a key the xtask process never
//!   sees, and the attached document verifies; `[source]` stays out of every
//!   document;
//! - a missing key record, a signature over other bytes, a malformed signature
//!   file, an already signed document and a revoked signer leave no output;
//! - every `[source]` violation, tree shape fault and body fault is refused;
//! - key material verifies under the channel's endorser, a successor list must
//!   rise and keep its ids, and a foreign endorser is `UnknownEndorser`;
//! - the throwaway trust verifies against its own anchor and channel and
//!   against no release anchor.

// Rust guideline compliant 2026-11-06

use std::fmt::Write as _;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer as _, SigningKey};
use package::{index_signing_message, IndexEnvelope};
use pohunek_test_support::env::TestEnv;

const DAY: u64 = 24 * 60 * 60;

/// The domain prefix of an index signature, restated from the format.
const INDEX_DOMAIN: &[u8] = b"pohunek-plugin-index-v1\n";

/// Key id of the production primary root in `packaging/runtime-catalog-anchor.json`.
const PRODUCTION_PRIMARY: &str = "f52cf66e";

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

fn fixtures() -> PathBuf {
    pohunek_test_support::manifest_dir().join("src/plugin_index/fixtures")
}

fn production_anchor() -> PathBuf {
    pohunek_test_support::workspace_root().join("packaging/runtime-catalog-anchor.json")
}

fn production_channel() -> PathBuf {
    pohunek_test_support::workspace_root().join("packaging/plugin-index-channel.json")
}

/// A key created by `catalog keygen`.
struct OwnerKey {
    file: PathBuf,
    key_id: String,
    public_key: String,
}

impl OwnerKey {
    fn signing_key(&self) -> SigningKey {
        let text = std::fs::read_to_string(&self.file).expect("key file");
        let seed: Vec<u8> = text
            .trim_end()
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                u8::from_str_radix(std::str::from_utf8(pair).expect("ascii"), 16).expect("hex")
            })
            .collect();
        SigningKey::from_bytes(&seed.try_into().expect("32-byte seed"))
    }
}

/// A hermetic checkout marker plus a scratch area, with the trust of one
/// ceremony: root P, index key K, K's record, an empty revocation list, the
/// anchor and the channel.
struct Tooling {
    env: TestEnv,
    primary: OwnerKey,
    index_key: OwnerKey,
    record: PathBuf,
    revocations: PathBuf,
    anchor: PathBuf,
    channel: PathBuf,
}

fn s(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

impl Tooling {
    fn new() -> Self {
        let env = TestEnv::new().expect("hermetic test environment");
        let marker = env.cwd().join("crates/xtask/Cargo.toml");
        std::fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir");
        std::fs::write(marker, "").expect("marker");
        let mut tooling = Self {
            env,
            primary: OwnerKey {
                file: PathBuf::new(),
                key_id: String::new(),
                public_key: String::new(),
            },
            index_key: OwnerKey {
                file: PathBuf::new(),
                key_id: String::new(),
                public_key: String::new(),
            },
            record: PathBuf::new(),
            revocations: PathBuf::new(),
            anchor: PathBuf::new(),
            channel: PathBuf::new(),
        };
        tooling.primary = tooling.keygen("primary.key");
        tooling.index_key = tooling.keygen("index.key");
        tooling.record = tooling.path("index.record.json");
        let start = now() - DAY;
        tooling.xtask_ok(&[
            "index",
            "endorse",
            "--endorser-key-file",
            s(&tooling.primary.file),
            "--key-file",
            s(&tooling.index_key.file),
            "--scope",
            "official",
            "--not-before",
            &start.to_string(),
            "--not-after",
            &(start + 30 * DAY).to_string(),
            "--output",
            s(&tooling.record),
        ]);
        tooling.revocations = tooling.path("revocations.json");
        tooling.xtask_ok(&[
            "index",
            "revoke",
            "--endorser-key-file",
            s(&tooling.primary.file),
            "--sequence",
            "1",
            "--output",
            s(&tooling.revocations),
        ]);
        tooling.anchor = tooling.anchor_with(&tooling.primary, &[], "anchor.json");
        tooling.channel = tooling.channel_for(&tooling.anchor, &tooling.primary, "channel.json");
        tooling
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

    /// Requires the command to exit with `code`, print `reason` on stderr and
    /// leave `output` absent when it did not exist before.
    fn assert_refused(&self, args: &[&str], code: i32, output: &Path, reason: &str) {
        let existed = output.exists();
        let result = self.xtask(args);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert_eq!(result.status.code(), Some(code), "{args:?}: {stderr}");
        assert!(stderr.contains(reason), "expected {reason:?} in: {stderr}");
        assert_eq!(output.exists(), existed, "a refusal changed the output");
    }

    fn keygen(&self, name: &str) -> OwnerKey {
        let file = self.path(name);
        let stdout = self.xtask_ok(&["catalog", "keygen", "--output", s(&file)]);
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
        OwnerKey {
            file,
            key_id,
            public_key,
        }
    }

    fn anchor_with(&self, root: &OwnerKey, revoked: &[&str], name: &str) -> PathBuf {
        let public = self.path(&format!("{name}.pub"));
        std::fs::write(&public, format!("{}\n", root.public_key)).expect("write public key");
        let anchor = self.path(name);
        let root_spec = format!("{}:0:4000000000", public.display());
        let mut args = vec!["catalog", "anchor", "--root", &root_spec];
        for id in revoked {
            args.push("--revoked-key-id");
            args.push(id);
        }
        args.push("--output");
        args.push(s(&anchor));
        self.xtask_ok(&args);
        anchor
    }

    fn channel_for(&self, anchor: &Path, endorser: &OwnerKey, name: &str) -> PathBuf {
        let channel = self.path(name);
        self.xtask_ok(&[
            "index",
            "channel",
            "--anchor",
            s(anchor),
            "--endorser-key-id",
            &endorser.key_id,
            "--output",
            s(&channel),
        ]);
        channel
    }

    /// A copy of the fixtures tree that a scenario may damage.
    fn tree(&self, name: &str) -> PathBuf {
        let target = self.path(name);
        copy_dir(&fixtures(), &target);
        target
    }

    fn build_args(
        &self,
        tree: &Path,
        sequence: u64,
        expires_at: u64,
        output: &Path,
    ) -> Vec<String> {
        [
            "index",
            "build",
            "--entries",
            s(tree),
            "--revocations",
            s(&self.revocations),
            "--key-record",
            s(&self.record),
            "--sequence",
            &sequence.to_string(),
            "--expires-at",
            &expires_at.to_string(),
            "--output",
            s(output),
        ]
        .map(str::to_owned)
        .to_vec()
    }

    fn build_ok(&self, tree: &Path, name: &str) -> PathBuf {
        let output = self.path(name);
        let at = now();
        let args = self.build_args(tree, at, at + 7 * DAY, &output);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.xtask_ok(&args);
        output
    }

    /// Message, external signature by the index key, attach: the signed file.
    fn signed(&self, unsigned: &Path, name: &str) -> PathBuf {
        let message = self.path(&format!("{name}.message.bin"));
        self.xtask_ok(&[
            "index",
            "message",
            "--unsigned",
            s(unsigned),
            "--output",
            s(&message),
        ]);
        let signature = self.path(&format!("{name}.sig"));
        let bytes = std::fs::read(&message).expect("message");
        let signed = self.index_key.signing_key().sign(&bytes);
        std::fs::write(&signature, format!("{}\n", hex(&signed.to_bytes()))).expect("sig");
        let output = self.path(name);
        self.xtask_ok(&self.attach_args(unsigned, &self.index_key.key_id, &signature, &output));
        output
    }

    fn attach_args<'a>(
        &'a self,
        unsigned: &'a Path,
        key_id: &'a str,
        signature: &'a Path,
        output: &'a Path,
    ) -> Vec<&'a str> {
        vec![
            "index",
            "attach",
            "--unsigned",
            s(unsigned),
            "--key-id",
            key_id,
            "--signature",
            s(signature),
            "--anchor",
            s(&self.anchor),
            "--channel",
            s(&self.channel),
            "--output",
            s(output),
        ]
    }

    fn verify_args<'a>(&'a self, index: &'a Path) -> Vec<&'a str> {
        vec![
            "index",
            "verify",
            "--index",
            s(index),
            "--anchor",
            s(&self.anchor),
            "--channel",
            s(&self.channel),
        ]
    }
}

/// A named damage of an entries tree, the reason its refusal must give and
/// the function that applies it.
type Damage = (&'static str, &'static str, Box<dyn Fn(&Path)>);

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for item in std::fs::read_dir(from).expect("read_dir") {
        let item = item.expect("entry");
        let target = to.join(item.file_name());
        if item.file_type().expect("type").is_dir() {
            copy_dir(&item.path(), &target);
        } else {
            std::fs::copy(item.path(), target).expect("copy");
        }
    }
}

fn read_envelope(path: &Path) -> IndexEnvelope {
    serde_json::from_slice(&std::fs::read(path).expect("document")).expect("envelope")
}

fn muse_file(tree: &Path) -> PathBuf {
    tree.join("official/pohunek.runtime.muse/0.2.0.toml")
}

fn edit(path: &Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).expect("read");
    assert!(text.contains(from), "{from:?} is not in {}", path.display());
    std::fs::write(path, text.replacen(from, to, 1)).expect("write");
}

#[test]
fn a_fixtures_tree_is_built_signed_outside_and_verified() {
    let tooling = Tooling::new();
    let unsigned = tooling.build_ok(&fixtures(), "unsigned.json");

    let envelope = read_envelope(&unsigned);
    assert!(envelope.signatures.is_empty());
    assert_eq!(envelope.index.entries.len(), 2);
    assert_eq!(
        envelope.index.entries[0].package_id.as_str(),
        "pohunek.runtime.aurora",
        "entries are sorted by package id"
    );

    let message = tooling.path("message.bin");
    let stdout = tooling.xtask_ok(&[
        "index",
        "message",
        "--unsigned",
        s(&unsigned),
        "--output",
        s(&message),
    ]);
    let bytes = std::fs::read(&message).expect("message");
    assert!(bytes.starts_with(INDEX_DOMAIN));
    assert_eq!(
        bytes,
        index_signing_message(&envelope.index).expect("package message"),
        "the message is the package crate's message for the parsed body"
    );
    assert_eq!(
        stdout,
        format!(
            "index message ok: sha256={} bytes={}\n",
            sha256_hex(&bytes),
            bytes.len()
        )
    );

    let signed = tooling.signed(&unsigned, "signed.json");
    let expected = read_envelope(&signed);
    assert_eq!(expected.signatures.len(), 1);
    assert_eq!(
        expected.signatures[0].key_id.as_str(),
        tooling.index_key.key_id
    );
    let stdout = tooling.xtask_ok(&tooling.verify_args(&signed));
    assert_eq!(
        stdout,
        format!(
            "index verify ok: sequence={} expires_at={} signer={} entries=2\n",
            expected.index.sequence, expected.index.expires_at, tooling.index_key.key_id
        )
    );
    for file in [&unsigned, &signed] {
        let text = std::fs::read_to_string(file).expect("document");
        let commits = ["a".repeat(40), "b".repeat(40)];
        for forbidden in [
            "built_with",
            "repo_id",
            "source",
            "v0.40.0",
            &commits[0],
            &commits[1],
        ] {
            assert!(
                !text.contains(forbidden),
                "{forbidden} leaked into {}",
                file.display()
            );
        }
    }
}

#[test]
fn versions_of_one_package_are_ordered_by_version_not_by_file_name() {
    let tooling = Tooling::new();
    // `0.2.0-rc.toml` sorts before `0.2.0.toml` by file name, but the version
    // strings sort the other way in byte order (`0.2.0` is a prefix of `0.2.0-rc`).
    let tree = tooling.tree("tree");
    let muse = tree.join("official/pohunek.runtime.muse");
    let text = std::fs::read_to_string(muse.join("0.2.0.toml")).expect("read");
    let text = text
        .replace("version = \"0.2.0\"", "version = \"0.2.0-rc\"")
        .replace("tag = \"v0.2.0\"", "tag = \"v0.2.0-rc\"")
        .replace(&"1".repeat(64), &"3".repeat(64));
    std::fs::write(muse.join("0.2.0-rc.toml"), text).expect("write");
    let unsigned = tooling.build_ok(&tree, "unsigned.json");

    let versions: Vec<String> = read_envelope(&unsigned)
        .index
        .entries
        .iter()
        .map(|entry| entry.version.as_str().to_owned())
        .collect();
    assert_eq!(versions, ["0.1.0", "0.2.0", "0.2.0-rc"]);
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    hex(&Sha256::digest(bytes))
}

#[test]
fn attach_refuses_an_unknown_signer_a_foreign_signature_and_a_malformed_file() {
    let tooling = Tooling::new();
    let unsigned = tooling.build_ok(&fixtures(), "unsigned.json");
    let message = tooling.path("message.bin");
    tooling.xtask_ok(&[
        "index",
        "message",
        "--unsigned",
        s(&unsigned),
        "--output",
        s(&message),
    ]);
    let good = hex(&tooling
        .index_key
        .signing_key()
        .sign(&std::fs::read(&message).expect("message"))
        .to_bytes());
    let good_file = tooling.path("good.sig");
    std::fs::write(&good_file, format!("{good}\n")).expect("sig");

    // A key id that no record has.
    let stranger = tooling.keygen("stranger.key");
    let output = tooling.path("unknown.json");
    tooling.assert_refused(
        &tooling.attach_args(&unsigned, &stranger.key_id, &good_file, &output),
        2,
        &output,
        "no key record",
    );

    // A signature over different bytes.
    let other = hex(&tooling
        .index_key
        .signing_key()
        .sign(b"different bytes")
        .to_bytes());
    let other_file = tooling.path("other.sig");
    std::fs::write(&other_file, other).expect("sig");
    let output = tooling.path("foreign.json");
    tooling.assert_refused(
        &tooling.attach_args(&unsigned, &tooling.index_key.key_id, &other_file, &output),
        1,
        &output,
        "plugin index signature does not verify",
    );

    // Not exactly 128 lowercase hex characters and an optional newline.
    let upper = good.to_uppercase();
    let malformed = [
        format!("{upper}\n"),
        format!("{}\n", &good[..127]),
        format!("{good} \n"),
        format!("{good}\n\n"),
        format!("{good}\n{good}\n"),
        String::new(),
    ];
    for (index, content) in malformed.iter().enumerate() {
        let file = tooling.path(&format!("malformed-{index}.sig"));
        std::fs::write(&file, content).expect("sig");
        let output = tooling.path(&format!("malformed-{index}.json"));
        tooling.assert_refused(
            &tooling.attach_args(&unsigned, &tooling.index_key.key_id, &file, &output),
            2,
            &output,
            // Two signatures exceed the read bound before the shape is judged.
            if index == 4 {
                "byte limit"
            } else {
                "128 lowercase hex"
            },
        );
    }

    // The signature without its newline is accepted.
    let bare = tooling.path("bare.sig");
    std::fs::write(&bare, &good).expect("sig");
    let accepted = tooling.path("accepted.json");
    tooling.xtask_ok(&tooling.attach_args(&unsigned, &tooling.index_key.key_id, &bare, &accepted));
    assert!(accepted.exists());

    // An already signed document is not an unsigned one.
    let again = tooling.path("again.json");
    tooling.assert_refused(
        &tooling.attach_args(&accepted, &tooling.index_key.key_id, &good_file, &again),
        2,
        &again,
        "already carries signatures",
    );
}

#[test]
fn attach_refuses_a_signer_the_anchor_revokes() {
    let tooling = Tooling::new();
    let unsigned = tooling.build_ok(&fixtures(), "unsigned.json");
    let message = tooling.path("message.bin");
    tooling.xtask_ok(&[
        "index",
        "message",
        "--unsigned",
        s(&unsigned),
        "--output",
        s(&message),
    ]);
    let signature = tooling.path("good.sig");
    std::fs::write(
        &signature,
        hex(&tooling
            .index_key
            .signing_key()
            .sign(&std::fs::read(&message).expect("message"))
            .to_bytes()),
    )
    .expect("sig");

    let anchor = tooling.anchor_with(
        &tooling.primary,
        &[&tooling.index_key.key_id],
        "revoking.json",
    );
    let channel = tooling.channel_for(&anchor, &tooling.primary, "revoking-channel.json");
    let output = tooling.path("revoked.json");
    tooling.assert_refused(
        &[
            "index",
            "attach",
            "--unsigned",
            s(&unsigned),
            "--key-id",
            &tooling.index_key.key_id,
            "--signature",
            s(&signature),
            "--anchor",
            s(&anchor),
            "--channel",
            s(&channel),
            "--output",
            s(&output),
        ],
        1,
        &output,
        "revoked",
    );
}

#[test]
fn build_refuses_every_source_violation_and_leaves_no_output() {
    let tooling = Tooling::new();
    let at = now();
    let cases: [(&str, &str, &str); 14] = [
        (
            "repo = \"pohunekd/plugin-muse-code\"",
            "repo = \"-x/plugin-muse-code\"",
            "repo",
        ),
        (
            "built_with = \"v0.40.0\"",
            "built_with = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
            "built_with",
        ),
        (
            "built_with = \"v0.40.0\"",
            "built_with = \"v0.40\"",
            "built_with",
        ),
        (
            "built_with = \"v0.40.0\"",
            "built_with = \"0.40.0\"",
            "built_with",
        ),
        (
            "built_with = \"v0.40.0\"",
            "built_with = \"v0.40.0-rc.1\"",
            "built_with",
        ),
        ("tag = \"v0.2.0\"", "tag = \"v0.2.1\"", "tag"),
        ("repo_id = 1001", "repo_id = 0", "repo_id"),
        (
            "repo = \"pohunekd/plugin-muse-code\"",
            "repo = \"pohunekd/plugin muse\"",
            "repo",
        ),
        (
            "repo = \"pohunekd/plugin-muse-code\"",
            "repo = \"pohunekd/../x\"",
            "repo",
        ),
        (
            "repo = \"pohunekd/plugin-muse-code\"",
            "repo = \"plugin-muse-code\"",
            "repo",
        ),
        (
            "commit = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
            "commit = \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"",
            "commit",
        ),
        (
            "commit = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
            "commit = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
            "commit",
        ),
        (
            "built_with = \"v0.40.0\"",
            "built_with = \"v0.40.0\"\nextra = \"x\"",
            "not a valid entry file",
        ),
        (
            "core = \">=0.40.0\"",
            "core = \">=0.40.0\"\nsurprise = 1",
            "not a valid entry file",
        ),
    ];
    for (index, (from, to, reason)) in cases.iter().enumerate() {
        let tree = tooling.tree(&format!("tree-{index}"));
        edit(&muse_file(&tree), from, to);
        let output = tooling.path(&format!("unsigned-{index}.json"));
        let args = tooling.build_args(&tree, at, at + 7 * DAY, &output);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        tooling.assert_refused(&args, 2, &output, reason);
    }
}

#[test]
fn build_refuses_a_tree_that_is_not_exactly_the_entry_layout() {
    let tooling = Tooling::new();
    let at = now();
    let mut damaged: Vec<Damage> = Vec::new();
    damaged.push((
        "stray file",
        "holds only package directories",
        Box::new(|tree: &Path| std::fs::write(tree.join("official/README.md"), "x").expect("w")),
    ));
    damaged.push((
        "non-toml file",
        "a package directory holds only",
        Box::new(|tree: &Path| {
            std::fs::write(tree.join("official/pohunek.runtime.muse/0.2.0.json"), "{}").expect("w");
        }),
    ));
    damaged.push((
        "deeper file",
        "a package directory holds only",
        Box::new(|tree: &Path| {
            let deeper = tree.join("official/pohunek.runtime.muse/extra");
            std::fs::create_dir_all(&deeper).expect("mkdir");
            std::fs::write(deeper.join("0.3.0.toml"), "x").expect("w");
        }),
    ));
    damaged.push((
        "directory beside official",
        "only the `official` directory is allowed",
        Box::new(|tree: &Path| std::fs::create_dir_all(tree.join("unofficial")).expect("mkdir")),
    ));
    damaged.push((
        "symbolic link entry",
        "a symbolic link",
        Box::new(|tree: &Path| {
            // The link points at an identical copy outside the tree, so only
            // the link rule can refuse it.
            let entry = tree.join("official/pohunek.runtime.muse/0.2.0.toml");
            let copy = tree.with_file_name("outside-0.2.0.toml");
            std::fs::copy(&entry, &copy).expect("copy");
            std::fs::remove_file(&entry).expect("remove");
            symlink(&copy, &entry).expect("symlink");
        }),
    ));
    damaged.push((
        "directory name differs from package id",
        "but its directory is",
        Box::new(|tree: &Path| {
            std::fs::rename(
                tree.join("official/pohunek.runtime.muse"),
                tree.join("official/pohunek.runtime.other"),
            )
            .expect("rename");
        }),
    ));
    damaged.push((
        "file stem differs from version",
        "but its file stem is",
        Box::new(|tree: &Path| {
            std::fs::rename(
                tree.join("official/pohunek.runtime.muse/0.2.0.toml"),
                tree.join("official/pohunek.runtime.muse/0.2.1.toml"),
            )
            .expect("rename");
        }),
    ));
    for (index, (name, reason, damage)) in damaged.iter().enumerate() {
        let tree = tooling.tree(&format!("tree-{index}"));
        damage(&tree);
        let output = tooling.path(&format!("unsigned-{index}.json"));
        let args = tooling.build_args(&tree, at, at + 7 * DAY, &output);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = tooling.xtask(&args);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success(), "{name} must be refused");
        assert!(
            stderr.contains(reason),
            "{name}: expected {reason:?} in: {stderr}"
        );
        assert!(!output.exists(), "{name} left an output");
    }
}

#[test]
fn build_refuses_a_body_the_verifier_refuses_and_an_existing_output() {
    let tooling = Tooling::new();
    let at = now();
    let tree = tooling.tree("tree");

    // Lifetime over 14 days, a sequence over a day ahead, an expired document.
    for (name, sequence, expires_at, reason) in [
        ("long", at, at + 15 * DAY, "lifetime"),
        ("future", at + 2 * DAY, at + 3 * DAY, "future"),
        ("expired", at - 3 * DAY, at - DAY, "expired"),
    ] {
        let output = tooling.path(&format!("{name}.json"));
        let args = tooling.build_args(&tree, sequence, expires_at, &output);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = tooling.xtask(&args);
        let stderr = String::from_utf8_lossy(&result.stderr).to_lowercase();
        assert_eq!(result.status.code(), Some(1), "{name}: {stderr}");
        assert!(stderr.contains(reason), "{name}: {stderr}");
        assert!(!output.exists(), "{name} left an output");
    }

    // Two entries with one digest.
    let aurora = tree.join("official/pohunek.runtime.aurora/0.1.0.toml");
    edit(&aurora, &"2".repeat(64), &"1".repeat(64));
    let output = tooling.path("duplicate-digest.json");
    let args = tooling.build_args(&tree, at, at + 7 * DAY, &output);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = tooling.xtask(&args);
    assert_eq!(result.status.code(), Some(1));
    assert!(!output.exists());

    // An existing output is never overwritten.
    let existing = tooling.path("existing.json");
    std::fs::write(&existing, b"keep\n").expect("existing");
    let args = tooling.build_args(&fixtures(), at, at + 7 * DAY, &existing);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    assert!(!tooling.xtask(&args).status.success());
    assert_eq!(std::fs::read(&existing).expect("read"), b"keep\n");
}

#[test]
fn print_source_prints_the_validated_source_of_a_single_entry_tree_only() {
    let tooling = Tooling::new();
    let tree = tooling.tree("single");
    std::fs::remove_dir_all(tree.join("official/pohunek.runtime.aurora")).expect("remove");
    let stdout = tooling.xtask_ok(&["index", "build", "--entries", s(&tree), "--print-source"]);
    assert_eq!(
        stdout,
        "repo=pohunekd/plugin-muse-code\nrepo_id=1001\ntag=v0.2.0\n\
         commit=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbuilt_with=v0.40.0\n"
    );

    let result = tooling.xtask(&[
        "index",
        "build",
        "--entries",
        s(&fixtures()),
        "--print-source",
    ]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty(), "a refusal printed source lines");

    edit(&muse_file(&tree), "tag = \"v0.2.0\"", "tag = \"main\"");
    let result = tooling.xtask(&["index", "build", "--entries", s(&tree), "--print-source"]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
}

#[test]
fn message_refuses_a_document_the_verifier_cannot_parse_and_an_existing_output() {
    let tooling = Tooling::new();
    let unsigned = tooling.build_ok(&fixtures(), "unsigned.json");
    let text = std::fs::read_to_string(&unsigned).expect("unsigned");

    let duplicate = tooling.path("duplicate-keys.json");
    std::fs::write(&duplicate, text.replacen('{', "{\"schema_version\": 1,", 1)).expect("write");
    let other_schema = tooling.path("schema-2.json");
    std::fs::write(
        &other_schema,
        text.replacen("\"schema_version\": 1", "\"schema_version\": 2", 1),
    )
    .expect("write");
    for (name, input) in [("duplicate", duplicate), ("schema", other_schema)] {
        let output = tooling.path(&format!("{name}.bin"));
        let result = tooling.xtask(&[
            "index",
            "message",
            "--unsigned",
            s(&input),
            "--output",
            s(&output),
        ]);
        assert_eq!(result.status.code(), Some(1), "{name}");
        assert!(!output.exists(), "{name} left an output");
    }

    let existing = tooling.path("existing.bin");
    std::fs::write(&existing, b"keep").expect("existing");
    let result = tooling.xtask(&[
        "index",
        "message",
        "--unsigned",
        s(&unsigned),
        "--output",
        s(&existing),
    ]);
    assert!(!result.status.success());
    assert_eq!(std::fs::read(&existing).expect("read"), b"keep");
}

#[test]
fn verify_applies_the_daemons_marks_freshness_and_signature_checks() {
    let tooling = Tooling::new();
    let unsigned = tooling.build_ok(&fixtures(), "unsigned.json");
    let signed = tooling.signed(&unsigned, "signed.json");
    let sequence = read_envelope(&signed).index.sequence;

    let mut args = tooling.verify_args(&signed);
    let above = (sequence + 1).to_string();
    args.extend(["--index-high-water", &above]);
    let result = tooling.xtask(&args);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("sequence"));

    let mut args = tooling.verify_args(&signed);
    args.extend(["--revocation-high-water", "2"]);
    let result = tooling.xtask(&args);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr)
        .contains("revocation sequence 1 is older than high-water mark 2"));

    // An equal mark is accepted, as at install time.
    let mut args = tooling.verify_args(&signed);
    let equal = sequence.to_string();
    args.extend(["--index-high-water", &equal, "--revocation-high-water", "1"]);
    tooling.xtask_ok(&args);

    // One changed byte of the signed body.
    let tampered = tooling.path("tampered.json");
    let text = std::fs::read_to_string(&signed).expect("signed");
    std::fs::write(&tampered, text.replacen("0.1.0", "0.1.1", 1)).expect("write");
    let result = tooling.xtask(&tooling.verify_args(&tampered));
    assert_eq!(result.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("plugin index signature does not verify")
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario walks the whole command sequence"
)]
fn key_material_verifies_under_the_channels_endorser_and_a_successor_list_must_extend() {
    let tooling = Tooling::new();
    let stdout = tooling.xtask_ok(&[
        "index",
        "verify",
        "--record",
        s(&tooling.record),
        "--revocations",
        s(&tooling.revocations),
        "--anchor",
        s(&tooling.anchor),
        "--channel",
        s(&tooling.channel),
    ]);
    assert_eq!(
        stdout,
        format!(
            "material verify ok: key_id={} revocation_sequence=1\n",
            tooling.index_key.key_id
        )
    );

    // Successor lists.
    let ghost = "ab".repeat(32);
    let second = tooling.path("revocations-2.json");
    tooling.xtask_ok(&[
        "index",
        "revoke",
        "--endorser-key-file",
        s(&tooling.primary.file),
        "--sequence",
        "2",
        "--key-id",
        &ghost,
        "--previous",
        s(&tooling.revocations),
        "--output",
        s(&second),
    ]);
    let material = |revocations: &Path, previous: &Path| {
        tooling.xtask(&[
            "index",
            "verify",
            "--record",
            s(&tooling.record),
            "--revocations",
            s(revocations),
            "--previous-revocations",
            s(previous),
            "--anchor",
            s(&tooling.anchor),
            "--channel",
            s(&tooling.channel),
        ])
    };
    let stdout = String::from_utf8(material(&second, &tooling.revocations).stdout).expect("utf-8");
    assert_eq!(
        stdout,
        format!(
            "material verify ok: key_id={} revocation_sequence=2\n",
            tooling.index_key.key_id
        )
    );
    // The same list, and a list that dropped an id, do not extend the previous.
    let same = material(&second, &second);
    assert!(!same.status.success());
    assert!(String::from_utf8_lossy(&same.stderr).contains("greater"));
    // Sequence 2 without the id sequence 1 revoked: build both chains.
    let with_ghost = tooling.path("revocations-ghost.json");
    tooling.xtask_ok(&[
        "index",
        "revoke",
        "--endorser-key-file",
        s(&tooling.primary.file),
        "--sequence",
        "1",
        "--key-id",
        &ghost,
        "--output",
        s(&with_ghost),
    ]);
    let other_id = "cd".repeat(32);
    let without_ghost = tooling.path("revocations-other.json");
    tooling.xtask_ok(&[
        "index",
        "revoke",
        "--endorser-key-file",
        s(&tooling.primary.file),
        "--sequence",
        "2",
        "--key-id",
        &other_id,
        "--previous",
        s(&tooling.revocations),
        "--output",
        s(&without_ghost),
    ]);
    let dropped = material(&without_ghost, &with_ghost);
    assert!(!dropped.status.success());
    assert!(String::from_utf8_lossy(&dropped.stderr).contains("never shrinks"));

    // A list signed by another key is not the endorser's.
    let impostor = tooling.keygen("impostor.key");
    let forged = tooling.path("forged-revocations.json");
    tooling.xtask_ok(&[
        "index",
        "revoke",
        "--endorser-key-file",
        s(&impostor.file),
        "--sequence",
        "1",
        "--output",
        s(&forged),
    ]);
    let result = tooling.xtask(&[
        "index",
        "verify",
        "--record",
        s(&tooling.record),
        "--revocations",
        s(&forged),
        "--anchor",
        s(&tooling.anchor),
        "--channel",
        s(&tooling.channel),
    ]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr)
        .contains("revocation list signature does not verify"));

    // A record endorsed by another root.
    let foreign_record = tooling.path("foreign.record.json");
    tooling.xtask_ok(&[
        "index",
        "endorse",
        "--endorser-key-file",
        s(&impostor.file),
        "--key-file",
        s(&tooling.index_key.file),
        "--scope",
        "official",
        "--not-before",
        &(now() - DAY).to_string(),
        "--not-after",
        &(now() + 30 * DAY).to_string(),
        "--output",
        s(&foreign_record),
    ]);
    let result = tooling.xtask(&[
        "index",
        "verify",
        "--record",
        s(&foreign_record),
        "--revocations",
        s(&tooling.revocations),
        "--anchor",
        s(&tooling.anchor),
        "--channel",
        s(&tooling.channel),
    ]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr)
        .contains("key record endorser is not the designated root"));
}

/// Extracts the single key record of `index` into a file.
fn record_of(tooling: &Tooling, index: &Path, name: &str) -> PathBuf {
    let envelope = read_envelope(index);
    let record = tooling.path(name);
    std::fs::write(
        &record,
        serde_json::to_vec_pretty(&envelope.key_records[0]).expect("record"),
    )
    .expect("write");
    record
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario walks the whole command sequence"
)]
fn the_throwaway_trust_verifies_against_itself_and_against_no_release_anchor() {
    let tooling = Tooling::new();
    let output = tooling.path("throwaway");
    std::fs::create_dir(&output).expect("mkdir");
    let stdout = tooling.xtask_ok(&[
        "index",
        "throwaway",
        "--entries",
        s(&fixtures()),
        "--output",
        s(&output),
    ]);
    assert!(
        stdout.starts_with("index throwaway ok: entries=2 "),
        "{stdout}"
    );

    let anchor = output.join("runtime-catalog-anchor.json");
    let channel = output.join("plugin-index-channel.json");
    let index = output.join("official-index-v1.json");
    let mut names: Vec<String> = std::fs::read_dir(&output)
        .expect("read_dir")
        .map(|item| {
            item.expect("entry")
                .file_name()
                .into_string()
                .expect("utf-8")
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "official-index-v1.json",
            "plugin-index-channel.json",
            "runtime-catalog-anchor.json"
        ]
    );

    let verified = tooling.xtask_ok(&[
        "index",
        "verify",
        "--index",
        s(&index),
        "--anchor",
        s(&anchor),
        "--channel",
        s(&channel),
    ]);
    assert!(
        verified.starts_with("index verify ok: sequence="),
        "{verified}"
    );
    assert!(verified.ends_with("entries=2\n"), "{verified}");

    // The anchor trusts exactly one root, and no seed was written.
    let anchor_text = std::fs::read_to_string(&anchor).expect("anchor");
    assert_eq!(
        anchor_text.matches("public_key").count(),
        1,
        "{anchor_text}"
    );
    let envelope = read_envelope(&index);
    assert_eq!(envelope.signatures.len(), 1);

    // The committed channel names the production primary as the endorser,
    // and that root is not the endorser of this index.
    let production = production_anchor();
    let production_channel = production_channel();
    let channel_text = std::fs::read_to_string(&production_channel).expect("production channel");
    assert!(
        channel_text.contains(PRODUCTION_PRIMARY),
        "the committed channel names another endorser"
    );
    let result = tooling.xtask(&[
        "index",
        "verify",
        "--index",
        s(&index),
        "--anchor",
        s(&production),
        "--channel",
        s(&production_channel),
    ]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr)
        .contains("revocation list signature does not verify"));

    // The throwaway key record names a root that is not the production one.
    let record = record_of(&tooling, &index, "throwaway.record.json");
    let revocations = tooling.path("throwaway.revocations.json");
    std::fs::write(
        &revocations,
        serde_json::to_vec_pretty(&envelope.revocations).expect("revocations"),
    )
    .expect("write");
    let result = tooling.xtask(&[
        "index",
        "verify",
        "--record",
        s(&record),
        "--revocations",
        s(&revocations),
        "--anchor",
        s(&production),
        "--channel",
        s(&production_channel),
    ]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr)
        .contains("key record endorser is not the designated root"));
}

#[test]
fn the_throwaway_refuses_an_output_that_is_missing_filled_or_inside_the_checkout() {
    let tooling = Tooling::new();
    let missing = tooling.path("missing");
    let result = tooling.xtask(&[
        "index",
        "throwaway",
        "--entries",
        s(&fixtures()),
        "--output",
        s(&missing),
    ]);
    assert!(!result.status.success());
    assert!(!missing.exists());

    let filled = tooling.path("filled");
    std::fs::create_dir(&filled).expect("mkdir");
    std::fs::write(filled.join("keep"), b"keep").expect("write");
    let result = tooling.xtask(&[
        "index",
        "throwaway",
        "--entries",
        s(&fixtures()),
        "--output",
        s(&filled),
    ]);
    assert!(!result.status.success());
    assert_eq!(std::fs::read_dir(&filled).expect("read_dir").count(), 1);

    let inside = tooling.env.cwd().join("inside");
    std::fs::create_dir(&inside).expect("mkdir");
    let result = tooling.xtask(&[
        "index",
        "throwaway",
        "--entries",
        s(&fixtures()),
        "--output",
        s(&inside),
    ]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("inside the checkout"));
    assert_eq!(std::fs::read_dir(&inside).expect("read_dir").count(), 0);

    let linked = tooling.path("linked");
    symlink(&inside, &linked).expect("symlink");
    let result = tooling.xtask(&[
        "index",
        "throwaway",
        "--entries",
        s(&fixtures()),
        "--output",
        s(&linked),
    ]);
    assert!(!result.status.success());
    assert_eq!(std::fs::read_dir(&inside).expect("read_dir").count(), 0);
}

#[test]
fn key_material_naming_a_revoked_key_is_refused() {
    let tooling = Tooling::new();
    let revoked = tooling.path("revokes-index-key.json");
    tooling.xtask_ok(&[
        "index",
        "revoke",
        "--endorser-key-file",
        s(&tooling.primary.file),
        "--sequence",
        "1",
        "--key-id",
        &tooling.index_key.key_id,
        "--output",
        s(&revoked),
    ]);
    let result = tooling.xtask(&[
        "index",
        "verify",
        "--record",
        s(&tooling.record),
        "--revocations",
        s(&revoked),
        "--anchor",
        s(&tooling.anchor),
        "--channel",
        s(&tooling.channel),
    ]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("key is revoked"));

    let anchor = tooling.anchor_with(
        &tooling.primary,
        &[&tooling.index_key.key_id],
        "revoking.json",
    );
    let channel = tooling.channel_for(&anchor, &tooling.primary, "revoking-channel.json");
    let result = tooling.xtask(&[
        "index",
        "verify",
        "--record",
        s(&tooling.record),
        "--revocations",
        s(&tooling.revocations),
        "--anchor",
        s(&anchor),
        "--channel",
        s(&channel),
    ]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("key is revoked"));
}

#[test]
fn an_entries_tree_may_carry_the_gitkeep_of_the_index_repository() {
    let tooling = Tooling::new();
    let tree = tooling.tree("with-gitkeep");
    std::fs::write(tree.join("official/.gitkeep"), b"").expect("gitkeep");
    tooling.build_ok(&tree, "unsigned.json");

    std::fs::write(tree.join("official/.gitkeep"), b"data").expect("gitkeep");
    let at = now();
    let output = tooling.path("refused.json");
    let args = tooling.build_args(&tree, at, at + 7 * DAY, &output);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    assert!(!tooling.xtask(&args).status.success());
    assert!(!output.exists());
}

/// An entry file for `check-entry`; `digit` makes the digest unique.
fn admission_entry(runtime: &str, package: &str, version: &str, digit: char, core: &str) -> String {
    format!(
        "package_id = \"{package}\"\nruntime_id = \"{runtime}\"\nversion = \"{version}\"\n\
         digest = \"sha256:{}\"\n\
         url = \"https://github.com/pohunekd/plugin-x/releases/download/v{version}/{package}-{version}.tar.zst\"\n\
         platforms = [\"x86_64-unknown-linux-gnu\"]\ncore = \"{core}\"\n\n\
         [source]\nrepo = \"pohunekd/plugin-x\"\nrepo_id = 1\ntag = \"v{version}\"\n\
         commit = \"cccccccccccccccccccccccccccccccccccccccc\"\nbuilt_with = \"v0.40.0\"\n",
        digit.to_string().repeat(64)
    )
}

impl Tooling {
    /// Runs `index check-entry` for a tree that holds `text` as the only entry
    /// and returns the exit code with stdout and stderr.
    fn check_entry(
        &self,
        text: &str,
        stage: &str,
        core_version: &str,
    ) -> (Option<i32>, String, String) {
        let id = text
            .lines()
            .find_map(|line| line.strip_prefix("package_id = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("package_id line");
        let version = text
            .lines()
            .find_map(|line| line.strip_prefix("version = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("version line");
        let tree = self.path(&format!("added-{id}-{version}-{stage}"));
        let file = tree.join(format!("official/{id}/{version}.toml"));
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, text).expect("entry");
        let result = self.xtask(&[
            "index",
            "check-entry",
            "--entries",
            s(&tree),
            "--existing",
            s(&fixtures()),
            "--core-version",
            core_version,
            "--stage",
            stage,
        ]);
        (
            result.status.code(),
            String::from_utf8_lossy(&result.stdout).into_owned(),
            String::from_utf8_lossy(&result.stderr).into_owned(),
        )
    }
}

/// A context entry file; `digit` makes the digest unique.
fn context_entry_text(name: &str, digit: char) -> String {
    admission_entry(
        "x",
        &format!("pohunek.context.{name}"),
        "0.42.0",
        digit,
        ">=0.42.0",
    )
    .replace("runtime_id = \"x\"\n", "kind = \"context\"\n")
}

#[test]
fn a_tree_with_context_entries_builds_in_both_schemas_and_admits_more_than_one() {
    let tooling = Tooling::new();
    let tree = tooling.tree("context-tree");
    let file = tree.join("official/pohunek.context.assistant/0.42.0.toml");
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, context_entry_text("assistant", '4')).expect("context entry");

    let v1 = tooling.path("context-v1.json");
    let at = now();
    let mut args = tooling.build_args(&tree, at, at + 7 * DAY, &v1);
    args.extend(["--schema-version".to_owned(), "1".to_owned()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    tooling.xtask_ok(&args);
    let v1_envelope = read_envelope(&v1);
    assert!(
        v1_envelope
            .index
            .entries
            .iter()
            .all(|entry| entry.kind.is_none() && entry.runtime_id.is_some()),
        "a schema 1 document lists runtime entries only"
    );

    let v2 = tooling.path("context-v2.json");
    let mut args = tooling.build_args(&tree, at, at + 7 * DAY, &v2);
    args.extend(["--schema-version".to_owned(), "2".to_owned()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    tooling.xtask_ok(&args);
    assert_eq!(
        read_envelope(&v2).index.entries.len(),
        v1_envelope.index.entries.len() + 1,
        "a schema 2 document also lists the context entry"
    );

    let added = tooling.path("added-context");
    let added_file = added.join("official/pohunek.context.second/0.42.0.toml");
    std::fs::create_dir_all(added_file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&added_file, context_entry_text("second", '5')).expect("added entry");
    let result = tooling.xtask(&[
        "index",
        "check-entry",
        "--entries",
        s(&added),
        "--existing",
        s(&tree),
        "--core-version",
        "0.42.0",
        "--stage",
        "schema",
    ]);
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn check_entry_accepts_a_new_release_and_prints_only_validated_fields() {
    let tooling = Tooling::new();
    let text = admission_entry("muse", "pohunek.runtime.muse", "0.10.0", '3', ">=0.40.0");
    let (code, stdout, stderr) = tooling.check_entry(&text, "schema", "0.40.0");
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        stdout,
        format!(
            "package_id=pohunek.runtime.muse\nruntime_id=muse\nversion=0.10.0\n\
             digest=sha256:{}\n\
             url=https://github.com/pohunekd/plugin-x/releases/download/v0.10.0/pohunek.runtime.muse-0.10.0.tar.zst\n\
             platforms=x86_64-unknown-linux-gnu\n",
            "3".repeat(64)
        )
    );
    assert!(!stdout.contains("core"), "the core range reached stdout");
}

/// Requires each case to exit 1 with its predicate on stderr and nothing on stdout.
fn assert_schema_refusals(tooling: &Tooling, cases: Vec<(&str, String, &str)>) {
    for (name, text, predicate) in cases {
        let (code, stdout, stderr) = tooling.check_entry(&text, "schema", "0.40.0");
        assert_eq!(code, Some(1), "{name}: {stderr}");
        assert!(
            stderr.contains(predicate),
            "{name}: expected {predicate} in {stderr}"
        );
        assert!(stdout.is_empty(), "{name}: a refusal printed fields");
    }
}

#[test]
fn check_entry_names_the_predicate_each_schema_refusal_fails() {
    let tooling = Tooling::new();
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "binding",
            admission_entry("pix", "pohunek.runtime.pi", "0.1.0", '3', ">=0.40.0"),
            "predicate=package-id-binding",
        ),
        (
            "refused id codex",
            admission_entry("codex", "pohunek.runtime.codex", "0.1.0", '3', ">=0.40.0"),
            "predicate=refused-runtime-id",
        ),
        (
            "refused id pi",
            admission_entry("pi", "pohunek.runtime.pi", "0.1.0", '3', ">=0.40.0"),
            "predicate=refused-runtime-id",
        ),
        (
            "runtime owned by another package",
            admission_entry("muse", "pohunek.runtime.muse2", "0.1.0", '3', ">=0.40.0"),
            "predicate=package-id-binding",
        ),
        (
            "refused id shell",
            admission_entry("shell", "pohunek.runtime.shell", "0.1.0", '3', ">=0.40.0"),
            "predicate=refused-runtime-id",
        ),
        (
            "refused id claude",
            admission_entry("claude", "pohunek.runtime.claude", "0.1.0", '3', ">=0.40.0"),
            "predicate=refused-runtime-id",
        ),
        (
            "refused id hermes",
            admission_entry("hermes", "pohunek.runtime.hermes", "0.1.0", '3', ">=0.40.0"),
            "predicate=refused-runtime-id",
        ),
    ];
    assert_schema_refusals(&tooling, cases);
}

#[test]
fn check_entry_names_the_predicate_each_version_and_url_refusal_fails() {
    let tooling = Tooling::new();
    let muse = "pohunek.runtime.muse";
    let existing_digest = '1';
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "four-part version",
            admission_entry("muse", muse, "1.0.0.0", '3', ">=0.40.0"),
            "predicate=version-not-semver",
        ),
        (
            "leading zero version",
            admission_entry("muse", muse, "01.0.0", '3', ">=0.40.0"),
            "predicate=version-not-semver",
        ),
        (
            "shell metacharacter in the url",
            admission_entry("muse", muse, "0.3.0", '3', ">=0.40.0")
                .replace("/pohunek.runtime.muse-0.3.0.tar.zst", "/$(id).tar.zst"),
            "predicate=url-charset",
        ),
        (
            "same version",
            admission_entry("muse", muse, "0.2.0", '3', ">=0.40.0"),
            "predicate=duplicate-version",
        ),
        (
            "duplicate digest",
            admission_entry("muse", muse, "0.3.0", existing_digest, ">=0.40.0"),
            "predicate=duplicate-digest",
        ),
        (
            "lower version",
            admission_entry("muse", muse, "0.1.0", '3', ">=0.40.0"),
            "predicate=version-not-greater",
        ),
        (
            "not semver",
            admission_entry("muse", muse, "1.0", '3', ">=0.40.0"),
            "predicate=version-not-semver",
        ),
        (
            "pre-release",
            admission_entry("muse", muse, "1.0.0-rc.1", '3', ">=0.40.0"),
            "predicate=version-not-release",
        ),
        (
            "build metadata",
            admission_entry("muse", muse, "1.0.0+b", '3', ">=0.40.0"),
            "predicate=version-not-release",
        ),
        (
            "source violation",
            admission_entry("muse", muse, "0.3.0", '3', ">=0.40.0").replace(
                "built_with = \"v0.40.0\"",
                "built_with = \"cccccccccccccccccccccccccccccccccccccccc\"",
            ),
            "predicate=schema",
        ),
        (
            "unknown field",
            admission_entry("muse", muse, "0.3.0", '3', ">=0.40.0") + "extra = 1\n",
            "predicate=schema",
        ),
    ];
    assert_schema_refusals(&tooling, cases);
}

#[test]
fn check_entry_orders_versions_as_semver_and_refuses_a_foreign_runtime_owner() {
    let tooling = Tooling::new();
    // `0.10.0` is above `0.2.0` although it sorts below it as text.
    let text = admission_entry("muse", "pohunek.runtime.muse", "0.10.0", '3', ">=0.40.0");
    assert_eq!(tooling.check_entry(&text, "schema", "0.40.0").0, Some(0));

    let aurora_for_muse =
        admission_entry("muse", "pohunek.runtime.aurora", "0.2.0", '3', ">=0.40.0");
    let (code, _, stderr) = tooling.check_entry(&aurora_for_muse, "schema", "0.40.0");
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("predicate=package-id-binding"), "{stderr}");
}

#[test]
fn check_entry_judges_the_core_range_by_its_lower_bound() {
    let tooling = Tooling::new();
    let accepted = [
        ">=0.40.0",
        "^0.40",
        "^0.40.0",
        "=0.40.0",
        "~0.40.0",
        "0.40.0",
        ">=0.40.0, <0.50.0",
        ">0.39",
        "0.40.*",
        ">0.39.18446744073709551615",
    ];
    for core in accepted {
        let text = admission_entry("muse", "pohunek.runtime.muse", "0.3.0", '3', core);
        let (code, stdout, stderr) = tooling.check_entry(&text, "core-range", "0.40.0");
        assert_eq!(code, Some(0), "{core}: {stderr}");
        assert_eq!(stdout, "core range ok: lower_bound=0.40.0\n", "{core}");
    }
    let refused = [
        (">=0.39.0, <0.41.0", "core-lower-bound"),
        ("<1.0", "core-lower-bound"),
        ("<=0.40.0", "core-lower-bound"),
        ("0.*", "core-lower-bound"),
        ("^0.39", "core-excludes-core-tag"),
        (">=0.41.0", "core-excludes-core-tag"),
        (">=0.40.0-rc.1", "core-prerelease"),
        ("*", "core-range-unparsed"),
        ("garbage", "core-range-unparsed"),
    ];
    for (core, predicate) in refused {
        let text = admission_entry("muse", "pohunek.runtime.muse", "0.3.0", '3', core);
        let (code, stdout, stderr) = tooling.check_entry(&text, "core-range", "0.40.0");
        assert_eq!(code, Some(1), "{core}: {stderr}");
        assert!(
            stderr.contains(&format!("predicate={predicate}")),
            "{core}: {stderr}"
        );
        assert!(stdout.is_empty(), "{core}");
    }
    let long = format!(">=0.40.0{}", ", <9.0.0".repeat(20));
    for core in [long.as_str(), ""] {
        let text = admission_entry("muse", "pohunek.runtime.muse", "0.3.0", '3', core);
        let (code, _, stderr) = tooling.check_entry(&text, "core-range", "0.40.0");
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains("predicate=core-range-unparsed"), "{stderr}");
    }
}

#[test]
fn check_entry_refuses_a_malformed_invocation_or_baseline() {
    let tooling = Tooling::new();
    let text = admission_entry("muse", "pohunek.runtime.muse", "0.3.0", '3', ">=0.40.0");
    let (code, _, stderr) = tooling.check_entry(&text, "schema", "v0.40.0");
    assert_eq!(code, Some(2), "{stderr}");

    let tree = tooling.path("two-entries");
    copy_dir(&fixtures(), &tree);
    let result = tooling.xtask(&[
        "index",
        "check-entry",
        "--entries",
        s(&tree),
        "--existing",
        s(&fixtures()),
        "--core-version",
        "0.40.0",
        "--stage",
        "schema",
    ]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("exactly one entry"));

    let broken = tooling.tree("broken-baseline");
    edit(&muse_file(&broken), "tag = \"v0.2.0\"", "tag = \"main\"");
    let added = tooling.path("added-for-broken");
    let file = added.join("official/pohunek.runtime.muse/0.3.0.toml");
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, &text).expect("entry");
    let result = tooling.xtask(&[
        "index",
        "check-entry",
        "--entries",
        s(&added),
        "--existing",
        s(&broken),
        "--core-version",
        "0.40.0",
        "--stage",
        "schema",
    ]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("predicate=existing-tree"));
}

#[test]
fn check_entry_refuses_an_identity_the_admitted_tree_binds_elsewhere() {
    let tooling = Tooling::new();
    let existing = tooling.tree("foreign-owner");
    let other = existing.join("official/pohunek.legacy/0.1.0.toml");
    std::fs::create_dir_all(other.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &other,
        admission_entry("muse", "pohunek.legacy", "0.1.0", '4', ">=0.40.0"),
    )
    .expect("entry");
    std::fs::remove_dir_all(existing.join("official/pohunek.runtime.muse")).expect("remove");
    std::fs::remove_dir_all(existing.join("official/pohunek.runtime.aurora")).expect("remove");

    let text = admission_entry("muse", "pohunek.runtime.muse", "0.3.0", '3', ">=0.40.0");
    let added = tooling.path("added-foreign-owner");
    let file = added.join("official/pohunek.runtime.muse/0.3.0.toml");
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, text).expect("entry");
    let result = tooling.xtask(&[
        "index",
        "check-entry",
        "--entries",
        s(&added),
        "--existing",
        s(&existing),
        "--core-version",
        "0.40.0",
        "--stage",
        "schema",
    ]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("predicate=identity-conflict"));
}

impl Tooling {
    /// A tree that holds `text` as its only entry.
    fn added_tree(&self, name: &str, text: &str) -> PathBuf {
        let tree = self.path(name);
        let file = tree.join("official/pohunek.runtime.muse/0.3.0.toml");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, text).expect("entry");
        tree
    }

    /// Runs `check-entry --stage schema` and returns exit code, stdout, stderr.
    fn check_schema(&self, added: &Path, existing: &Path) -> (Option<i32>, String, String) {
        let result = self.xtask(&[
            "index",
            "check-entry",
            "--entries",
            s(added),
            "--existing",
            s(existing),
            "--core-version",
            "0.40.0",
            "--stage",
            "schema",
        ]);
        (
            result.status.code(),
            String::from_utf8_lossy(&result.stdout).into_owned(),
            String::from_utf8_lossy(&result.stderr).into_owned(),
        )
    }
}

fn muse_entry() -> String {
    admission_entry("muse", "pohunek.runtime.muse", "0.3.0", '3', ">=0.40.0")
}

#[test]
fn check_entry_accepts_the_first_entry_into_an_index_that_holds_only_the_gitkeep() {
    let tooling = Tooling::new();
    let existing = tooling.path("empty-index");
    std::fs::create_dir_all(existing.join("official")).expect("mkdir");
    std::fs::write(existing.join("official/.gitkeep"), b"").expect("gitkeep");
    let added = tooling.added_tree("first-entry", &muse_entry());
    let (code, stdout, stderr) = tooling.check_schema(&added, &existing);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("package_id=pohunek.runtime.muse\n"));
}

#[test]
fn check_entry_refuses_an_added_tree_that_is_not_one_entry_file_and_every_source_fault() {
    let tooling = Tooling::new();
    let existing = tooling.path("empty-existing");
    std::fs::create_dir_all(existing.join("official")).expect("mkdir");

    let extra = tooling.added_tree("extra-file", &muse_entry());
    std::fs::write(extra.join("official/pohunek.runtime.muse/notes.txt"), b"x").expect("file");
    let linked = tooling.added_tree("symlinked", &muse_entry());
    symlink(
        "0.3.0.toml",
        linked.join("official/pohunek.runtime.muse/0.4.0.toml"),
    )
    .expect("link");
    let mut trees = vec![("extra file", extra), ("symlink", linked)];
    for (name, from, to) in [
        (
            "uppercase commit",
            "commit = \"cccccccccccccccccccccccccccccccccccccccc\"",
            "commit = \"CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC\"",
        ),
        (
            "dotdot repo",
            "repo = \"pohunekd/plugin-x\"",
            "repo = \"pohunekd/..\"",
        ),
        ("wrong tag", "tag = \"v0.3.0\"", "tag = \"v0.9.9\""),
        ("zero repo id", "repo_id = 1", "repo_id = 0"),
    ] {
        trees.push((
            name,
            tooling.added_tree(name, &muse_entry().replace(from, to)),
        ));
    }
    for (name, tree) in trees {
        let (code, stdout, stderr) = tooling.check_schema(&tree, &existing);
        assert_eq!(code, Some(1), "{name}: {stderr}");
        assert!(stderr.contains("predicate=schema"), "{name}: {stderr}");
        assert!(stdout.is_empty(), "{name}");
    }
}

#[test]
fn check_entry_refuses_an_entry_beyond_the_index_entry_limit() {
    let tooling = Tooling::new();
    let existing = tooling.path("full-index");
    for n in 0..256_u32 {
        let name = format!("r{n:04}");
        let package = format!("pohunek.runtime.{name}");
        let text = admission_entry(&name, &package, "0.1.0", '0', ">=0.40.0")
            .replace(&"0".repeat(64), &format!("{n:064x}"));
        let file = existing.join(format!("official/{package}/0.1.0.toml"));
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(file, text).expect("entry");
    }
    let added = tooling.added_tree("entry-257", &muse_entry());
    let (code, _, stderr) = tooling.check_schema(&added, &existing);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("predicate=schema"), "{stderr}");
    assert!(stderr.contains("above the limit"), "{stderr}");
}

#[test]
fn check_entry_never_lets_a_pull_request_name_start_a_log_line() {
    let tooling = Tooling::new();
    let text = muse_entry() + "\"x\\n::error title=spoof::admission ok\" = 1\n";
    let added = tooling.added_tree("hostile-key", &text);
    let (code, _, stderr) = tooling.check_schema(&added, &fixtures());
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("predicate=schema"), "{stderr}");
    assert!(
        stderr.lines().all(|line| !line.starts_with("::")),
        "a log line starts with a workflow command: {stderr}"
    );
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}

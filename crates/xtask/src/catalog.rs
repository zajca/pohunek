//! Runtime catalog assembly, signing and verification for release tooling.
//!
//! `catalog build` turns a package spec into the unsigned catalog body, with
//! each entry's digest, package id, runtime id, version and runtime API read
//! from the package archive itself, and checks the body with the rules the
//! daemon's verifier applies. `catalog sign` signs that body with a key file the
//! caller names by path (see [`crate::catalog_key`]) and verifies the result
//! before writing it, so a document the daemon would reject is never emitted.
//! `catalog verify` checks a catalog against an anchor file the way the daemon
//! does, and `catalog anchor` writes the anchor file a release ships.
//!
//! Every output is deterministic: entries, platforms, binary sets and
//! attestations are sorted,
//! documents are indented JSON with one trailing newline and Ed25519
//! signatures are deterministic.

// Rust guideline compliant 2026-10-08

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use nix::fcntl::OFlag;
use package::{
    catalog_document_bytes, check_catalog, parse_anchor, read_archive, sign_catalog,
    verify_catalog, AnchorFile, AnchorFileError, Attestation, Catalog, CatalogEntry, KeyId, Limits,
    PackageDigest, Release, RootKey, VerifiedCatalog, CATALOG_SCHEMA_VERSION, MAX_CATALOG_BYTES,
};
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde::Deserialize;

use crate::catalog_key::{read_public_key, read_signing_key};
use crate::XtaskError;

/// Schema version of the package spec read by `catalog build`.
const SPEC_SCHEMA_VERSION: u64 = 2;

/// Largest accepted spec or unsigned catalog file: the catalog size limit.
const MAX_SPEC_BYTES: usize = MAX_CATALOG_BYTES;

/// Mode of every file the tool writes: public documents, readable by all.
const OUTPUT_MODE: u32 = 0o644;

/// Archive member that holds the package descriptor.
const DESCRIPTOR_PATH: &str = "runtime.toml";

/// Window of the throwaway anchor a catalog is verified against right after
/// signing: the signer's own key, valid for all time.
const SELF_CHECK_WINDOW: (u64, u64) = (0, u64::MAX);

/// What `catalog build` reads.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    schema_version: u64,
    sequence: u64,
    expires_at: u64,
    release: Release,
    revoked_key_ids: Vec<KeyId>,
    revoked_digests: Vec<PackageDigest>,
    packages: Vec<PackageSpec>,
}

/// One package archive to list, with the compatibility the release claims.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageSpec {
    /// Archive path, absolute or relative to the spec file's directory.
    archive: PathBuf,
    platforms: Vec<String>,
    attestations: Vec<Attestation>,
    core: String,
}

pub(crate) fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> XtaskError {
    let path = path.to_path_buf();
    move |source| XtaskError::Io { path, source }
}

/// Reads the regular file at `path`, at most `max_bytes`, without following a
/// final symbolic link.
pub(crate) fn read_input(path: &Path, max_bytes: u64) -> Result<Vec<u8>, XtaskError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).bits())
        .open(path)
        .map_err(io_error(path))?;
    let metadata = file.metadata().map_err(io_error(path))?;
    if !metadata.is_file() {
        return Err(XtaskError::UnsupportedFileType(path.to_path_buf()));
    }
    if metadata.len() > max_bytes {
        return Err(XtaskError::Usage(format!(
            "`{}` is larger than the {max_bytes}-byte limit",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error(path))?;
    if u64::try_from(bytes.len()).map_or(true, |read| read > max_bytes) {
        return Err(XtaskError::Usage(format!(
            "`{}` is larger than the {max_bytes}-byte limit",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Writes `bytes` to `output`, refusing a symbolic link.
///
/// The open is no-follow, so a link is never written through.
pub(crate) fn write_output(output: &Path, bytes: &[u8]) -> Result<(), XtaskError> {
    let mut file: File = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(OUTPUT_MODE)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).bits())
        .open(output)
        .map_err(|error| {
            if error.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32) {
                XtaskError::OutputIsSymlink(output.to_path_buf())
            } else {
                XtaskError::Io {
                    path: output.to_path_buf(),
                    source: error,
                }
            }
        })?;
    file.write_all(bytes).map_err(io_error(output))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn pretty_with_newline<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, XtaskError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(XtaskError::Json)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Identity and runtime API a package descriptor declares.
struct Identity {
    package_id: PackageId,
    version: PackageVersion,
    runtime_id: RuntimeId,
    runtime_api: u32,
}

fn descriptor_identity(descriptor: &[u8]) -> Result<Identity, XtaskError> {
    let invalid = |reason: &str| {
        XtaskError::Usage(format!("package descriptor `{DESCRIPTOR_PATH}` {reason}"))
    };
    let text = std::str::from_utf8(descriptor).map_err(|_cause| invalid("is not UTF-8"))?;
    let table: toml::Table = text
        .parse()
        .map_err(|_cause| invalid("is not valid TOML"))?;
    let string = |value: Option<&toml::Value>, what: &str| {
        value
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| invalid(&format!("has no string `{what}`")))
    };
    let package_id = PackageId::parse(&string(table.get("id"), "id")?)
        .map_err(|_cause| invalid("has an invalid package `id`"))?;
    let version = PackageVersion::parse(&string(table.get("version"), "version")?)
        .map_err(|_cause| invalid("has an invalid `version`"))?;
    let runtime_api = table
        .get("runtime_api")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value >= 1)
        .ok_or_else(|| invalid("has no positive integer `runtime_api`"))?;
    let runtime = table
        .get("runtime")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| invalid("has no `[runtime]` table"))?;
    let runtime_id = RuntimeId::parse(&string(runtime.get("id"), "runtime.id")?)
        .map_err(|_cause| invalid("has an invalid `runtime.id`"))?;
    Ok(Identity {
        package_id,
        version,
        runtime_id,
        runtime_api,
    })
}

/// The catalog entry of the package archive at `archive`, listed for
/// `platforms` with their `attestations` and the core range `core`.
fn entry_from_archive(archive: &Path, package: &PackageSpec) -> Result<CatalogEntry, XtaskError> {
    let limits = Limits::DEFAULT;
    let bytes = read_input(archive, limits.max_compressed_bytes)?;
    let verified = read_archive(&bytes, &limits).map_err(XtaskError::Package)?;
    let descriptor = verified
        .entries()
        .iter()
        .find(|entry| entry.path == DESCRIPTOR_PATH)
        .ok_or_else(|| {
            XtaskError::Usage(format!(
                "package archive `{}` has no `{DESCRIPTOR_PATH}`",
                archive.display()
            ))
        })?;
    let identity = descriptor_identity(&descriptor.contents)?;
    let mut platforms = package.platforms.clone();
    platforms.sort();
    let mut attestations = package.attestations.clone();
    attestations.sort_by(|left, right| left.platform.cmp(&right.platform));
    Ok(CatalogEntry {
        package_id: identity.package_id,
        runtime_id: identity.runtime_id,
        version: identity.version,
        digest: verified.digest().clone(),
        runtime_api: identity.runtime_api,
        platforms,
        attestations,
        core: package.core.clone(),
    })
}

/// Builds the unsigned catalog body described by the spec at `spec_path`.
pub(crate) fn build(spec_path: &Path) -> Result<Catalog, XtaskError> {
    let bytes = read_input(spec_path, u64::try_from(MAX_SPEC_BYTES).unwrap_or(u64::MAX))?;
    let spec: Spec = serde_json::from_slice(&bytes).map_err(XtaskError::Json)?;
    if spec.schema_version != SPEC_SCHEMA_VERSION {
        return Err(XtaskError::Usage(format!(
            "unsupported catalog spec schema version (expected {SPEC_SCHEMA_VERSION})"
        )));
    }
    let base = spec_path.parent().unwrap_or_else(|| Path::new("."));
    let mut entries = Vec::with_capacity(spec.packages.len());
    for package in &spec.packages {
        let archive = base.join(&package.archive);
        entries.push(entry_from_archive(&archive, package)?);
    }
    entries.sort_by(|left, right| {
        (left.package_id.as_str(), left.version.as_str())
            .cmp(&(right.package_id.as_str(), right.version.as_str()))
    });
    let mut revoked_key_ids = spec.revoked_key_ids;
    revoked_key_ids.sort();
    let mut revoked_digests = spec.revoked_digests;
    revoked_digests.sort();
    let mut release = spec.release;
    release
        .binary_sets
        .sort_by(|left, right| left.target.cmp(&right.target));
    let catalog = Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence: spec.sequence,
        expires_at: spec.expires_at,
        release,
        revoked_key_ids,
        revoked_digests,
        entries,
    };
    check_catalog(&catalog).map_err(XtaskError::Catalog)?;
    Ok(catalog)
}

/// Builds the catalog of the spec and writes the unsigned body to `output`.
pub(crate) fn build_to(spec_path: &Path, output: &Path) -> Result<usize, XtaskError> {
    let catalog = build(spec_path)?;
    write_output(output, &pretty_with_newline(&catalog)?)?;
    Ok(catalog.entries.len())
}

/// A signed catalog document and who signed it.
pub(crate) struct Signed {
    pub(crate) document: Vec<u8>,
    pub(crate) signer: KeyId,
}

/// Signs the unsigned catalog at `catalog_path` with `key`, which must have
/// the key id `expected_id`.
///
/// The document is verified against an anchor holding only the signer's key
/// at `now` before it is returned, so an expired or invalid catalog is
/// refused.
pub(crate) fn sign_with(
    catalog_path: &Path,
    key: &SigningKey,
    expected_id: &KeyId,
    now: u64,
) -> Result<Signed, XtaskError> {
    let signer = KeyId::derive(&key.verifying_key());
    if &signer != expected_id {
        return Err(XtaskError::Usage(
            "the key file does not have the key id given with --key-id".to_owned(),
        ));
    }
    let bytes = read_input(
        catalog_path,
        u64::try_from(MAX_SPEC_BYTES).unwrap_or(u64::MAX),
    )?;
    let catalog: Catalog = serde_json::from_slice(&bytes).map_err(XtaskError::Json)?;
    let envelope = sign_catalog(catalog, key).map_err(XtaskError::Catalog)?;
    let document = catalog_document_bytes(&envelope).map_err(XtaskError::Catalog)?;

    let root = RootKey::new(
        key.verifying_key().to_bytes(),
        SELF_CHECK_WINDOW.0,
        SELF_CHECK_WINDOW.1,
    )
    .map_err(|error| XtaskError::Anchor(AnchorFileError::Anchor(error)))?;
    let anchor = AnchorFile::new(vec![root], Vec::new())
        .and_then(|file| file.trust_anchor())
        .map_err(|error| XtaskError::Anchor(AnchorFileError::Anchor(error)))?;
    verify_catalog(&document, &anchor, now, None).map_err(XtaskError::Catalog)?;
    Ok(Signed { document, signer })
}

/// Signs the catalog at `catalog_path` with the key file at `key_file` and
/// writes the document to `output`.
pub(crate) fn sign_to(
    catalog_path: &Path,
    key_file: &Path,
    key_id: &str,
    output: &Path,
    now: u64,
) -> Result<KeyId, XtaskError> {
    let expected = KeyId::parse(key_id).map_err(|_cause| {
        XtaskError::Usage("--key-id is not 64 lowercase hex characters".into())
    })?;
    let key = read_signing_key(key_file)?;
    let signed = sign_with(catalog_path, &key, &expected, now)?;
    write_output(output, &signed.document)?;
    Ok(signed.signer)
}

/// What a successful verification reports.
pub(crate) struct Verified {
    pub(crate) sequence: u64,
    pub(crate) expires_at: u64,
    pub(crate) signer: KeyId,
    pub(crate) entries: Vec<String>,
}

/// Verifies the signed catalog `document` against the anchor file bytes
/// `anchor_bytes` at `now`, with `high_water` as the lowest acceptable
/// sequence.
pub(crate) fn verify_bytes(
    document: &[u8],
    anchor_bytes: &[u8],
    high_water: Option<u64>,
    now: u64,
) -> Result<VerifiedCatalog, XtaskError> {
    let anchor = parse_anchor(anchor_bytes)
        .map_err(XtaskError::Anchor)?
        .trust_anchor()
        .map_err(|error| XtaskError::Anchor(AnchorFileError::Anchor(error)))?;
    verify_catalog(document, &anchor, now, high_water).map_err(XtaskError::Catalog)
}

/// Verifies the catalog at `catalog_path` against the anchor file at
/// `anchor_path` at `now`, with `high_water` as the lowest acceptable
/// sequence.
pub(crate) fn verify(
    catalog_path: &Path,
    anchor_path: &Path,
    high_water: Option<u64>,
    now: u64,
) -> Result<Verified, XtaskError> {
    let anchor_bytes = read_input(
        anchor_path,
        u64::try_from(package::MAX_ANCHOR_BYTES).unwrap_or(u64::MAX),
    )?;
    let document = read_input(
        catalog_path,
        u64::try_from(MAX_CATALOG_BYTES).unwrap_or(u64::MAX),
    )?;
    let verified = verify_bytes(&document, &anchor_bytes, high_water, now)?;
    let entries = verified
        .entries()
        .iter()
        .map(|entry| {
            format!(
                "{} {} {} api={} {} [{}] {}",
                entry.package_id().as_str(),
                entry.version().as_str(),
                entry.runtime_id().as_str(),
                entry.runtime_api(),
                entry.digest().as_str(),
                entry.platforms().join(", "),
                entry.core(),
            )
        })
        .collect();
    Ok(Verified {
        sequence: verified.sequence(),
        expires_at: verified.expires_at(),
        signer: verified.signer().clone(),
        entries,
    })
}

/// One trusted root requested from `catalog anchor`: the public key file and
/// the validity window the root may sign in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RootSpec {
    public_key_file: PathBuf,
    not_before: u64,
    not_after: u64,
}

impl RootSpec {
    #[cfg(test)]
    pub(crate) fn new(public_key_file: &Path, not_before: u64, not_after: u64) -> Self {
        Self {
            public_key_file: public_key_file.to_path_buf(),
            not_before,
            not_after,
        }
    }
}

impl std::str::FromStr for RootSpec {
    type Err = String;

    /// Parses `<public-key-file>:<not-before>:<not-after>`.
    ///
    /// The two window values are split off from the right, so the file path
    /// may itself contain colons.
    fn from_str(value: &str) -> Result<Self, String> {
        const FORM: &str = "expected <public-key-file>:<not-before>:<not-after>";
        let mut parts = value.rsplitn(3, ':');
        let (Some(not_after), Some(not_before), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(FORM.to_owned());
        };
        if path.is_empty() {
            return Err(format!("the public key file is empty: {FORM}"));
        }
        let seconds = |text: &str, name: &str| {
            text.parse::<u64>()
                .map_err(|_cause| format!("{name} is not a Unix second count: {FORM}"))
        };
        Ok(Self {
            public_key_file: PathBuf::from(path),
            not_before: seconds(not_before, "not-before")?,
            not_after: seconds(not_after, "not-after")?,
        })
    }
}

/// The anchor file bytes for `roots` and the `revoked` key ids.
///
/// Root count, repeated keys and windows are judged by [`AnchorFile::new`] and
/// [`RootKey::new`]; the bytes list the roots in ascending key id order.
pub(crate) fn anchor_bytes(roots: &[RootSpec], revoked: &[String]) -> Result<Vec<u8>, XtaskError> {
    let roots = roots
        .iter()
        .map(|spec| {
            let public_key = read_public_key(&spec.public_key_file)?;
            RootKey::new(public_key, spec.not_before, spec.not_after)
                .map_err(|error| XtaskError::Anchor(AnchorFileError::Anchor(error)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let revoked = revoked
        .iter()
        .map(|id| {
            KeyId::parse(id).map_err(|_cause| {
                XtaskError::Usage("--revoked-key-id is not 64 lowercase hex characters".into())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let anchor = AnchorFile::new(roots, revoked)
        .map_err(|error| XtaskError::Anchor(AnchorFileError::Anchor(error)))?;
    anchor.to_bytes().map_err(XtaskError::Anchor)
}

/// Writes the anchor file for `roots` to `output`; nothing is written when
/// the anchor is refused.
pub(crate) fn anchor_to(
    roots: &[RootSpec],
    revoked: &[String],
    output: &Path,
) -> Result<(), XtaskError> {
    write_output(output, &anchor_bytes(roots, revoked)?)
}

/// The key id and public key of the signing key file at `key_file`.
pub(crate) fn public_key_of(key_file: &Path) -> Result<(KeyId, String), XtaskError> {
    let key = read_signing_key(key_file)?;
    let public = key.verifying_key();
    Ok((KeyId::derive(&public), hex(&public.to_bytes())))
}

/// Seconds since the Unix epoch.
pub(crate) fn unix_now() -> Result<u64, XtaskError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_cause| XtaskError::Usage("the system clock is before the Unix epoch".into()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt as _};

    use package::{
        build_archive, ArchiveEntry, CatalogEntryRejection, CatalogError, ReleaseRejection,
        TrustAnchorError, MAX_REVOKED_KEYS, MAX_TRUST_ROOTS,
    };
    use serde_json::json;

    use super::*;

    const SEED: u8 = 11;
    const FAR_FUTURE: u64 = 4_102_444_800;
    const NOW: u64 = 1_800_000_000;
    const PLATFORM: &str = "x86_64-unknown-linux-gnu";
    const RELEASE_VERSION: &str = "1.0.0";
    const RELEASE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A `sha256:` digest of 64 copies of `byte`.
    fn sha(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    /// The spec entry of `archive` for `platforms`, with attestations listed
    /// in reverse platform order and a core range that admits the release.
    fn package(archive: &str, platforms: &[&str]) -> serde_json::Value {
        let attestations: Vec<_> = platforms
            .iter()
            .rev()
            .map(|platform| json!({ "platform": platform, "digest": sha('7') }))
            .collect();
        json!({
            "archive": archive,
            "platforms": platforms,
            "attestations": attestations,
            "core": ">=0.0.0",
        })
    }

    /// A release block with a binary set for every target the tests use,
    /// listed out of order.
    fn release_block() -> serde_json::Value {
        json!({
            "version": RELEASE_VERSION,
            "commit": RELEASE_COMMIT,
            "binary_sets": [
                { "target": "b-platform", "digest": sha('b') },
                { "target": PLATFORM, "digest": sha('9') },
                { "target": "a-platform", "digest": sha('a') },
            ],
        })
    }

    fn descriptor(package_id: &str, version: &str, runtime: &str) -> String {
        format!(
            "schema = 1\nid = \"{package_id}\"\nversion = \"{version}\"\nruntime_api = 1\n\n[runtime]\nid = \"{runtime}\"\nname = \"X\"\nprogram = \"x\"\n"
        )
    }

    fn write_archive(dir: &Path, name: &str, descriptor: &str) -> (PathBuf, PackageDigest) {
        let entries = [ArchiveEntry {
            path: DESCRIPTOR_PATH.to_owned(),
            contents: descriptor.as_bytes().to_vec(),
            executable: false,
        }];
        let bytes = build_archive(&entries, &Limits::DEFAULT).expect("archive");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("reads")
            .digest()
            .clone();
        let path = dir.join(name);
        fs::write(&path, bytes).expect("write archive");
        (path, digest)
    }

    fn write_spec(dir: &Path, packages: &serde_json::Value) -> PathBuf {
        let path = dir.join("spec.json");
        let spec = json!({
            "schema_version": 2,
            "sequence": 4,
            "expires_at": FAR_FUTURE,
            "release": release_block(),
            "revoked_key_ids": [],
            "revoked_digests": [],
            "packages": packages,
        });
        fs::write(&path, serde_json::to_vec(&spec).expect("json")).expect("write spec");
        path
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[SEED; 32])
    }

    fn key_id() -> KeyId {
        KeyId::derive(&key().verifying_key())
    }

    /// A key file and the public key file of [`key`].
    fn key_files(dir: &Path) -> (PathBuf, PathBuf) {
        let private = dir.join("signing.key");
        fs::write(&private, hex(&[SEED; 32])).expect("write key");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).expect("mode");
        let public = dir.join("public.key");
        fs::write(&public, hex(&key().verifying_key().to_bytes())).expect("write public key");
        (private, public)
    }

    struct Release {
        dir: tempfile::TempDir,
        spec: PathBuf,
        digest: PackageDigest,
    }

    fn release() -> Release {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let (_archive, digest) = write_archive(
            dir.path(),
            "codex.tar.zst",
            &descriptor("pohunek.runtime.codex", "1.0.0", "codex"),
        );
        let spec = write_spec(dir.path(), &json!([package("codex.tar.zst", &[PLATFORM])]));
        Release { dir, spec, digest }
    }

    fn built(release: &Release) -> PathBuf {
        let unsigned = release.dir.path().join("unsigned.json");
        build_to(&release.spec, &unsigned).expect("build");
        unsigned
    }

    fn signed(release: &Release) -> PathBuf {
        let unsigned = built(release);
        let (private, _public) = key_files(release.dir.path());
        let out = release.dir.path().join("runtime-catalog.json");
        sign_to(&unsigned, &private, key_id().as_str(), &out, NOW).expect("sign");
        out
    }

    fn anchor_file(release: &Release, name: &str, not_before: u64, not_after: u64) -> PathBuf {
        let (_private, public) = key_files(release.dir.path());
        let out = release.dir.path().join(name);
        anchor_to(&[RootSpec::new(&public, not_before, not_after)], &[], &out).expect("anchor");
        out
    }

    /// The anchor inputs and the checked-in anchor under `packaging/`.
    struct ShippedAnchor {
        trust_dir: PathBuf,
        roots: Vec<RootSpec>,
        checked_in: Vec<u8>,
    }

    fn shipped_anchor() -> ShippedAnchor {
        let packaging = pohunek_test_support::workspace_root().join("packaging");
        let trust_dir = packaging.join("catalog-trust");
        let roots_text = fs::read_to_string(trust_dir.join("roots.txt")).expect("roots.txt");
        let roots = roots_text
            .lines()
            .map(|line| {
                format!("{}/{line}", trust_dir.display())
                    .parse::<RootSpec>()
                    .expect("roots.txt line")
            })
            .collect();
        let checked_in = fs::read(packaging.join("runtime-catalog-anchor.json")).expect("anchor");
        ShippedAnchor {
            trust_dir,
            roots,
            checked_in,
        }
    }

    #[test]
    fn the_checked_in_anchor_is_what_the_shipped_inputs_produce() {
        let shipped = shipped_anchor();

        let regenerated = anchor_bytes(&shipped.roots, &[]).expect("anchor");
        assert_eq!(
            regenerated, shipped.checked_in,
            "packaging/runtime-catalog-anchor.json differs from `catalog anchor` over \
             packaging/catalog-trust/roots.txt"
        );

        assert!(shipped.checked_in.len() <= package::MAX_ANCHOR_BYTES);
        let anchor = package::parse_anchor(&shipped.checked_in).expect("parses");
        assert!(anchor.revoked_key_ids().is_empty());
        let [ci, primary] = anchor.roots() else {
            panic!("exactly two roots expected");
        };
        for (root, file) in [(ci, "ci.pub"), (primary, "primary.pub")] {
            let path = shipped.trust_dir.join(file);
            let text = fs::read_to_string(&path).expect("public key file");
            assert!(
                text.len() == 65
                    && text.ends_with('\n')
                    && text[..64]
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{file} must hold 64 lowercase hex characters and one newline"
            );
            let bytes = read_public_key(&path).expect("public key");
            let verifying = ed25519_dalek::VerifyingKey::from_bytes(&bytes).expect("key");
            assert_eq!(root.id(), &KeyId::derive(&verifying), "{file}");
        }
        assert!(
            ci.not_after() < primary.not_after(),
            "the CI root must expire before the offline primary"
        );
    }

    #[test]
    fn an_entry_takes_its_identity_and_digest_from_the_archive() {
        let release = release();
        let catalog = build(&release.spec).expect("build");
        assert_eq!(catalog.sequence, 4);
        assert_eq!(catalog.expires_at, FAR_FUTURE);
        let [entry] = catalog.entries.as_slice() else {
            panic!("one entry expected");
        };
        assert_eq!(entry.package_id.as_str(), "pohunek.runtime.codex");
        assert_eq!(entry.runtime_id.as_str(), "codex");
        assert_eq!(entry.version.as_str(), "1.0.0");
        assert_eq!(entry.digest, release.digest);
        assert_eq!(entry.runtime_api, 1);
        assert_eq!(entry.platforms, [PLATFORM]);
        assert_eq!(entry.attestations.len(), 1);
        assert_eq!(entry.attestations[0].platform, PLATFORM);
        assert_eq!(entry.attestations[0].digest.as_str(), sha('7'));
        assert_eq!(entry.core, ">=0.0.0");
        assert_eq!(catalog.release.version, RELEASE_VERSION);
        assert_eq!(catalog.release.commit, RELEASE_COMMIT);
    }

    #[test]
    fn the_unsigned_catalog_is_deterministic_and_sorted() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        write_archive(
            dir.path(),
            "b.tar.zst",
            &descriptor("pohunek.runtime.hermes", "1.0.0", "hermes"),
        );
        write_archive(
            dir.path(),
            "a2.tar.zst",
            &descriptor("pohunek.runtime.claude", "2.0.0", "claude"),
        );
        write_archive(
            dir.path(),
            "a1.tar.zst",
            &descriptor("pohunek.runtime.claude", "1.0.0", "claude"),
        );
        let spec = write_spec(
            dir.path(),
            &json!([
                package("b.tar.zst", &["b-platform", "a-platform"]),
                package("a2.tar.zst", &[PLATFORM]),
                package("a1.tar.zst", &[PLATFORM]),
            ]),
        );
        let first = dir.path().join("first.json");
        let second = dir.path().join("second.json");
        build_to(&spec, &first).expect("build");
        build_to(&spec, &second).expect("build");
        assert_eq!(
            fs::read(&first).expect("read"),
            fs::read(&second).expect("read")
        );

        let catalog = build(&spec).expect("build");
        let order: Vec<(&str, &str)> = catalog
            .entries
            .iter()
            .map(|entry| (entry.package_id.as_str(), entry.version.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("pohunek.runtime.claude", "1.0.0"),
                ("pohunek.runtime.claude", "2.0.0"),
                ("pohunek.runtime.hermes", "1.0.0"),
            ]
        );
        assert_eq!(catalog.entries[2].platforms, ["a-platform", "b-platform"]);
        let attested: Vec<&str> = catalog.entries[2]
            .attestations
            .iter()
            .map(|record| record.platform.as_str())
            .collect();
        assert_eq!(attested, ["a-platform", "b-platform"]);
        let targets: Vec<&str> = catalog
            .release
            .binary_sets
            .iter()
            .map(|set| set.target.as_str())
            .collect();
        assert_eq!(targets, ["a-platform", "b-platform", PLATFORM]);
    }

    #[test]
    fn build_refuses_archives_it_cannot_read_an_identity_from() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let spec_for = |name: &str| write_spec(dir.path(), &json!([package(name, &[PLATFORM])]));

        let entries = [ArchiveEntry {
            path: "other.toml".to_owned(),
            contents: b"x".to_vec(),
            executable: false,
        }];
        fs::write(
            dir.path().join("no-descriptor.tar.zst"),
            build_archive(&entries, &Limits::DEFAULT).expect("archive"),
        )
        .expect("write");
        let error = build(&spec_for("no-descriptor.tar.zst")).expect_err("no descriptor");
        assert!(error.to_string().contains("no `runtime.toml`"), "{error}");

        write_archive(dir.path(), "toml.tar.zst", "this is = = not toml");
        build(&spec_for("toml.tar.zst")).expect_err("not toml");
        write_archive(
            dir.path(),
            "id.tar.zst",
            &descriptor("Not A Package Id", "1.0.0", "codex"),
        );
        build(&spec_for("id.tar.zst")).expect_err("bad package id");
        write_archive(
            dir.path(),
            "missing-runtime.tar.zst",
            "id = \"a.b\"\nversion = \"1.0.0\"\n",
        );
        build(&spec_for("missing-runtime.tar.zst")).expect_err("no runtime table");

        fs::write(dir.path().join("garbage.tar.zst"), b"not an archive").expect("write");
        assert!(matches!(
            build(&spec_for("garbage.tar.zst")),
            Err(XtaskError::Package(_))
        ));
        assert!(matches!(
            build(&spec_for("absent.tar.zst")),
            Err(XtaskError::Io { .. })
        ));
    }

    #[test]
    fn the_runtime_api_comes_from_the_archive_descriptor_and_never_from_the_spec() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        write_archive(
            dir.path(),
            "api3.tar.zst",
            &descriptor("pohunek.runtime.codex", "1.0.0", "codex")
                .replace("runtime_api = 1", "runtime_api = 3"),
        );
        let spec = write_spec(dir.path(), &json!([package("api3.tar.zst", &[PLATFORM])]));
        assert_eq!(build(&spec).expect("build").entries[0].runtime_api, 3);

        let mut member = package("api3.tar.zst", &[PLATFORM]);
        member["runtime_api"] = json!(1);
        let spec = write_spec(dir.path(), &json!([member]));
        assert!(matches!(build(&spec), Err(XtaskError::Json(_))));

        for (name, replacement) in [
            ("zero", "runtime_api = 0"),
            ("negative", "runtime_api = -1"),
            ("beyond u32", "runtime_api = 4294967296"),
            ("text", "runtime_api = \"1\""),
            ("absent", ""),
        ] {
            let text = descriptor("pohunek.runtime.codex", "1.0.0", "codex")
                .replace("runtime_api = 1", replacement);
            write_archive(dir.path(), "bad.tar.zst", &text);
            let spec = write_spec(dir.path(), &json!([package("bad.tar.zst", &[PLATFORM])]));
            let error = build(&spec).expect_err(name);
            assert!(error.to_string().contains("runtime_api"), "{name}: {error}");
        }
    }

    #[test]
    fn build_refuses_a_catalog_the_verifier_would_refuse() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        write_archive(
            dir.path(),
            "codex.tar.zst",
            &descriptor("pohunek.runtime.codex", "1.0.0", "codex"),
        );
        let reject = |edit: &dyn Fn(&mut serde_json::Value), reason: CatalogEntryRejection| {
            let mut entry = package("codex.tar.zst", &[PLATFORM]);
            edit(&mut entry);
            let spec = write_spec(dir.path(), &json!([entry]));
            match build(&spec) {
                Err(XtaskError::Catalog(CatalogError::Entry { reason: found, .. })) => {
                    assert_eq!(found, reason);
                }
                other => panic!("expected {reason:?}, got {other:?}"),
            }
        };
        reject(
            &|entry| entry["core"] = json!(">=1.0.1"),
            CatalogEntryRejection::CoreExcludesRelease,
        );
        reject(
            &|entry| entry["core"] = json!("*"),
            CatalogEntryRejection::CoreRange,
        );
        reject(
            &|entry| entry["attestations"] = json!([]),
            CatalogEntryRejection::Attestations,
        );
        reject(
            &|entry| entry["platforms"] = json!([PLATFORM, "a-platform"]),
            CatalogEntryRejection::MissingAttestation,
        );
        reject(
            &|entry| {
                entry["attestations"]
                    .as_array_mut()
                    .expect("array")
                    .push(json!({ "platform": "a-platform", "digest": sha('7') }));
            },
            CatalogEntryRejection::AttestationPlatform,
        );
        reject(
            &|entry| {
                entry["platforms"] = json!(["c-platform"]);
                entry["attestations"] = json!([{ "platform": "c-platform", "digest": sha('7') }]);
            },
            CatalogEntryRejection::NoBinarySet,
        );

        // The spec must give the core range explicitly.
        let mut entry = package("codex.tar.zst", &[PLATFORM]);
        entry.as_object_mut().expect("object").remove("core");
        let spec = write_spec(dir.path(), &json!([entry]));
        assert!(matches!(build(&spec), Err(XtaskError::Json(_))));
    }

    #[test]
    fn build_refuses_an_invalid_release_block() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        write_archive(
            dir.path(),
            "codex.tar.zst",
            &descriptor("pohunek.runtime.codex", "1.0.0", "codex"),
        );
        let packages = json!([package("codex.tar.zst", &[PLATFORM])]);
        let spec = dir.path().join("spec.json");
        let with = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut value = json!({
                "schema_version": 2, "sequence": 1, "expires_at": FAR_FUTURE,
                "release": release_block(),
                "revoked_key_ids": [], "revoked_digests": [], "packages": packages,
            });
            edit(&mut value);
            fs::write(&spec, serde_json::to_vec(&value).expect("json")).expect("write");
            build(&spec)
        };
        assert!(matches!(
            with(&|value| value["release"]["version"] = json!("1.0.0-rc.1")),
            Err(XtaskError::Catalog(CatalogError::Release(
                ReleaseRejection::Version
            )))
        ));
        assert!(matches!(
            with(&|value| value["release"]["commit"] = json!("abc")),
            Err(XtaskError::Catalog(CatalogError::Release(
                ReleaseRejection::Commit
            )))
        ));
        assert!(matches!(
            with(&|value| value["release"]["binary_sets"] = json!([])),
            Err(XtaskError::Catalog(CatalogError::Release(
                ReleaseRejection::BinarySets
            )))
        ));
        assert!(matches!(
            with(&|value| value["release"]["binary_sets"][0]["target"] = json!(PLATFORM)),
            Err(XtaskError::Catalog(CatalogError::Release(
                ReleaseRejection::DuplicateTarget
            )))
        ));
        assert!(matches!(
            with(&|value| value["release"]["binary_sets"][0]["digest"] = json!("sha256:zz")),
            Err(XtaskError::Json(_))
        ));
        assert!(matches!(
            with(&|value| {
                value.as_object_mut().expect("object").remove("release");
            }),
            Err(XtaskError::Json(_))
        ));
    }

    #[test]
    fn build_refuses_unknown_spec_members_and_versions_and_links() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let spec = dir.path().join("spec.json");
        let base = json!({
            "schema_version": 2, "sequence": 1, "expires_at": FAR_FUTURE,
            "release": release_block(),
            "revoked_key_ids": [], "revoked_digests": [], "packages": []
        });
        let mut extra = base.clone();
        extra["extra"] = json!(true);
        fs::write(&spec, serde_json::to_vec(&extra).expect("json")).expect("write");
        assert!(matches!(build(&spec), Err(XtaskError::Json(_))));
        let mut version = base.clone();
        version["schema_version"] = json!(1);
        fs::write(&spec, serde_json::to_vec(&version).expect("json")).expect("write");
        assert!(matches!(build(&spec), Err(XtaskError::Usage(_))));

        let real = dir.path().join("real.json");
        fs::write(&real, serde_json::to_vec(&base).expect("json")).expect("write");
        let link = dir.path().join("link.json");
        symlink(&real, &link).expect("symlink");
        assert!(matches!(build(&link), Err(XtaskError::Io { .. })));
    }

    #[test]
    fn a_signed_catalog_verifies_against_the_anchor_of_its_key_and_lists_its_entries() {
        let release = release();
        let catalog = signed(&release);
        let anchor = anchor_file(&release, "anchor.json", 1, FAR_FUTURE);
        let verified = verify(&catalog, &anchor, None, NOW).expect("verify");
        assert_eq!(verified.sequence, 4);
        assert_eq!(verified.signer, key_id());
        assert_eq!(verified.entries.len(), 1);
        assert!(verified.entries[0].contains(release.digest.as_str()));
        assert!(verified.entries[0].contains("pohunek.runtime.codex"));
    }

    #[test]
    fn signing_is_byte_for_byte_reproducible() {
        let release = release();
        let first = fs::read(signed(&release)).expect("read");
        let second = fs::read(signed(&release)).expect("read");
        assert_eq!(first, second);
        assert!(first.ends_with(b"\n"));
    }

    #[test]
    fn sign_refuses_a_key_whose_id_is_not_the_one_given() {
        let release = release();
        let unsigned = built(&release);
        let (private, _public) = key_files(release.dir.path());
        let other = KeyId::derive(&SigningKey::from_bytes(&[SEED + 1; 32]).verifying_key());
        let out = release.dir.path().join("out.json");
        let error = sign_to(&unsigned, &private, other.as_str(), &out, NOW).expect_err("mismatch");
        assert!(error.to_string().contains("--key-id"), "{error}");
        assert!(!out.exists(), "nothing is written for a refused key");
        sign_to(&unsigned, &private, "nothex", &out, NOW).expect_err("bad key id");
    }

    #[test]
    fn sign_refuses_an_expired_catalog_and_an_invalid_entry_instead_of_emitting_them() {
        let release = release();
        let unsigned = built(&release);
        let (private, _public) = key_files(release.dir.path());
        let out = release.dir.path().join("out.json");
        let error =
            sign_to(&unsigned, &private, key_id().as_str(), &out, FAR_FUTURE).expect_err("expired");
        assert!(matches!(error, XtaskError::Catalog(CatalogError::Expired)));
        assert!(!out.exists());

        // An unsigned body edited by hand after `build` is judged again at sign.
        let dir = release.dir.path();
        let mut body: serde_json::Value =
            serde_json::from_slice(&fs::read(&unsigned).expect("read")).expect("json");
        body["entries"][0]["runtime_id"] = json!("shell");
        let unsigned = dir.join("shell-unsigned.json");
        fs::write(&unsigned, serde_json::to_vec(&body).expect("json")).expect("write");
        let error = sign_to(&unsigned, &private, key_id().as_str(), &out, NOW)
            .expect_err("a shell entry is invalid");
        assert!(matches!(
            error,
            XtaskError::Catalog(CatalogError::Entry {
                reason: CatalogEntryRejection::ReservedShell,
                ..
            })
        ));
        assert!(!out.exists());
    }

    #[test]
    fn sign_refuses_an_unsafe_key_file_and_writes_nothing() {
        let release = release();
        let unsigned = built(&release);
        let (private, _public) = key_files(release.dir.path());
        fs::set_permissions(&private, fs::Permissions::from_mode(0o644)).expect("mode");
        let out = release.dir.path().join("out.json");
        let error = sign_to(&unsigned, &private, key_id().as_str(), &out, NOW).expect_err("unsafe");
        assert!(matches!(error, XtaskError::KeyFile { .. }), "{error}");
        assert!(!out.exists());
    }

    #[test]
    fn outputs_are_never_written_through_a_symbolic_link() {
        let release = release();
        let unsigned = built(&release);
        let (private, _public) = key_files(release.dir.path());
        let victim = release.dir.path().join("victim");
        fs::write(&victim, b"keep").expect("write");
        let link = release.dir.path().join("link.json");
        symlink(&victim, &link).expect("symlink");
        assert!(matches!(
            sign_to(&unsigned, &private, key_id().as_str(), &link, NOW),
            Err(XtaskError::OutputIsSymlink(_))
        ));
        assert!(matches!(
            build_to(&release.spec, &link),
            Err(XtaskError::OutputIsSymlink(_))
        ));
        assert_eq!(fs::read(&victim).expect("read"), b"keep");
    }

    #[test]
    fn verification_fails_closed_on_a_tampered_catalog_the_wrong_anchor_and_a_stale_or_expired_one()
    {
        let release = release();
        let catalog = signed(&release);
        let anchor = anchor_file(&release, "anchor.json", 1, FAR_FUTURE);

        let mut text = fs::read_to_string(&catalog).expect("read");
        text = text.replace("\"sequence\": 4", "\"sequence\": 5");
        let tampered = release.dir.path().join("tampered.json");
        fs::write(&tampered, text).expect("write");
        assert!(matches!(
            verify(&tampered, &anchor, None, NOW),
            Err(XtaskError::Catalog(CatalogError::BadSignature))
        ));

        let other = release.dir.path().join("other-public.key");
        fs::write(
            &other,
            hex(&SigningKey::from_bytes(&[SEED + 1; 32])
                .verifying_key()
                .to_bytes()),
        )
        .expect("write");
        let other_anchor = release.dir.path().join("other-anchor.json");
        anchor_to(&[RootSpec::new(&other, 1, FAR_FUTURE)], &[], &other_anchor).expect("anchor");
        assert!(matches!(
            verify(&catalog, &other_anchor, None, NOW),
            Err(XtaskError::Catalog(CatalogError::UnknownSigner))
        ));

        assert!(matches!(
            verify(&catalog, &anchor, Some(5), NOW),
            Err(XtaskError::Catalog(CatalogError::StaleSequence { .. }))
        ));
        let unbounded = anchor_file(&release, "unbounded.json", 1, u64::MAX);
        assert!(matches!(
            verify(&catalog, &unbounded, None, FAR_FUTURE),
            Err(XtaskError::Catalog(CatalogError::Expired))
        ));
        assert!(matches!(
            verify(&catalog, &anchor, None, FAR_FUTURE),
            Err(XtaskError::Catalog(CatalogError::SignerExpired))
        ));
        let late_root = anchor_file(&release, "late.json", NOW + 1, FAR_FUTURE);
        assert!(matches!(
            verify(&catalog, &late_root, None, NOW),
            Err(XtaskError::Catalog(CatalogError::SignerNotYetValid))
        ));
    }

    #[test]
    fn an_anchor_revoking_the_signer_refuses_the_catalog() {
        let release = release();
        let catalog = signed(&release);
        let (_private, public) = key_files(release.dir.path());
        let anchor = release.dir.path().join("revoking-anchor.json");
        anchor_to(
            &[RootSpec::new(&public, 1, FAR_FUTURE)],
            &[key_id().as_str().to_owned()],
            &anchor,
        )
        .expect("anchor");
        assert!(matches!(
            verify(&catalog, &anchor, None, NOW),
            Err(XtaskError::Catalog(CatalogError::SignerRevoked))
        ));
    }

    #[test]
    fn the_anchor_file_is_deterministic_validated_and_reads_back() {
        let release = release();
        let first = anchor_file(&release, "anchor.json", 1, FAR_FUTURE);
        let second = release.dir.path().join("second-anchor.json");
        let (_private, public) = key_files(release.dir.path());
        anchor_to(&[RootSpec::new(&public, 1, FAR_FUTURE)], &[], &second).expect("anchor");
        assert_eq!(
            fs::read(&first).expect("read"),
            fs::read(&second).expect("read")
        );
        let parsed = parse_anchor(&fs::read(&first).expect("read")).expect("parse");
        assert_eq!(parsed.roots()[0].id(), &key_id());

        let bad_public = release.dir.path().join("bad.key");
        fs::write(&bad_public, "not a key").expect("write");
        assert!(anchor_to(&[RootSpec::new(&bad_public, 1, 2)], &[], &second).is_err());
    }

    const CI_SEED: u8 = SEED + 1;
    const CI_WINDOW: (u64, u64) = (NOW - 100, NOW + 100);

    fn ci_key() -> SigningKey {
        SigningKey::from_bytes(&[CI_SEED; 32])
    }

    /// The owner-private key file and public key file of the CI root.
    fn ci_key_files(dir: &Path) -> (PathBuf, PathBuf) {
        let private = dir.join("ci-signing.key");
        fs::write(&private, hex(&[CI_SEED; 32])).expect("write key");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).expect("mode");
        let public = dir.join("ci-public.key");
        fs::write(&public, hex(&ci_key().verifying_key().to_bytes())).expect("write public key");
        (private, public)
    }

    fn ci_key_id() -> KeyId {
        KeyId::derive(&ci_key().verifying_key())
    }

    /// An anchor listing the primary root for all time and the CI root for
    /// [`CI_WINDOW`], with `revoked` revoked.
    fn two_root_anchor(release: &Release, name: &str, revoked: &[String]) -> PathBuf {
        let dir = release.dir.path();
        let (_private, primary) = key_files(dir);
        let (_ci_private, ci) = ci_key_files(dir);
        let out = dir.join(name);
        anchor_to(
            &[
                RootSpec::new(&primary, 1, FAR_FUTURE),
                RootSpec::new(&ci, CI_WINDOW.0, CI_WINDOW.1),
            ],
            revoked,
            &out,
        )
        .expect("anchor");
        out
    }

    fn signed_by_ci(release: &Release) -> PathBuf {
        let unsigned = built(release);
        let (private, _public) = ci_key_files(release.dir.path());
        let out = release.dir.path().join("ci-catalog.json");
        sign_to(&unsigned, &private, ci_key_id().as_str(), &out, NOW).expect("sign");
        out
    }

    #[test]
    fn a_two_root_anchor_reads_back_and_verifies_a_catalog_signed_by_either_root() {
        let release = release();
        let anchor = two_root_anchor(&release, "anchor.json", &[]);
        let parsed = parse_anchor(&fs::read(&anchor).expect("read")).expect("parse");
        let ids: Vec<&KeyId> = parsed.roots().iter().map(RootKey::id).collect();
        let mut expected = [key_id(), ci_key_id()];
        expected.sort();
        assert_eq!(ids, expected.iter().collect::<Vec<_>>());
        for root in parsed.roots() {
            let window = if root.id() == &ci_key_id() {
                CI_WINDOW
            } else {
                (1, FAR_FUTURE)
            };
            assert_eq!((root.not_before(), root.not_after()), window);
        }

        let primary = signed(&release);
        let verified = verify(&primary, &anchor, None, NOW).expect("primary root");
        assert_eq!(verified.signer, key_id());
        let ci = signed_by_ci(&release);
        let verified = verify(&ci, &anchor, None, NOW).expect("ci root");
        assert_eq!(verified.signer, ci_key_id());
    }

    #[test]
    fn a_root_signs_from_not_before_inclusive_until_not_after_exclusive() {
        let release = release();
        let anchor = two_root_anchor(&release, "anchor.json", &[]);
        let ci = signed_by_ci(&release);
        assert!(matches!(
            verify(&ci, &anchor, None, CI_WINDOW.0 - 1),
            Err(XtaskError::Catalog(CatalogError::SignerNotYetValid))
        ));
        verify(&ci, &anchor, None, CI_WINDOW.0).expect("not_before is inclusive");
        verify(&ci, &anchor, None, CI_WINDOW.1 - 1).expect("last second inside");
        assert!(matches!(
            verify(&ci, &anchor, None, CI_WINDOW.1),
            Err(XtaskError::Catalog(CatalogError::SignerExpired))
        ));
        let primary = signed(&release);
        verify(&primary, &anchor, None, CI_WINDOW.1).expect("primary window is its own");
    }

    #[test]
    fn revoking_the_ci_root_leaves_the_primary_root_trusted() {
        let release = release();
        let anchor = two_root_anchor(
            &release,
            "revoking.json",
            &[ci_key_id().as_str().to_owned()],
        );
        assert!(matches!(
            verify(&signed_by_ci(&release), &anchor, None, NOW),
            Err(XtaskError::Catalog(CatalogError::SignerRevoked))
        ));
        verify(&signed(&release), &anchor, None, NOW).expect("primary root still verifies");
    }

    #[test]
    fn the_anchor_bytes_do_not_depend_on_the_order_of_the_roots() {
        let release = release();
        let dir = release.dir.path();
        let (_private, primary) = key_files(dir);
        let (_ci_private, ci) = ci_key_files(dir);
        let a = RootSpec::new(&primary, 1, FAR_FUTURE);
        let b = RootSpec::new(&ci, CI_WINDOW.0, CI_WINDOW.1);
        assert_eq!(
            anchor_bytes(&[a.clone(), b.clone()], &[]).expect("bytes"),
            anchor_bytes(&[b, a], &[]).expect("bytes")
        );
    }

    #[test]
    fn a_refused_anchor_writes_no_file() {
        let release = release();
        let dir = release.dir.path();
        let (_private, primary) = key_files(dir);
        let (_ci_private, ci) = ci_key_files(dir);
        let copy = dir.join("primary-copy.key");
        fs::copy(&primary, &copy).expect("copy");
        let out = dir.join("refused-anchor.json");
        let root = |file: &Path| RootSpec::new(file, 1, FAR_FUTURE);

        let refusals: [(Vec<RootSpec>, Vec<String>, TrustAnchorError); 5] = [
            (Vec::new(), Vec::new(), TrustAnchorError::NoRoots),
            (
                vec![root(&primary), root(&copy)],
                Vec::new(),
                TrustAnchorError::DuplicateRoot,
            ),
            (
                vec![RootSpec::new(&primary, FAR_FUTURE, 1)],
                Vec::new(),
                TrustAnchorError::EmptyWindow,
            ),
            (
                vec![RootSpec::new(&primary, 5, 5)],
                Vec::new(),
                TrustAnchorError::EmptyWindow,
            ),
            (
                vec![root(&primary), root(&ci)],
                vec![key_id().as_str().to_owned(); MAX_REVOKED_KEYS + 1],
                TrustAnchorError::TooLarge,
            ),
        ];
        for (roots, revoked, expected) in refusals {
            let result = anchor_to(&roots, &revoked, &out);
            assert!(
                matches!(&result, Err(XtaskError::Anchor(AnchorFileError::Anchor(found))) if *found == expected),
                "expected {expected:?}, got {result:?}"
            );
            assert!(!out.exists(), "{expected:?} left a file behind");
        }

        let nine: Vec<RootSpec> = (0..=MAX_TRUST_ROOTS)
            .map(|index| {
                let seed = u8::try_from(index).expect("small index") + 100;
                let file = dir.join(format!("extra-{index}.key"));
                fs::write(
                    &file,
                    hex(&SigningKey::from_bytes(&[seed; 32])
                        .verifying_key()
                        .to_bytes()),
                )
                .expect("write");
                root(&file)
            })
            .collect();
        assert_eq!(nine.len(), MAX_TRUST_ROOTS + 1);
        assert!(matches!(
            anchor_to(&nine, &[], &out),
            Err(XtaskError::Anchor(AnchorFileError::Anchor(
                TrustAnchorError::TooLarge
            )))
        ));
        assert!(!out.exists());
        anchor_to(&nine[..MAX_TRUST_ROOTS], &[], &out).expect("eight roots are accepted");

        let missing = dir.join("missing.key");
        let other = dir.join("never-written.json");
        assert!(anchor_to(&[root(&primary), root(&missing)], &[], &other).is_err());
        assert!(anchor_to(&[root(&primary)], &["nothex".to_owned()], &other).is_err());
        assert!(!other.exists());
    }

    #[test]
    fn the_command_line_takes_repeated_roots_and_refuses_malformed_ones() {
        let release = release();
        let dir = release.dir.path();
        let (_private, primary) = key_files(dir);
        let (_ci_private, ci) = ci_key_files(dir);
        let out = dir.join("cli-anchor.json");
        let out_arg = out.to_string_lossy().into_owned();
        let run = |parts: &[String]| crate::run(parts.iter().cloned());
        let anchor_args = |roots: &[String]| -> Vec<String> {
            let mut args: Vec<String> = ["catalog", "anchor"].map(str::to_owned).into();
            for root in roots {
                args.push("--root".to_owned());
                args.push(root.clone());
            }
            args.extend(["--output".to_owned(), out_arg.clone()]);
            args
        };
        let primary_root = format!("{}:1:{FAR_FUTURE}", primary.to_string_lossy());
        let ci_root = format!("{}:{}:{}", ci.to_string_lossy(), CI_WINDOW.0, CI_WINDOW.1);

        run(&anchor_args(&[primary_root.clone(), ci_root.clone()])).expect("two roots");
        let forward = fs::read(&out).expect("read");
        fs::remove_file(&out).expect("remove");
        run(&anchor_args(&[ci_root.clone(), primary_root.clone()])).expect("reversed");
        assert_eq!(forward, fs::read(&out).expect("read"));
        fs::remove_file(&out).expect("remove");

        let malformed = [
            Vec::new(),
            vec![primary.to_string_lossy().into_owned()],
            vec![format!("{}:1", primary.to_string_lossy())],
            vec![format!("{}:x:2", primary.to_string_lossy())],
            vec![format!("{}:1:-2", primary.to_string_lossy())],
            vec![":1:2".to_owned()],
        ];
        for roots in malformed {
            let result = run(&anchor_args(&roots));
            assert!(matches!(result, Err(XtaskError::Usage(_))), "{roots:?}");
            assert!(!out.exists(), "{roots:?}");
        }
        let old_form = run(&[
            "catalog",
            "anchor",
            "--public-key-file",
            "k",
            "--not-before",
            "1",
            "--not-after",
            "2",
            "--output",
            &out_arg,
        ]
        .map(str::to_owned));
        assert!(matches!(old_form, Err(XtaskError::Usage(_))));
        assert!(!out.exists());
    }

    #[test]
    fn a_root_argument_splits_the_window_off_the_right_so_paths_may_hold_colons() {
        let spec: RootSpec = "/keys/a:b/public.key:7:9".parse().expect("parse");
        assert_eq!(spec, RootSpec::new(Path::new("/keys/a:b/public.key"), 7, 9));
    }

    #[test]
    fn public_key_reports_the_id_and_key_and_never_the_seed() {
        let release = release();
        let (private, _public) = key_files(release.dir.path());
        let (id, public) = public_key_of(&private).expect("public key");
        assert_eq!(id, key_id());
        assert_eq!(public, hex(&key().verifying_key().to_bytes()));
        assert!(!public.contains(&hex(&[SEED; 32])));
    }

    #[test]
    fn the_command_line_runs_the_whole_release_flow() {
        let release = release();
        let dir = release.dir.path();
        let (private, public) = key_files(dir);
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let args = |parts: &[&str]| -> Result<(), XtaskError> {
            crate::run(parts.iter().map(|part| (*part).to_owned()))
        };
        args(&[
            "catalog",
            "build",
            "--spec",
            &release.spec.to_string_lossy(),
            "--output",
            &path("unsigned.json"),
        ])
        .expect("build");
        args(&[
            "catalog",
            "anchor",
            "--root",
            &format!("{}:1:4102444800", public.to_string_lossy()),
            "--output",
            &path("anchor.json"),
        ])
        .expect("anchor");
        args(&[
            "catalog",
            "sign",
            "--catalog",
            &path("unsigned.json"),
            "--key-file",
            &private.to_string_lossy(),
            "--key-id",
            key_id().as_str(),
            "--output",
            &path("runtime-catalog.json"),
        ])
        .expect("sign");
        args(&[
            "catalog",
            "verify",
            "--catalog",
            &path("runtime-catalog.json"),
            "--anchor",
            &path("anchor.json"),
        ])
        .expect("verify");
        args(&[
            "catalog",
            "public-key",
            "--key-file",
            &private.to_string_lossy(),
        ])
        .expect("public key");
        assert!(args(&[
            "catalog",
            "sign",
            "--catalog",
            &path("unsigned.json"),
            "--key-file",
            &private.to_string_lossy(),
            "--key-id",
            &"0".repeat(64),
            "--output",
            &path("refused.json"),
        ])
        .is_err());
        assert!(!dir.join("refused.json").exists());
    }

    #[test]
    fn no_key_can_be_passed_as_an_argument_or_an_environment_variable() {
        for forbidden in ["--key", "--seed", "--private-key", "--key-hex"] {
            let result = crate::run(
                [
                    "catalog",
                    "sign",
                    "--catalog",
                    "c",
                    forbidden,
                    "x",
                    "--key-id",
                    "k",
                    "--output",
                    "o",
                ]
                .map(str::to_owned),
            );
            assert!(matches!(result, Err(XtaskError::Usage(_))), "{forbidden}");
        }
        let help = crate::run(["catalog", "sign", "--help"].map(str::to_owned));
        let Err(XtaskError::Usage(text)) = help else {
            panic!("help is reported as usage");
        };
        assert!(
            !text.contains("[env:"),
            "no argument reads the environment: {text}"
        );
    }
}

//! Runtime catalog assembly, signing and verification for release tooling.
//!
//! `catalog build` turns a package spec into the unsigned catalog body, with
//! each entry's digest, package id, runtime id and version read from the
//! package archive itself. `catalog sign` signs that body with a key file the
//! caller names by path (see [`crate::catalog_key`]) and verifies the result
//! before writing it, so a document the daemon would reject is never emitted.
//! `catalog verify` checks a catalog against an anchor file the way the daemon
//! does, and `catalog anchor` writes the anchor file a release ships.
//!
//! Every output is deterministic: entries are sorted, platforms are sorted,
//! documents are indented JSON with one trailing newline and Ed25519
//! signatures are deterministic.

// Rust guideline compliant 2026-10-05

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use nix::fcntl::OFlag;
use package::{
    catalog_document_bytes, parse_anchor, read_archive, sign_catalog, verify_catalog, AnchorFile,
    AnchorFileError, Catalog, CatalogEntry, KeyId, Limits, PackageDigest, RootKey,
    CATALOG_SCHEMA_VERSION, MAX_CATALOG_BYTES,
};
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde::Deserialize;

use crate::catalog_key::{read_public_key, read_signing_key};
use crate::XtaskError;

/// Schema version of the package spec read by `catalog build`.
const SPEC_SCHEMA_VERSION: u64 = 1;

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
    core: String,
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> XtaskError {
    let path = path.to_path_buf();
    move |source| XtaskError::Io { path, source }
}

/// Reads the regular file at `path`, at most `max_bytes`, without following a
/// final symbolic link.
fn read_input(path: &Path, max_bytes: u64) -> Result<Vec<u8>, XtaskError> {
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
fn write_output(output: &Path, bytes: &[u8]) -> Result<(), XtaskError> {
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

fn hex(bytes: &[u8]) -> String {
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

/// Package id, version and runtime id a package descriptor declares.
fn descriptor_identity(
    descriptor: &[u8],
) -> Result<(PackageId, PackageVersion, RuntimeId), XtaskError> {
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
    let runtime = table
        .get("runtime")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| invalid("has no `[runtime]` table"))?;
    let runtime_id = RuntimeId::parse(&string(runtime.get("id"), "runtime.id")?)
        .map_err(|_cause| invalid("has an invalid `runtime.id`"))?;
    Ok((package_id, version, runtime_id))
}

/// The catalog entry of the package archive at `archive`, listed for
/// `platforms` and the core range `core`.
fn entry_from_archive(
    archive: &Path,
    platforms: &[String],
    core: &str,
) -> Result<CatalogEntry, XtaskError> {
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
    let (package_id, version, runtime_id) = descriptor_identity(&descriptor.contents)?;
    let mut platforms = platforms.to_vec();
    platforms.sort();
    Ok(CatalogEntry {
        package_id,
        runtime_id,
        version,
        digest: verified.digest().clone(),
        platforms,
        core: core.to_owned(),
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
        entries.push(entry_from_archive(
            &archive,
            &package.platforms,
            &package.core,
        )?);
    }
    entries.sort_by(|left, right| {
        (left.package_id.as_str(), left.version.as_str())
            .cmp(&(right.package_id.as_str(), right.version.as_str()))
    });
    let mut revoked_key_ids = spec.revoked_key_ids;
    revoked_key_ids.sort();
    let mut revoked_digests = spec.revoked_digests;
    revoked_digests.sort();
    Ok(Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence: spec.sequence,
        expires_at: spec.expires_at,
        revoked_key_ids,
        revoked_digests,
        entries,
    })
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
    let anchor = parse_anchor(&anchor_bytes)
        .map_err(XtaskError::Anchor)?
        .trust_anchor()
        .map_err(|error| XtaskError::Anchor(AnchorFileError::Anchor(error)))?;
    let document = read_input(
        catalog_path,
        u64::try_from(MAX_CATALOG_BYTES).unwrap_or(u64::MAX),
    )?;
    let verified =
        verify_catalog(&document, &anchor, now, high_water).map_err(XtaskError::Catalog)?;
    let entries = verified
        .entries()
        .iter()
        .map(|entry| {
            format!(
                "{} {} {} {} [{}] {}",
                entry.package_id().as_str(),
                entry.version().as_str(),
                entry.runtime_id().as_str(),
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

/// The anchor file bytes for the public key at `public_key_file`.
pub(crate) fn anchor_bytes(
    public_key_file: &Path,
    not_before: u64,
    not_after: u64,
    revoked: &[String],
) -> Result<Vec<u8>, XtaskError> {
    let public_key = read_public_key(public_key_file)?;
    let root = RootKey::new(public_key, not_before, not_after)
        .map_err(|error| XtaskError::Anchor(AnchorFileError::Anchor(error)))?;
    let revoked = revoked
        .iter()
        .map(|id| {
            KeyId::parse(id).map_err(|_cause| {
                XtaskError::Usage("--revoked-key-id is not 64 lowercase hex characters".into())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let anchor = AnchorFile::new(vec![root], revoked)
        .map_err(|error| XtaskError::Anchor(AnchorFileError::Anchor(error)))?;
    anchor.to_bytes().map_err(XtaskError::Anchor)
}

/// Writes the anchor file for `public_key_file` to `output`.
pub(crate) fn anchor_to(
    public_key_file: &Path,
    not_before: u64,
    not_after: u64,
    revoked: &[String],
    output: &Path,
) -> Result<(), XtaskError> {
    write_output(
        output,
        &anchor_bytes(public_key_file, not_before, not_after, revoked)?,
    )
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

    use package::{build_archive, ArchiveEntry, CatalogEntryRejection, CatalogError};
    use serde_json::json;

    use super::*;

    const SEED: u8 = 11;
    const FAR_FUTURE: u64 = 4_102_444_800;
    const NOW: u64 = 1_800_000_000;
    const PLATFORM: &str = "x86_64-unknown-linux-gnu";

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
            "schema_version": 1,
            "sequence": 4,
            "expires_at": FAR_FUTURE,
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
        let spec = write_spec(
            dir.path(),
            &json!([{ "archive": "codex.tar.zst", "platforms": [PLATFORM], "core": ">=0.0.0" }]),
        );
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
        anchor_to(&public, not_before, not_after, &[], &out).expect("anchor");
        out
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
        assert_eq!(entry.platforms, [PLATFORM]);
        assert_eq!(entry.core, ">=0.0.0");
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
                { "archive": "b.tar.zst", "platforms": ["b-platform", "a-platform"], "core": ">=0.0.0" },
                { "archive": "a2.tar.zst", "platforms": [PLATFORM], "core": ">=0.0.0" },
                { "archive": "a1.tar.zst", "platforms": [PLATFORM], "core": ">=0.0.0" },
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
    }

    #[test]
    fn build_refuses_archives_it_cannot_read_an_identity_from() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let spec_for = |name: &str| {
            write_spec(
                dir.path(),
                &json!([{ "archive": name, "platforms": [PLATFORM], "core": ">=0.0.0" }]),
            )
        };

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
    fn build_refuses_unknown_spec_members_and_versions_and_links() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let spec = dir.path().join("spec.json");
        let base = json!({
            "schema_version": 1, "sequence": 1, "expires_at": FAR_FUTURE,
            "revoked_key_ids": [], "revoked_digests": [], "packages": []
        });
        let mut extra = base.clone();
        extra["extra"] = json!(true);
        fs::write(&spec, serde_json::to_vec(&extra).expect("json")).expect("write");
        assert!(matches!(build(&spec), Err(XtaskError::Json(_))));
        let mut version = base.clone();
        version["schema_version"] = json!(2);
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

        let dir = release.dir.path();
        write_archive(
            dir,
            "shell.tar.zst",
            &descriptor("acme.runtime.shell", "1.0.0", "shell"),
        );
        let spec = write_spec(
            dir,
            &json!([{ "archive": "shell.tar.zst", "platforms": [PLATFORM], "core": ">=0.0.0" }]),
        );
        let unsigned = dir.join("shell-unsigned.json");
        build_to(&spec, &unsigned).expect("build lists whatever the archive says");
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
        anchor_to(&other, 1, FAR_FUTURE, &[], &other_anchor).expect("anchor");
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
            &public,
            1,
            FAR_FUTURE,
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
        anchor_to(&public, 1, FAR_FUTURE, &[], &second).expect("anchor");
        assert_eq!(
            fs::read(&first).expect("read"),
            fs::read(&second).expect("read")
        );
        let parsed = parse_anchor(&fs::read(&first).expect("read")).expect("parse");
        assert_eq!(parsed.roots()[0].id(), &key_id());

        assert!(
            anchor_to(&public, FAR_FUTURE, 1, &[], &second).is_err(),
            "empty window"
        );
        assert!(anchor_to(&public, 1, 2, &["nothex".to_owned()], &second).is_err());
        let bad_public = release.dir.path().join("bad.key");
        fs::write(&bad_public, "not a key").expect("write");
        assert!(anchor_to(&bad_public, 1, 2, &[], &second).is_err());
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
            "--public-key-file",
            &public.to_string_lossy(),
            "--not-before",
            "1",
            "--not-after",
            "4102444800",
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

//! Names shared by metadata writers and the service purge.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;

/// Data-root child holding pending and imported legacy manifests.
pub const MIGRATIONS_SUBDIR: &str = "migrations";
/// Pending legacy manifest; purge preserves it.
pub const LEGACY_MANIFEST_NAME: &str = "durable-session-workers.json";
const STORE_TEMP_INFIX: &str = ".tmp.";
const ARCHIVE_PREFIX: &str = "durable-session-workers.imported-";
const ARCHIVE_SUFFIX: &str = ".json";
/// Historical archive names retain eight bytes of each SHA-256 digest.
const ARCHIVE_DIGEST_BYTES: usize = 8;
const ARCHIVE_HEX_CHARS: usize = ARCHIVE_DIGEST_BYTES * 2;

/// Name a metadata-store copy carries before its atomic commit.
#[must_use]
pub fn metadata_store_temp_name(store: &OsStr, process: u32, sequence: u64) -> OsString {
    let mut name = store.to_os_string();
    name.push(format!("{STORE_TEMP_INFIX}{process}.{sequence}"));
    name
}

/// Whether `name` is a canonical PID/sequence temporary copy of `store`.
#[must_use]
pub fn is_metadata_store_temp(store: &str, name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(store)
        .and_then(|tail| tail.strip_prefix(STORE_TEMP_INFIX))
    else {
        return false;
    };
    let Some((process, sequence)) = rest.split_once('.') else {
        return false;
    };
    process
        .parse::<u32>()
        .is_ok_and(|value| value.to_string() == process)
        && sequence
            .parse::<u64>()
            .is_ok_and(|value| value.to_string() == sequence)
}

/// Historical imported-manifest name, including already-migrated imports.
///
/// Already-migrated stores archive before validating the fingerprint, so its
/// filtered prefix can contain fewer than sixteen hexadecimal characters.
#[must_use]
pub fn legacy_manifest_archive_name(fingerprint: &str, created_digest: &[u8; 32]) -> String {
    let fingerprint: String = fingerprint
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(ARCHIVE_HEX_CHARS)
        .collect();
    let mut created = String::with_capacity(ARCHIVE_HEX_CHARS);
    for byte in &created_digest[..ARCHIVE_DIGEST_BYTES] {
        let _ = write!(created, "{byte:02x}");
    }
    format!("{ARCHIVE_PREFIX}{fingerprint}-{created}{ARCHIVE_SUFFIX}")
}

/// Whether `name` belongs to the legacy archive producer's exact name grammar.
#[must_use]
pub fn is_imported_manifest_archive(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(ARCHIVE_PREFIX)
        .and_then(|name| name.strip_suffix(ARCHIVE_SUFFIX))
    else {
        return false;
    };
    let Some((fingerprint, created)) = rest.split_once('-') else {
        return false;
    };
    fingerprint.len() <= ARCHIVE_HEX_CHARS
        && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
        && created.len() == ARCHIVE_HEX_CHARS
        && created
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

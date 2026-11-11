//! Install by package id: fetch the plugin index and the archive it lists,
//! select a version and stage both files for the daemon.
//!
//! The transport carries no trust. The index parsed here is untrusted input
//! used to pick an entry and to explain failures; the daemon verifies its
//! signature, revocations, freshness and the archive digest again before it
//! installs anything.

// Rust guideline compliant 2026-11-06

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use package::{
    archive_digest, host_platform, parse_index_envelope, IndexEntry, IndexEntryKind,
    MAX_COMPRESSED_BYTES, MAX_INDEX_BYTES, OFFICIAL_CONTEXT_PACKAGE_PREFIX,
};
use protocol::{PackageDigest, PackageId, PackageVersion};
use reqwest::redirect::{Attempt, Policy};
use reqwest::{Client, Response, Url};
use semver::Version;
use tempfile::TempDir;

use crate::commands::plugin::Error;

/// The official plugin index, schema 1 (runtime entries only): the newest
/// release of the index repository.
pub(crate) const OFFICIAL_INDEX_URL: &str = "https://github.com/pohunekd/pohunek-plugin-index/releases/latest/download/official-index-v1.json";

/// The official plugin index, schema 2 (runtime and context entries), which
/// the index repository publishes beside the schema 1 document.
pub(crate) const OFFICIAL_INDEX_URL_V2: &str = "https://github.com/pohunekd/pohunek-plugin-index/releases/latest/download/official-index-v2.json";

/// Core version an entry's `core` range is matched against: this build's.
const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Longest wait for a connection to be established.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest time one download may take from request to the last body byte.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Redirect hops followed per download; the same limit reqwest's default
/// policy applies.
const MAX_REDIRECTS: usize = 10;

/// Hosts that may be reached over plain `http`: this machine, which carries
/// the test servers and nothing else.
const LOOPBACK_HOSTS: [&str; 2] = ["127.0.0.1", "[::1]"];

/// Prefix of the staging directory name.
const STAGING_PREFIX: &str = "pohunek-plugin-";

/// File name of the staged index document.
const STAGED_INDEX: &str = "index.json";

/// File name of the staged archive.
const STAGED_ARCHIVE: &str = "archive.tar.zst";

/// Mode of a staged file: readable by the owner only.
const STAGED_FILE_MODE: u32 = 0o600;

/// A source argument that names a package: `<package-id>[@<version>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageRef {
    /// The requested package.
    pub(crate) id: PackageId,
    /// The exact version, when the argument pinned one.
    pub(crate) version: Option<PackageVersion>,
}

impl PackageRef {
    /// Parse `text`, splitting at the first `@`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PackageRefInvalid`] when the id or the version does
    /// not parse.
    pub(crate) fn parse(text: &str) -> Result<Self, Error> {
        let invalid = |detail: String| Error::PackageRefInvalid {
            source_text: text.to_owned(),
            detail,
        };
        let (id_text, version_text) = match text.split_once('@') {
            Some((id, version)) => (id, Some(version)),
            None => (text, None),
        };
        let id = PackageId::parse(id_text).map_err(|error| invalid(error.to_string()))?;
        let version = version_text
            .map(PackageVersion::parse)
            .transpose()
            .map_err(|error| invalid(error.to_string()))?;
        Ok(Self { id, version })
    }
}

/// The index entry a by-id install resolved to.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    /// The resolved package id.
    pub(crate) id: PackageId,
    /// The resolved version.
    pub(crate) version: PackageVersion,
    /// The digest the index lists for the archive.
    pub(crate) digest: PackageDigest,
}

impl Resolved {
    /// `<id>@<version>`, the form that pins the reviewed version.
    pub(crate) fn pinned(&self) -> String {
        format!("{}@{}", self.id, self.version)
    }
}

/// The index and archive of a by-id install, staged in a private directory
/// that is removed when this value drops.
#[derive(Debug)]
pub(crate) struct Staged {
    /// Held so the directory outlives the install; dropping it removes both files.
    _dir: TempDir,
    /// What the index resolved the request to.
    pub(crate) resolved: Resolved,
    /// Canonical absolute path of the staged archive.
    pub(crate) archive_path: String,
    /// Canonical absolute path of the staged index document.
    pub(crate) index_path: String,
}

/// Whether `url` may be requested: `https`, or `http` to this machine, and no
/// embedded credentials.
pub(crate) fn url_allowed(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => url.host_str().is_some(),
        "http" => url
            .host_str()
            .is_some_and(|host| LOOPBACK_HOSTS.contains(&host)),
        _ => false,
    }
}

/// Parse `text` as a URL that [`url_allowed`] accepts.
fn checked_url(text: &str) -> Result<Url, Error> {
    Url::parse(text)
        .ok()
        .filter(url_allowed)
        .ok_or_else(|| Error::IndexUrlRefused {
            url: redacted(text),
        })
}

/// `text` limited to what an error message may show: scheme and host only.
fn redacted(text: &str) -> String {
    Url::parse(text)
        .ok()
        .and_then(|url| {
            url.host_str()
                .map(|host| format!("{}://{host}/…", url.scheme()))
        })
        .unwrap_or_else(|| "an unparsable URL".to_owned())
}

/// Refused redirect target, carried through reqwest's error chain.
#[derive(Debug, thiserror::Error)]
#[error("redirect to a refused URL")]
struct RefusedRedirect(String);

/// Redirect policy that checks every hop before it is requested.
fn redirect_policy() -> Policy {
    Policy::custom(|attempt: Attempt<'_>| {
        if !url_allowed(attempt.url()) {
            let shown = redacted(attempt.url().as_str());
            return attempt.error(RefusedRedirect(shown));
        }
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        attempt.follow()
    })
}

fn client() -> Result<Client, Error> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .redirect(redirect_policy())
        .build()
        .map_err(|error| Error::IndexUnavailable {
            detail: format!("the HTTP client cannot start: {error}"),
        })
}

/// What a download is, for the error codes it can fail with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Download {
    Index,
    Archive,
}

impl Download {
    fn limit(self) -> u64 {
        match self {
            Self::Index => u64::try_from(MAX_INDEX_BYTES).unwrap_or(u64::MAX),
            Self::Archive => MAX_COMPRESSED_BYTES,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Archive => "archive",
        }
    }

    fn unavailable(self, detail: String) -> Error {
        match self {
            Self::Index => Error::IndexUnavailable { detail },
            Self::Archive => Error::ArchiveUnavailable { detail },
        }
    }
}

/// Fetch `url` with a hard cap on the body.
async fn fetch(client: &Client, url: &Url, kind: Download) -> Result<Vec<u8>, Error> {
    let mut response = client.get(url.clone()).send().await.map_err(|error| {
        if let Some(target) = refused_redirect(&error) {
            return Error::IndexUrlRefused { url: target };
        }
        kind.unavailable(transport_detail(&error))
    })?;
    if !response.status().is_success() {
        return Err(kind.unavailable(format!("the server answered {}", response.status())));
    }
    let limit = kind.limit();
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(too_large(kind));
    }
    read_capped(&mut response, kind, limit).await
}

async fn read_capped(
    response: &mut Response,
    kind: Download,
    limit: u64,
) -> Result<Vec<u8>, Error> {
    let mut body = Vec::new();
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|error| kind.unavailable(transport_detail(&error)))?;
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        let total = u64::try_from(body.len())
            .unwrap_or(u64::MAX)
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        if total > limit {
            return Err(too_large(kind));
        }
        body.extend_from_slice(&chunk);
    }
}

fn too_large(kind: Download) -> Error {
    Error::DownloadTooLarge {
        what: kind.name(),
        limit: kind.limit(),
    }
}

/// The redacted target of a redirect the policy refused, when `error` is one.
fn refused_redirect(error: &reqwest::Error) -> Option<String> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if let Some(refused) = current.downcast_ref::<RefusedRedirect>() {
            return Some(refused.0.clone());
        }
        source = current.source();
    }
    None
}

/// A transport failure described without the URL it concerned.
fn transport_detail(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "the request timed out".to_owned()
    } else if error.is_connect() {
        "the server cannot be reached".to_owned()
    } else if error.is_redirect() {
        "too many redirects".to_owned()
    } else {
        "the transfer failed".to_owned()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The kind of entry `request` can resolve to: ids under the context prefix
/// resolve to `context` entries only, every other id to `runtime` entries.
fn wanted_kind(request: &PackageRef) -> IndexEntryKind {
    if request
        .id
        .as_str()
        .starts_with(OFFICIAL_CONTEXT_PACKAGE_PREFIX)
    {
        IndexEntryKind::Context
    } else {
        IndexEntryKind::Runtime
    }
}

/// The entry `request` resolves to among `entries`, for `core` on `platform`.
///
/// With a pinned version the exact entry is chosen; otherwise the highest
/// semantic version. An entry whose version is not semantic is never chosen
/// by the unpinned form.
fn select<'a>(
    entries: &'a [IndexEntry],
    request: &PackageRef,
    core: &Version,
    platform: &str,
) -> Result<&'a IndexEntry, Error> {
    let wanted = wanted_kind(request);
    let candidates: Vec<&IndexEntry> = entries
        .iter()
        .filter(|entry| {
            entry.package_id == request.id
                && entry.kind() == wanted
                && entry.supports(core, platform)
        })
        .collect();
    if candidates.is_empty() {
        return Err(Error::IndexPackageNotFound {
            package: request.id.to_string(),
        });
    }
    if let Some(version) = &request.version {
        return candidates
            .into_iter()
            .find(|entry| &entry.version == version)
            .ok_or_else(|| Error::IndexVersionNotFound {
                package: request.id.to_string(),
                version: version.to_string(),
            });
    }
    candidates
        .into_iter()
        .filter_map(|entry| {
            Version::parse(entry.version.as_str())
                .ok()
                .map(|version| (version, entry))
        })
        .max_by(|(left, _), (right, _)| left.cmp_precedence(right))
        .map(|(_, entry)| entry)
        .ok_or_else(|| Error::IndexPackageNotFound {
            package: request.id.to_string(),
        })
}

/// Fetches the official index document for entries of `kind`.
///
/// A `runtime` request reads the schema 1 document, which every daemon
/// accepts; a `context` request reads the schema 2 document, the only one that
/// lists a context package. The index repository builds both documents at the
/// same sequence, so the daemon's index high-water mark, which the two share,
/// never rejects one because the other was read first.
async fn fetch_official_index(client: &Client, kind: IndexEntryKind) -> Result<Vec<u8>, Error> {
    let url = match kind {
        IndexEntryKind::Runtime => OFFICIAL_INDEX_URL,
        IndexEntryKind::Context => OFFICIAL_INDEX_URL_V2,
    };
    fetch(client, &checked_url(url)?, Download::Index).await
}

/// The index document and the entry `request` resolved to.
struct Selection {
    index_bytes: Vec<u8>,
    entry: IndexEntry,
}

/// Fetch the index at `index_url` (the official one when `None`) and select
/// the entry `request` resolves to for this core version and platform.
async fn select_entry(
    client: &Client,
    request: &PackageRef,
    index_url: Option<&str>,
) -> Result<Selection, Error> {
    let core = Version::parse(CORE_VERSION).map_err(|_error| Error::IndexInvalid {
        detail: "this build has no semantic version to match entries against".to_owned(),
    })?;
    let platform = host_platform().ok_or(Error::PlatformUnsupported)?;
    let index_bytes = match index_url {
        Some(url) => fetch(client, &checked_url(url)?, Download::Index).await?,
        None => fetch_official_index(client, wanted_kind(request)).await?,
    };
    let envelope = parse_index_envelope(&index_bytes).map_err(|error| Error::IndexInvalid {
        detail: error.to_string(),
    })?;
    if unix_now() >= envelope.index.expires_at {
        return Err(Error::IndexExpired);
    }
    let entry = select(&envelope.index.entries, request, &core, &platform)?.clone();
    Ok(Selection { index_bytes, entry })
}

impl Selection {
    fn resolved(&self) -> Resolved {
        Resolved {
            id: self.entry.package_id.clone(),
            version: self.entry.version.clone(),
            digest: self.entry.digest.clone(),
        }
    }
}

/// Fetch the index at `index_url` (the official one when `None`) and resolve
/// `request` without downloading the archive.
///
/// The result is what the unverified index claims; the daemon verifies the
/// signature, revocations, freshness and digest when the package installs.
///
/// # Errors
///
/// Returns the first failed step as an [`Error`].
pub(crate) async fn resolve(
    request: &PackageRef,
    index_url: Option<&str>,
) -> Result<Resolved, Error> {
    let client = client()?;
    Ok(select_entry(&client, request, index_url).await?.resolved())
}

/// Fetch the index at `index_url` (the official one when `None`), resolve
/// `request`, download the archive and stage both files.
///
/// # Errors
///
/// Returns the first failed step as an [`Error`]; nothing is sent to the
/// daemon by this function.
pub(crate) async fn stage(request: &PackageRef, index_url: Option<&str>) -> Result<Staged, Error> {
    let client = client()?;
    let selection = select_entry(&client, request, index_url).await?;
    let resolved = selection.resolved();
    let Selection { index_bytes, entry } = selection;
    let archive_url = checked_url(&entry.url)?;
    let archive = fetch(&client, &archive_url, Download::Archive).await?;
    let actual = archive_digest(&archive).map_err(|_error| Error::ArchiveDigestMismatch {
        expected: resolved.digest.as_str().to_owned(),
        actual: "unreadable".to_owned(),
    })?;
    if actual != resolved.digest {
        return Err(Error::ArchiveDigestMismatch {
            expected: resolved.digest.as_str().to_owned(),
            actual: actual.as_str().to_owned(),
        });
    }
    let dir = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir()
        .map_err(|error| staging_failed(&error))?;
    let canonical = std::fs::canonicalize(dir.path()).map_err(|error| staging_failed(&error))?;
    let index_path = write_staged(&canonical, STAGED_INDEX, &index_bytes)?;
    let archive_path = write_staged(&canonical, STAGED_ARCHIVE, &archive)?;
    Ok(Staged {
        _dir: dir,
        resolved,
        archive_path,
        index_path,
    })
}

fn staging_failed(error: &std::io::Error) -> Error {
    Error::StagingFailed {
        detail: error.kind().to_string(),
    }
}

/// Write `bytes` to a new owner-only file `name` in `dir` and return its path.
fn write_staged(dir: &std::path::Path, name: &str, bytes: &[u8]) -> Result<String, Error> {
    let path: PathBuf = dir.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(STAGED_FILE_MODE)
        .open(&path)
        .map_err(|error| staging_failed(&error))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| staging_failed(&error))?;
    path.into_os_string()
        .into_string()
        .map_err(|_path| Error::StagingFailed {
            detail: "the staging path is not UTF-8".to_owned(),
        })
}

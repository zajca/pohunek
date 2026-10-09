//! Safe extraction and verification of release archives for the assembler.
//!
//! An archive is unpacked into a directory this module creates itself:
//! only regular files and directories are accepted, every member lies below
//! one top directory with a fixed name, no member path leaves it, no path
//! repeats, and the member count and the decompressed sizes are bounded.
//! The archive's `MANIFEST` is then checked against the extracted bytes, so
//! the tree the assembler repacks is exactly what the producer sealed.

// Rust guideline compliant 2026-10-08

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use nix::fcntl::OFlag;
use sha2::{Digest as _, Sha256};

use crate::catalog::{hex, io_error};
use crate::release::{refuse, Fault, InputClass};
use crate::XtaskError;

/// Name of the manifest at the top of a release archive tree.
pub(crate) const MANIFEST_FILE: &str = "MANIFEST";

/// First line of a supported manifest.
const MANIFEST_HEADER: &str = "pohunek-archive-manifest 1";

/// Largest number of members one archive may hold.
///
/// A release archive holds the binaries and the rendered documentation, a few
/// hundred files. The bound only stops an archive that declares millions of
/// members; lowering it below the documentation size makes real archives fail.
const MAX_MEMBERS: usize = 20_000;

/// Largest size of one extracted member: the executable size limit shared
/// with the attestation (the release binaries are tens of MiB).
const MAX_MEMBER_BYTES: u64 = 1024 * 1024 * 1024;

/// Largest total size of one extracted archive, so a compression bomb is
/// refused after at most this much is written.
const MAX_EXTRACTED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Largest accepted manifest: one 100-byte line per member of a full-size
/// archive.
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

/// Largest input file that is hashed whole (a compressed release archive).
pub(crate) const MAX_HASHED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Read buffer size for hashing.
const HASH_CHUNK_BYTES: usize = 64 * 1024;

/// Mode of an extracted directory.
const DIR_MODE: u32 = 0o755;

/// Mode of an extracted file the archive marks executable.
const EXECUTABLE_MODE: u32 = 0o755;

/// Mode of every other extracted file.
const FILE_MODE: u32 = 0o644;

/// Mode a file has while its bytes are being written: owner-only.
const WRITING_MODE: u32 = 0o600;

/// Execute bits of a tar header mode; any of them marks the file executable.
const EXECUTE_BITS: u32 = 0o111;

/// Characters a manifest path may use, matching `packaging/write-manifest`.
fn is_manifest_path_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._+@=,/-".contains(&byte)
}

/// Opens the regular file at `path` without following a final symbolic link.
pub(crate) fn open_regular(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).bits())
        .open(path)?;
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ))
    }
}

/// Lowercase hex SHA-256 of the regular file at `path`.
///
/// The file is refused as `class` named `name` when it is not a regular file
/// or is larger than [`MAX_HASHED_BYTES`].
pub(crate) fn sha256_file(
    path: &Path,
    class: InputClass,
    name: &str,
) -> Result<String, XtaskError> {
    let mut file = open_regular(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => refuse(class, Fault::Missing, name),
        io::ErrorKind::InvalidInput => refuse(class, Fault::NotRegularFile, name),
        _ if error.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32) => {
            refuse(class, Fault::NotRegularFile, name)
        }
        _ => XtaskError::Io {
            path: path.to_path_buf(),
            source: error,
        },
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        let read = file.read(&mut buffer).map_err(io_error(path))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_HASHED_BYTES {
            return Err(refuse(class, Fault::TooLarge, name));
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// A reader that stops with an error once more than `remaining` bytes were
/// read, and records that it did so.
struct Bounded<'a, R> {
    inner: R,
    remaining: u64,
    exceeded: &'a Cell<bool>,
}

impl<R: Read> Read for Bounded<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        let read_bytes = read as u64;
        if read_bytes > self.remaining {
            self.exceeded.set(true);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decompressed size limit exceeded",
            ));
        }
        self.remaining -= read_bytes;
        Ok(read)
    }
}

/// The path of a member below the top directory, or a refusal.
///
/// The member must be the top directory itself (an empty result) or lie
/// below it; every component must be a plain name.
fn member_path(path: &Path, top: &str, archive: &str) -> Result<PathBuf, XtaskError> {
    let unsafe_path = || refuse(InputClass::Archive, Fault::UnsafePath, archive);
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == top => {}
        _ => return Err(unsafe_path()),
    }
    let mut relative = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => relative.push(part),
            _ => return Err(unsafe_path()),
        }
    }
    Ok(relative)
}

/// Unpacks the gzip-compressed tar archive at `archive` into `dest` and
/// returns the extracted top directory `dest/<top>`.
///
/// `name` is the archive's file name in refusals. Nothing is created outside
/// `dest`: no member is a link, so no later member can be redirected, and
/// every file is created exclusively.
pub(crate) fn extract_archive(
    archive: &Path,
    name: &str,
    top: &str,
    dest: &Path,
) -> Result<PathBuf, XtaskError> {
    let file = open_regular(archive).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            refuse(InputClass::Archive, Fault::Missing, name)
        } else {
            refuse(InputClass::Archive, Fault::NotRegularFile, name)
        }
    })?;
    let exceeded = Cell::new(false);
    let reader = Bounded {
        inner: GzDecoder::new(file),
        remaining: MAX_EXTRACTED_BYTES,
        exceeded: &exceeded,
    };
    let corrupt = |_cause: io::Error| {
        let fault = if exceeded.get() {
            Fault::TooLarge
        } else {
            Fault::Corrupt
        };
        refuse(InputClass::Archive, fault, name)
    };
    let mut tar = tar::Archive::new(reader);
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut members = 0_usize;
    let mut total: u64 = 0;
    let top_dir = dest.join(top);
    for entry in tar.entries().map_err(corrupt)? {
        let mut entry = entry.map_err(corrupt)?;
        members += 1;
        if members > MAX_MEMBERS {
            return Err(refuse(InputClass::Archive, Fault::TooManyMembers, name));
        }
        let kind = entry.header().entry_type();
        let path = entry.path().map_err(corrupt)?.into_owned();
        let relative = member_path(&path, top, name)?;
        if !seen.insert(relative.clone()) {
            return Err(refuse(InputClass::Archive, Fault::Duplicate, name));
        }
        let target = top_dir.join(&relative);
        if kind.is_dir() {
            DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(&target)
                .map_err(io_error(&target))?;
        } else if kind.is_file() && !relative.as_os_str().is_empty() {
            let size = entry.size();
            total = total.saturating_add(size);
            if size > MAX_MEMBER_BYTES || total > MAX_EXTRACTED_BYTES {
                return Err(refuse(InputClass::Archive, Fault::TooLarge, name));
            }
            let executable = entry.header().mode().map_err(corrupt)? & EXECUTE_BITS != 0;
            if let Some(parent) = target.parent() {
                DirBuilder::new()
                    .recursive(true)
                    .mode(DIR_MODE)
                    .create(parent)
                    .map_err(io_error(parent))?;
            }
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(WRITING_MODE)
                .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).bits())
                .open(&target)
                .map_err(io_error(&target))?;
            let written = io::copy(&mut entry, &mut out).map_err(|error| {
                if exceeded.get() {
                    refuse(InputClass::Archive, Fault::TooLarge, name)
                } else if error.kind() == io::ErrorKind::InvalidData
                    || error.kind() == io::ErrorKind::UnexpectedEof
                {
                    refuse(InputClass::Archive, Fault::Corrupt, name)
                } else {
                    XtaskError::Io {
                        path: target.clone(),
                        source: error,
                    }
                }
            })?;
            if written != size {
                return Err(refuse(InputClass::Archive, Fault::Corrupt, name));
            }
            drop(out);
            let mode = if executable {
                EXECUTABLE_MODE
            } else {
                FILE_MODE
            };
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))
                .map_err(io_error(&target))?;
        } else {
            return Err(refuse(InputClass::Archive, Fault::UnsupportedMember, name));
        }
    }
    if !top_dir.is_dir() {
        return Err(refuse(InputClass::Archive, Fault::Missing, name));
    }
    Ok(top_dir)
}

/// The parsed `MANIFEST` of a release archive tree.
#[derive(Debug)]
pub(crate) struct Manifest {
    pub(crate) component: String,
    pub(crate) version: String,
    pub(crate) target: String,
    pub(crate) signing: String,
    pub(crate) minimum_macos: Option<String>,
    /// Lowercase hex SHA-256 by member path.
    files: BTreeMap<String, String>,
}

/// Reads and parses `top/MANIFEST` with the grammar of
/// `packaging/write-manifest`.
pub(crate) fn read_manifest(top: &Path, name: &str) -> Result<Manifest, XtaskError> {
    let path = top.join(MANIFEST_FILE);
    let malformed = || refuse(InputClass::Manifest, Fault::Malformed, name);
    let mut file = open_regular(&path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            refuse(InputClass::Manifest, Fault::Missing, name)
        } else {
            refuse(InputClass::Manifest, Fault::NotRegularFile, name)
        }
    })?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error(&path))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(refuse(InputClass::Manifest, Fault::TooLarge, name));
    }
    let text = String::from_utf8(bytes).map_err(|_cause| malformed())?;
    let mut lines = text.lines();
    if lines.next() != Some(MANIFEST_HEADER) {
        return Err(malformed());
    }
    let mut component = None;
    let mut version = None;
    let mut target = None;
    let mut signing = None;
    let mut minimum_macos = None;
    let mut files = BTreeMap::new();
    let set_once = |slot: &mut Option<String>, value: &str| {
        if slot.replace(value.to_owned()).is_some() || value.is_empty() {
            Err(malformed())
        } else {
            Ok(())
        }
    };
    for line in lines {
        if let Some(value) = line.strip_prefix("component ") {
            set_once(&mut component, value)?;
        } else if let Some(value) = line.strip_prefix("version ") {
            set_once(&mut version, value)?;
        } else if let Some(value) = line.strip_prefix("target ") {
            set_once(&mut target, value)?;
        } else if let Some(value) = line.strip_prefix("signing ") {
            set_once(&mut signing, value)?;
        } else if let Some(value) = line.strip_prefix("minimum-macos ") {
            set_once(&mut minimum_macos, value)?;
        } else if let Some(entry) = line.strip_prefix("sha256 ") {
            let (digest, member) = entry.split_once(' ').ok_or_else(malformed)?;
            let digest_ok = digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
            if !digest_ok
                || member.is_empty()
                || !member.bytes().all(is_manifest_path_char)
                || files.insert(member.to_owned(), digest.to_owned()).is_some()
            {
                return Err(malformed());
            }
        } else {
            return Err(malformed());
        }
    }
    let (Some(component), Some(version), Some(target), Some(signing)) =
        (component, version, target, signing)
    else {
        return Err(malformed());
    };
    Ok(Manifest {
        component,
        version,
        target,
        signing,
        minimum_macos,
        files,
    })
}

/// Every regular file below `top` as `/`-joined relative paths, without the
/// top-level manifest. Anything but regular files and directories is refused.
fn tree_files(top: &Path, name: &str) -> Result<BTreeSet<String>, XtaskError> {
    let mut files = BTreeSet::new();
    let mut pending = vec![(top.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        for entry in fs::read_dir(&dir).map_err(io_error(&dir))? {
            let entry = entry.map_err(io_error(&dir))?;
            let file_name = entry
                .file_name()
                .into_string()
                .map_err(|_cause| refuse(InputClass::Archive, Fault::UnsafePath, name))?;
            let relative = format!("{prefix}{file_name}");
            let kind = entry.file_type().map_err(io_error(&entry.path()))?;
            if kind.is_dir() {
                pending.push((entry.path(), format!("{relative}/")));
            } else if kind.is_file() {
                if relative != MANIFEST_FILE {
                    files.insert(relative);
                }
            } else {
                return Err(refuse(InputClass::Archive, Fault::UnsupportedMember, name));
            }
        }
    }
    Ok(files)
}

/// Requires the files below `top` to be exactly the manifest's members, each
/// with the digest the manifest states.
pub(crate) fn verify_tree(top: &Path, manifest: &Manifest, name: &str) -> Result<(), XtaskError> {
    let present = tree_files(top, name)?;
    for member in manifest.files.keys() {
        if !present.contains(member) {
            return Err(refuse(InputClass::Manifest, Fault::Missing, member));
        }
    }
    for member in &present {
        if !manifest.files.contains_key(member) {
            return Err(refuse(InputClass::Manifest, Fault::Unexpected, member));
        }
    }
    for (member, digest) in &manifest.files {
        if &sha256_file(&top.join(member), InputClass::Manifest, member)? != digest {
            return Err(refuse(
                InputClass::Manifest,
                Fault::ChecksumMismatch,
                member,
            ));
        }
    }
    Ok(())
}

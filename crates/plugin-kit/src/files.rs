//! Capped, symlink-safe file helpers shared by every kit command.
//!
//! Moved from `xtask::catalog` unchanged: no-follow reads with a byte cap, an
//! exclusive document create that refuses symbolic links, and the canonical
//! JSON document bytes.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use nix::fcntl::OFlag;

use crate::error::Error;

/// Mode of a written document: readable by the daemon, as an installed
/// release file would be.
const DOCUMENT_MODE: u32 = 0o644;

/// Maps an I/O failure at `path` to [`Error::Io`].
pub fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::Io { path, source }
}

/// Reads the regular file at `path`, at most `max_bytes`, without following a
/// final symbolic link.
pub fn read_input(path: &Path, max_bytes: u64) -> Result<Vec<u8>, Error> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).bits())
        .open(path)
        .map_err(io_error(path))?;
    let metadata = file.metadata().map_err(io_error(path))?;
    if !metadata.is_file() {
        return Err(Error::UnsupportedFileType(path.to_path_buf()));
    }
    if metadata.len() > max_bytes {
        return Err(Error::Usage(format!(
            "`{}` is larger than the {max_bytes}-byte limit",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error(path))?;
    if u64::try_from(bytes.len()).map_or(true, |read| read > max_bytes) {
        return Err(Error::Usage(format!(
            "`{}` is larger than the {max_bytes}-byte limit",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Creates `path` exclusively and writes `bytes` with [`DOCUMENT_MODE`].
///
/// An existing path or a symbolic link is refused and left unchanged; a
/// failed write removes the file this call created.
pub fn write_new_document(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut file: File = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(DOCUMENT_MODE)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).bits())
        .open(path)
        .map_err(io_error(path))?;
    // The create mode is masked by the umask; the document is public, so the
    // mode is set explicitly.
    let written = file
        .set_permissions(std::fs::Permissions::from_mode(DOCUMENT_MODE))
        .and_then(|()| file.write_all(bytes))
        .and_then(|()| file.sync_all());
    if let Err(source) = written {
        // The file is ours: it did not exist before the exclusive create.
        let _ = std::fs::remove_file(path);
        return Err(Error::Io {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

/// The canonical JSON bytes of `value`: pretty with a trailing newline.
pub fn document_bytes<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Lowercase hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The current Unix time in seconds.
pub fn unix_now() -> Result<u64, Error> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_cause| Error::Usage("the system clock is before the Unix epoch".into()))
}

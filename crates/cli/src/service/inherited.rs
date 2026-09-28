//! The transaction lock a `pohunek service lock` ancestor hands down.
//!
//! `pohunek service lock -- <command>` holds the transaction lock and runs
//! `<command>` with a descriptor of the lock inherited, naming its number in
//! [`LOCK_FD_ENV`]. Every `pohunek` process captures that descriptor once, at
//! the start of `main` ([`capture`]), before it has a thread or a descriptor
//! of its own, marks it close-on-exec, and removes the variable from its
//! environment, so no process it spawns sees either.
//!
//! `pohunek service install|upgrade|uninstall|check|lock` then adopt the
//! captured descriptor as their transaction lock instead of taking a new one
//! (see [`super::record::Store::adopt`]). A variable that does not name a
//! descriptor holding the lock fails those commands; they never fall back to
//! taking a lock of their own, because the ancestor that set the variable
//! relies on its lock covering them.

// Rust guideline compliant 2026-09-28

use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{FromRawFd as _, RawFd};
use std::sync::{Mutex, PoisonError};

use nix::fcntl::{fcntl, FcntlArg, FdFlag};

use super::error::Error;

/// Environment variable naming the inherited transaction-lock descriptor.
///
/// Set by `pohunek service lock` for its child and read by every `pohunek`
/// the child runs. The value is a decimal descriptor number.
pub const LOCK_FD_ENV: &str = "POHUNEK_SERVICE_LOCK_FD";

/// Lowest descriptor a handed-down lock may use; 0 to 2 are standard streams.
const FIRST_NON_STDIO_FD: RawFd = 3;

/// What [`capture`] found in the environment.
#[derive(Debug)]
enum Captured {
    /// The variable was not set, or [`take`] already consumed the capture.
    Absent,
    /// The variable named this open descriptor, now owned by this process.
    Descriptor(File),
    /// The variable was set but named no usable descriptor.
    Invalid(String),
}

/// The capture of this process, consumed by the first [`take`].
static CAPTURED: Mutex<Captured> = Mutex::new(Captured::Absent);

/// Captures the handed-down lock descriptor and clears [`LOCK_FD_ENV`].
///
/// An empty value counts as unset. A value that is not a decimal number of an open descriptor above the
/// standard streams is remembered as invalid and reported by the first
/// command that needs the lock.
///
/// # Safety
///
/// Call this as the first statement of `main`, before any thread starts and
/// before the process opens a descriptor. Only then is every open descriptor
/// above 2 one the process inherited and nothing else owns, which is what
/// taking ownership of the named descriptor requires.
#[expect(
    unsafe_code,
    reason = "owning an inherited descriptor by number has no safe equivalent"
)]
pub unsafe fn capture() {
    let Some(value) = std::env::var_os(LOCK_FD_ENV) else {
        return;
    };
    // No other thread exists yet, so no reader races this removal.
    std::env::remove_var(LOCK_FD_ENV);
    // An empty value is unset, as `packaging/install-daemon.sh` treats it.
    if value.is_empty() {
        return;
    }
    let captured = match parse(&value) {
        Ok(fd) => match fcntl(fd, FcntlArg::F_GETFD) {
            // SAFETY: `fd` is open (F_GETFD succeeded), and per this
            // function's contract it was inherited and nothing else in this
            // process owns it; the swap into `CAPTURED` below makes this the
            // only owner.
            Ok(_flags) => Captured::Descriptor(unsafe { own(fd) }),
            Err(errno) => Captured::Invalid(format!("descriptor {fd} is not open ({errno})")),
        },
        Err(detail) => Captured::Invalid(detail),
    };
    let captured = match captured {
        Captured::Descriptor(file) => match close_on_exec(&file) {
            Ok(()) => Captured::Descriptor(file),
            Err(detail) => Captured::Invalid(detail),
        },
        other => other,
    };
    *CAPTURED.lock().unwrap_or_else(PoisonError::into_inner) = captured;
}

/// Returns the captured descriptor, if [`capture`] found one; later calls see none.
///
/// # Errors
///
/// Returns [`Error::InheritedLock`] when [`LOCK_FD_ENV`] was set to something
/// other than an open descriptor.
pub(crate) fn take() -> Result<Option<File>, Error> {
    let captured = std::mem::replace(
        &mut *CAPTURED.lock().unwrap_or_else(PoisonError::into_inner),
        Captured::Absent,
    );
    match captured {
        Captured::Absent => Ok(None),
        Captured::Descriptor(file) => Ok(Some(file)),
        Captured::Invalid(detail) => Err(Error::InheritedLock { detail }),
    }
}

/// Parses a descriptor number above the standard streams.
fn parse(value: &OsStr) -> Result<RawFd, String> {
    value
        .to_str()
        .and_then(|text| text.parse::<RawFd>().ok())
        .filter(|fd| *fd >= FIRST_NON_STDIO_FD)
        .ok_or_else(|| {
            format!(
                "{value:?} is not a descriptor number of at least {FIRST_NON_STDIO_FD}",
                value = value.to_string_lossy()
            )
        })
}

/// Takes ownership of `fd`.
///
/// # Safety
///
/// `fd` must be open and owned by nothing else in this process.
#[expect(
    unsafe_code,
    reason = "owning an inherited descriptor by number has no safe equivalent"
)]
unsafe fn own(fd: RawFd) -> File {
    // SAFETY: guaranteed by this function's contract.
    unsafe { File::from_raw_fd(fd) }
}

/// Marks `file` close-on-exec so no process this one spawns inherits it.
fn close_on_exec(file: &File) -> Result<(), String> {
    use std::os::fd::AsRawFd as _;

    fcntl(file.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map(drop)
        .map_err(|errno| format!("could not mark the descriptor close-on-exec ({errno})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_descriptor_numbers_above_the_standard_streams_parse() {
        assert_eq!(parse(OsStr::new("3")), Ok(3));
        assert_eq!(parse(OsStr::new("41")), Ok(41));
        for rejected in ["", "0", "2", "-1", "3x", " 3", "fd"] {
            let detail = parse(OsStr::new(rejected)).expect_err(rejected);
            assert!(detail.contains("is not a descriptor number"), "{detail}");
        }
    }

    #[test]
    fn take_reports_an_invalid_capture_once_and_then_nothing() {
        *CAPTURED.lock().expect("capture") = Captured::Invalid("bad".to_owned());
        let error = take().expect_err("invalid capture");
        assert!(matches!(&error, Error::InheritedLock { detail } if detail == "bad"));
        assert_eq!(error.code(), "service_inherited_lock_invalid");
        assert!(take().expect("consumed").is_none());
    }
}

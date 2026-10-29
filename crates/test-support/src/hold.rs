//! A long-lived process image whose environment the kernel always exposes.
//!
//! Fixtures that need a process the product can attribute to a runtime by its
//! ownership markers cannot use a restricted platform binary for it: macOS 27
//! withholds the environment of binaries such as `/bin/sh`, `/bin/cat` and
//! `/bin/sleep`, so the markers of such a process read as unobservable and no
//! sweep can select it. The test binary itself is an image the kernel always
//! exposes. A fixture re-runs it to hold still until it is killed: `exec` keeps
//! the process id, the environment and the ignored signal dispositions, so a
//! script such as `trap '' HUP TERM; exec <holder>` yields a marker-carrying
//! process that survives a hangup and a termination request.
//!
//! A fixture that needs the long-lived process under the name of another
//! program (an agent that process recognition matches by name) runs the image
//! through a link ([`link_as`]) or a file of its own ([`install_at`]). A link or
//! copy of a system binary would not do: a multicall userland such as uutils
//! coreutils picks the applet from the name it runs under and exits on an
//! unknown one.
//!
//! A test binary that starts holders declares the image once at its crate root
//! with [`hold_image!`](crate::hold_image) and builds the command through
//! [`command`] or, inside a shell script, [`shell_words`]. The image ends by
//! itself after [`HOLD_LIFETIME`], so a run that died before cleanup cannot
//! leave the process behind for long.
//!
//! # Examples
//!
//! ```
//! let script = format!("trap '' HUP TERM; exec {}", pohunek_test_support::hold::shell_words());
//! assert!(script.contains("--ignored"));
//! ```

// Rust guideline compliant 2026-10-12

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Upper bound on how long a holder runs when nothing kills it.
///
/// Long enough to outlast any fixture scenario, short enough that a holder
/// orphaned by a crashed run ends within minutes. Shortening it risks ending a
/// holder under a slow scenario; lengthening it only delays the end of an
/// orphan.
pub const HOLD_LIFETIME: Duration = Duration::from_secs(300);

/// Extra libtest filter that marks a launch as a holder.
///
/// A run of the whole binary with `--ignored` (the real-manager CI job does this)
/// selects the holding test too, and it must return at once there. The holders the
/// fixtures start pass this word as a second filter: under `--exact` it matches no
/// test, so it selects nothing, and unlike an environment variable it survives a
/// launcher that clears the environment. It is a bare word because libtest rejects
/// an unknown flag.
pub const HOLD_MARKER: &str = "hold-image-marker";

/// Libtest name of the holding test, declared by [`hold_image!`](crate::hold_image).
///
/// The declaring module sits at the crate root of the test binary, so the name
/// is the same in every binary.
pub const HOLD_TEST: &str = "hold_image::hold";

/// Arguments that make the test binary hold: it runs the holding test alone.
pub const ARGUMENTS: [&str; 4] = ["--exact", HOLD_TEST, "--ignored", HOLD_MARKER];

/// Path of the running test binary, which is also the holding image.
///
/// # Panics
///
/// Panics when the operating system cannot report the path of the running
/// executable.
#[must_use]
pub fn image() -> PathBuf {
    std::env::current_exe().expect("path of the running test binary")
}

/// A command that runs the holding image.
///
/// All three standard streams are null: libtest prints when it starts, and a
/// holder whose terminal hangs up before that write dies of the failed write
/// instead of holding. The caller sets the environment and the working
/// directory. The binary must declare the image with
/// [`hold_image!`](crate::hold_image).
#[must_use]
pub fn command() -> Command {
    command_for(&image())
}

/// A command that runs the holding image under the name `program`.
///
/// `program` is a link to the image ([`link_as`]) or a file of its own
/// ([`install_at`]); the process takes its name and command line from it. The
/// streams are null as for [`command`].
#[must_use]
pub fn command_for(program: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .args(ARGUMENTS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Names the holding image `path`: a symbolic link to the running test binary.
///
/// A fixture that needs a long-lived process under the name of an agent runs
/// the link. Unlike a link to a system binary such as `sleep`, the image does
/// not choose what it does from the name it runs under: a multicall userland
/// (uutils coreutils) answers an unknown name with an error and exits.
///
/// # Errors
///
/// Returns the failure of creating the link.
///
/// # Panics
///
/// Panics when the operating system cannot report the path of the running
/// executable.
pub fn link_as(path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(image(), path)
}

/// Installs the holding image at `path` as a file of its own, so that a process
/// running it reports `path` as its executable.
///
/// The file is a copy of the test binary with its mode, never a hard link: a
/// fixture may change the mode of the installed file, which must not reach the
/// test binary. A child process makes the copy, so this process never holds a
/// write descriptor on a file it is about to run (`ETXTBSY`, see [`crate::fs`]).
///
/// # Errors
///
/// Returns the failure of starting `cp` or its unsuccessful exit.
///
/// # Panics
///
/// Panics when the operating system cannot report the path of the running
/// executable.
pub fn install_at(path: &Path) -> io::Result<()> {
    let status = Command::new("cp")
        .arg(image())
        .arg(path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "cp of the holding image to {} failed: {status}",
            path.display()
        )))
    }
}

/// Shell words that run the holding image, with the path single-quoted.
///
/// The words end in redirections of all three standard streams to `/dev/null`.
/// libtest prints when it starts, and a fixture whose terminal hangs up before
/// that write (a worker killed right after it started the holder) would
/// otherwise kill the holder with the panic of the failed write. Prefix the words
/// with `exec` to replace the shell, or run them as a child. The binary must
/// declare the image with [`hold_image!`](crate::hold_image).
///
/// # Panics
///
/// Panics when the operating system cannot report the path of the running
/// executable, or when the path is not UTF-8.
#[must_use]
pub fn shell_words() -> String {
    shell_words_for(&image())
}

/// [`shell_words`] for the image run under the name `program` ([`link_as`],
/// [`install_at`]).
///
/// # Examples
///
/// ```
/// let words = pohunek_test_support::hold::shell_words_for(std::path::Path::new("/agents/codex"));
/// assert!(words.starts_with("'/agents/codex' --exact"));
/// ```
///
/// # Panics
///
/// Panics when the path is not UTF-8.
#[must_use]
pub fn shell_words_for(program: &Path) -> String {
    let path = program.to_str().expect("the program path is UTF-8");
    format!("'{}' {}", path.replace('\'', r"'\''"), shell_arguments())
}

/// The arguments of a holder as shell words, ending in the redirections of
/// [`shell_words`]; a script that names the program itself appends them to it.
#[must_use]
pub fn shell_arguments() -> String {
    format!("--exact {HOLD_TEST} --ignored {HOLD_MARKER} </dev/null >/dev/null 2>&1")
}

/// Body of the holding test; only [`hold_image!`](crate::hold_image) calls it.
///
/// Returns at once unless the process was started as a holder (see [`HOLD_MARKER`]).
#[doc(hidden)]
pub fn hold() {
    if !std::env::args().any(|argument| argument == HOLD_MARKER) {
        return;
    }
    // timing-allowed: #821 the holder's own lifetime; no scenario waits on this sleep, it ends when the scenario kills the process
    std::thread::sleep(HOLD_LIFETIME);
}

/// Declares the holding test of this test binary.
///
/// Call it once at the crate root of a test binary (a library's unit-test
/// build or an integration-test file). The declared test is ignored, so no
/// ordinary run executes it, and a `--ignored` run of the binary returns from it
/// at once; [`command`] and [`shell_words`] start it as a child process that holds.
#[macro_export]
macro_rules! hold_image {
    () => {
        #[cfg(test)]
        mod hold_image {
            #[test]
            #[ignore = "image of the fixture processes that hold still until they are killed"]
            fn hold() {
                $crate::hold::hold();
            }
        }
    };
}

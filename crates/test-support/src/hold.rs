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

use std::path::PathBuf;
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
    let mut command = Command::new(image());
    command
        .args(["--exact", HOLD_TEST, "--ignored", HOLD_MARKER])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
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
    let path = image();
    let path = path.to_str().expect("the test binary path is UTF-8");
    format!(
        "'{}' --exact {HOLD_TEST} --ignored {HOLD_MARKER} </dev/null >/dev/null 2>&1",
        path.replace('\'', r"'\''")
    )
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

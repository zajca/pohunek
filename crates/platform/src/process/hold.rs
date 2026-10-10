//! A long-lived process image whose environment the kernel always exposes.
//!
//! The process scenarios assert which ownership markers a process carries, so
//! their fixtures need an image whose environment can be read on every host.
//! macOS 27 withholds the environment of restricted platform binaries such as
//! `/bin/sh` and `/bin/sleep`. A shell fixture therefore ends in this test
//! binary, re-run to hold still until it is killed: `exec` keeps the process
//! id, the environment, and ignored signal dispositions, so a script such as
//! `trap '' HUP TERM; exec "$0" ...` yields a marker-carrying process that
//! survives a termination request.

// Rust guideline compliant 2026-10-10

use std::process::Command;
use std::time::Duration;

use pohunek_test_support::env::TestEnv;

/// Upper bound on how long a fixture holds when nothing kills it, so a test
/// run that died before cleanup cannot leave the process behind for long.
const HOLD_LIFETIME: Duration = Duration::from_secs(300);

/// Shell command that runs the holding image.
///
/// `$0` is this test binary, passed by [`shell_command`]; the arguments select
/// [`hold`] through the libtest filter. Prefix it with `exec` to replace the
/// shell, or run it as a child.
pub(super) const HOLD: &str = r#""$0" --exact process::hold::hold --ignored"#;

/// Returns the path of the test binary, which is also the holding image.
pub(super) fn image() -> String {
    std::env::current_exe()
        .expect("path of the running test binary")
        .into_os_string()
        .into_string()
        .expect("the test binary path is UTF-8")
}

/// Builds `/bin/sh -c <script> <test binary>` with the hermetic environment of
/// `env`, so the script can name the holding image as `$0`.
pub(super) fn shell_command(env: &TestEnv, script: &str) -> Command {
    let mut command = env.command("/bin/sh");
    command.args(["-c", script, &image()]);
    command
}

/// Body of the holding image; only the process scenarios start it, as a child.
#[test]
#[ignore = "image of the fixture processes spawned by the process scenarios"]
fn hold() {
    std::thread::sleep(HOLD_LIFETIME);
}

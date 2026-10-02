//! `systemd-analyze verify` of the rendered daemon unit and sessions slice.

// Rust guideline compliant 2026-10-01

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pohunek_platform::supervisor::JobDefinition;

use super::error::{io_error, supervisor_error, Error};
use super::settings;

/// The program that verifies the rendered daemon unit and sessions slice.
///
/// The default is `systemd-analyze` found on `PATH`, run with the caller's
/// environment. A different program or runtime directory is configuration of
/// the same verification path: the units are rendered, written and handed to
/// the program in the same way, and a program that is missing or rejects them
/// still fails the install.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitVerifier {
    program: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
}

impl UnitVerifier {
    /// Runs `program` instead of `systemd-analyze`, as
    /// `program --user verify <unit files>`.
    #[must_use]
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = Some(program.into());
        self
    }

    /// Gives the verifier `dir` as its `XDG_RUNTIME_DIR` instead of the
    /// caller's. `systemd-analyze --user` needs an existing, owner-only
    /// directory there but no running user manager.
    #[must_use]
    pub fn with_runtime_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.runtime_dir = Some(dir.into());
        self
    }

    /// Verifies the rendered daemon unit and sessions slice.
    ///
    /// The files are rendered into a private scratch directory under their
    /// real names, so the verifier sees exactly what the backend installs. A
    /// missing verifier is an error: verification is part of the install.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VerifierMissing`] when the program is absent and
    /// [`Error::UnitVerification`] when it rejects the units or times out.
    pub async fn verify(
        &self,
        namespace: &pohunek_platform::supervisor::Namespace,
        definition: &JobDefinition,
    ) -> Result<(), Error> {
        use pohunek_platform::supervisor::systemd::{render_daemon_unit, render_sessions_slice};

        let verifier = self.resolve(std::env::var_os("PATH").as_deref())?;
        let unit = render_daemon_unit(definition)
            .map_err(|source| supervisor_error("render daemon unit", source))?;
        let scratch = scratch_dir()?;
        let unit_path = scratch.path.join(namespace.daemon_unit());
        let slice_path = scratch.path.join(namespace.sessions_slice());
        std::fs::write(&unit_path, unit).map_err(io_error("write", unit_path.clone()))?;
        std::fs::write(&slice_path, render_sessions_slice())
            .map_err(io_error("write", slice_path.clone()))?;
        run_verifier(
            &verifier,
            self.runtime_dir.as_deref(),
            &[unit_path, slice_path],
        )
        .await
    }

    /// The executable to run: the configured program, or `systemd-analyze`
    /// looked up in `search_path`.
    fn resolve(&self, search_path: Option<&OsStr>) -> Result<PathBuf, Error> {
        match &self.program {
            Some(program) => is_executable(program)
                .then(|| program.clone())
                .ok_or(Error::VerifierMissing),
            None => find_in(VERIFIER, search_path).ok_or(Error::VerifierMissing),
        }
    }
}

/// The systemd unit verifier.
const VERIFIER: &str = "systemd-analyze";

async fn run_verifier(
    verifier: &Path,
    runtime_dir: Option<&Path>,
    files: &[PathBuf],
) -> Result<(), Error> {
    use std::process::Stdio;

    use tokio::io::AsyncReadExt as _;

    let mut command = tokio::process::Command::new(verifier);
    command.args(["--user", "verify"]).args(files);
    if let Some(dir) = runtime_dir {
        command.env("XDG_RUNTIME_DIR", dir);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(io_error("run", verifier))?;
    let limit = u64::try_from(settings::UNIT_VERIFY_OUTPUT).unwrap_or(u64::MAX);
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let run = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut stdout = stdout.take(limit);
        let mut stderr = stderr.take(limit);
        let (read_out, read_err) =
            tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        read_out.and(read_err)?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out, err))
    };
    let (status, out, err) = tokio::time::timeout(settings::UNIT_VERIFY_TIMEOUT, run)
        .await
        .map_err(|_elapsed| Error::UnitVerification {
            detail: format!(
                "{VERIFIER} did not finish within {} s",
                settings::UNIT_VERIFY_TIMEOUT.as_secs()
            ),
        })?
        .map_err(io_error("run", verifier))?;
    if status.success() {
        return Ok(());
    }
    let mut detail = String::from_utf8_lossy(&err).trim().to_owned();
    let stdout = String::from_utf8_lossy(&out);
    if !stdout.trim().is_empty() {
        detail.push('\n');
        detail.push_str(stdout.trim());
    }
    Err(Error::UnitVerification {
        detail: format!("{VERIFIER} exited with {status}: {detail}"),
    })
}

/// A private scratch directory removed on drop.
struct Scratch {
    path: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Scratch holds only rendered unit text; a leftover is harmless.
        let _cleanup = std::fs::remove_dir_all(&self.path);
    }
}

/// Distinguishes scratch directories created in one process within one clock tick.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Name of a scratch directory; the sequence keeps concurrent callers of one
/// process apart when the clock returns the same nanosecond to both.
fn scratch_name(pid: u32, nanos: u128, sequence: u64) -> String {
    format!("pohunek-unit-verify-{pid}-{nanos}-{sequence}")
}

fn scratch_dir() -> Result<Scratch, Error> {
    use std::os::unix::fs::DirBuilderExt as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    let path = std::env::temp_dir().join(scratch_name(
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
        SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .map_err(io_error("create", path.clone()))?;
    Ok(Scratch { path })
}

/// Whether `path` names a regular file with an execute bit.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Finds an executable by name in the `search_path` directories.
fn find_in(name: &str, search_path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(search_path?)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::context::tests::context;
    use crate::service::context::tests::temp_root;
    use crate::service::definition::{daemon_definition, initial_config};
    use pohunek_test_support::env::TestEnv;

    #[test]
    fn scratch_names_differ_for_one_process_and_one_clock_reading() {
        assert_ne!(scratch_name(7, 42, 0), scratch_name(7, 42, 1));
    }

    #[test]
    fn concurrent_scratch_directories_never_collide() {
        let scratches: Vec<_> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        (0..16)
                            .map(|_| scratch_dir().expect("scratch"))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().expect("worker"))
                .collect()
        });
        let distinct: std::collections::BTreeSet<_> = scratches
            .iter()
            .map(|scratch| scratch.path.clone())
            .collect();
        assert_eq!(distinct.len(), scratches.len());
    }

    /// A definition whose daemon executable exists below `root`.
    fn definition_below(
        root: &Path,
    ) -> (
        pohunek_service_config::ServiceConfig,
        pohunek_platform::supervisor::JobDefinition,
    ) {
        let context = context(root);
        let config = initial_config(&context, &root.join("prefix"), "1.2.3").expect("config");
        let daemon = config.daemon_executable();
        std::fs::create_dir_all(daemon.parent().expect("version dir")).expect("version dir");
        std::fs::copy("/bin/true", &daemon).expect("stand-in daemon");
        let definition = daemon_definition(&context, &config).expect("definition");
        (config, definition)
    }

    #[tokio::test]
    async fn rendered_units_pass_systemd_analyze() {
        let env = TestEnv::new().expect("hermetic test environment");
        let (_root, root) = temp_root();
        let (config, definition) = definition_below(&root);
        let verifier = UnitVerifier::default().with_runtime_dir(env.runtime_dir());
        verifier
            .verify(&config.namespace(), &definition)
            .await
            .expect("rendered units verify");

        std::fs::remove_file(config.daemon_executable()).expect("remove daemon");
        assert!(matches!(
            verifier.verify(&config.namespace(), &definition).await,
            Err(Error::UnitVerification { .. })
        ));
    }

    #[tokio::test]
    async fn a_configured_program_accepting_the_units_verifies_them() {
        let (_root, root) = temp_root();
        let (config, definition) = definition_below(&root);
        UnitVerifier::default()
            .with_program("/bin/true")
            .verify(&config.namespace(), &definition)
            .await
            .expect("accepting program");
    }

    #[tokio::test]
    async fn a_configured_program_rejecting_the_units_fails_the_verification() {
        let (_root, root) = temp_root();
        let (config, definition) = definition_below(&root);
        let result = UnitVerifier::default()
            .with_program("/bin/false")
            .verify(&config.namespace(), &definition)
            .await;
        assert!(matches!(result, Err(Error::UnitVerification { .. })));
    }

    #[tokio::test]
    async fn a_missing_configured_program_is_a_missing_verifier() {
        let (_root, root) = temp_root();
        let (config, definition) = definition_below(&root);
        let result = UnitVerifier::default()
            .with_program(root.join("no-such-verifier"))
            .verify(&config.namespace(), &definition)
            .await;
        assert!(matches!(result, Err(Error::VerifierMissing)));
    }

    #[test]
    fn systemd_analyze_is_looked_up_only_in_the_given_search_path() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_root, root) = temp_root();
        let verifier = UnitVerifier::default();
        assert!(matches!(
            verifier.resolve(Some(root.as_os_str())),
            Err(Error::VerifierMissing)
        ));
        assert!(matches!(
            verifier.resolve(None),
            Err(Error::VerifierMissing)
        ));

        let installed = root.join(VERIFIER);
        std::fs::write(&installed, "").expect("write verifier");
        assert!(
            matches!(
                verifier.resolve(Some(root.as_os_str())),
                Err(Error::VerifierMissing)
            ),
            "a file without an execute bit is not a verifier"
        );
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755))
            .expect("chmod verifier");
        assert_eq!(
            verifier
                .resolve(Some(root.as_os_str()))
                .expect("verifier found"),
            installed
        );
    }
}

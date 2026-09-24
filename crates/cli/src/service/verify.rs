//! `systemd-analyze verify` of the rendered daemon unit and sessions slice.

// Rust guideline compliant 2026-09-24

use std::path::{Path, PathBuf};

use pohunek_platform::supervisor::JobDefinition;

use super::error::{io_error, supervisor_error, Error};
use super::settings;

/// Verifies the rendered daemon unit and sessions slice with `systemd-analyze`.
///
/// The files are rendered into a private scratch directory under their real
/// names, so the verifier sees exactly what the backend installs. A missing
/// `systemd-analyze` is an error: verification is part of the install.
///
/// # Errors
///
/// Returns [`Error::VerifierMissing`] when the tool is absent and
/// [`Error::UnitVerification`] when it rejects the units or times out.
pub async fn verify_units(
    namespace: &pohunek_platform::supervisor::Namespace,
    definition: &JobDefinition,
) -> Result<(), Error> {
    use pohunek_platform::supervisor::systemd::{render_daemon_unit, render_sessions_slice};

    let verifier = find_on_path(VERIFIER).ok_or(Error::VerifierMissing)?;
    let unit = render_daemon_unit(definition)
        .map_err(|source| supervisor_error("render daemon unit", source))?;
    let scratch = scratch_dir()?;
    let unit_path = scratch.path.join(namespace.daemon_unit());
    let slice_path = scratch.path.join(namespace.sessions_slice());
    std::fs::write(&unit_path, unit).map_err(io_error("write", unit_path.clone()))?;
    std::fs::write(&slice_path, render_sessions_slice())
        .map_err(io_error("write", slice_path.clone()))?;
    run_verifier(&verifier, &[unit_path, slice_path]).await
}

/// The systemd unit verifier.
const VERIFIER: &str = "systemd-analyze";

async fn run_verifier(verifier: &Path, files: &[PathBuf]) -> Result<(), Error> {
    use std::process::Stdio;

    use tokio::io::AsyncReadExt as _;

    let mut child = tokio::process::Command::new(verifier)
        .args(["--user", "verify"])
        .args(files)
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

fn scratch_dir() -> Result<Scratch, Error> {
    use std::os::unix::fs::DirBuilderExt as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    let path = std::env::temp_dir().join(format!(
        "pohunek-unit-verify-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .map_err(io_error("create", path.clone()))?;
    Ok(Scratch { path })
}

/// Finds an executable by name on `PATH`.
fn find_on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::context::tests::context;
    use crate::service::definition::{daemon_definition, initial_config};

    #[tokio::test]
    async fn rendered_units_pass_systemd_analyze() {
        let root = tempfile::tempdir().expect("temp dir");
        let context = context(root.path());
        let config =
            initial_config(&context, &root.path().join("prefix"), "1.2.3").expect("config");
        let daemon = config.daemon_executable();
        std::fs::create_dir_all(daemon.parent().expect("version dir")).expect("version dir");
        std::fs::copy("/bin/true", &daemon).expect("stand-in daemon");
        let definition = daemon_definition(&context, &config).expect("definition");
        verify_units(&config.namespace(), &definition)
            .await
            .expect("rendered units verify");

        std::fs::remove_file(&daemon).expect("remove daemon");
        assert!(matches!(
            verify_units(&config.namespace(), &definition).await,
            Err(Error::UnitVerification { .. })
        ));
    }
}

//! The isolated release prefix, throwaway catalog trust and running daemon
//! through which the compatibility gate installs the Hermes runtime package.
//!
//! The gate takes release binaries and a package archive, never a source
//! switch. It copies the binaries into a private prefix whose trust anchor
//! trusts one key that exists only in this process, signs a catalog with that
//! key that authorizes exactly the given archive, starts the copied daemon,
//! and installs the archive with `pohunek plugin install --catalog`. The CLI
//! then renders the operator assets from the package it selects, the way it
//! does on an operator host.

// Rust guideline compliant 2026-10-12

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use package::{
    read_archive, CatalogEntry, Limits as PackageLimits, PackageDigest, ANCHOR_FILE_NAME,
};
use serde_json::Value;

use super::{
    bounded_process_failure, fail, push_path_forms, require_safe_pohunek_binary, run_process,
    sanitize_output, ProcessOutput, DAEMON_SOCKET_RELATIVE_PATH, EXPECTED_PACKAGE_ID,
    EXPECTED_RUNTIME_ID, PROCESS_POLL_INTERVAL,
};
use crate::catalog::{archive_identity, read_input};
use crate::throwaway_trust::{self, write_public, CoreRelease, Trust};
use crate::XtaskError;

/// The release executables the gate installs, by file name in `--bin-dir`.
pub(super) const BINARIES: [&str; 3] = ["pohunek", "pohunekd", "pohunek-sessiond"];

/// Name of the signed catalog file written next to the anchor.
pub(crate) const CATALOG_FILE: &str = "runtime-catalog.json";

/// Mode of the prefix directories and installed executables: the service
/// installer's public mode. The daemon refuses an anchor whose directory chain
/// group or others can write.
const PUBLIC_EXECUTABLE_MODE: u32 = 0o755;

/// Directory below the isolation root that holds the installed executables
/// and the anchor beside the daemon.
const PREFIX_DIRECTORY: &str = "prefix";

/// Directory below the isolation root that holds the archive and catalog the
/// CLI installs from.
const CATALOG_DIRECTORY: &str = "catalog";

/// File name of the archive copy the CLI installs.
const ARCHIVE_FILE: &str = "hermes.tar.zst";

/// File the daemon's combined output is written to.
const DAEMON_LOG_FILE: &str = "pohunekd.log";

/// Variable selecting direct-child worker supervision, the documented
/// dev/test contract for a daemon without a service manager.
const LAUNCHER_VARIABLE: &str = "POHUNEK_WORKER_LAUNCHER";

/// Value of [`LAUNCHER_VARIABLE`] for supervision without a service manager.
const SUBPROCESS_LAUNCHER: &str = "subprocess";

/// Shell the daemon hands to terminal sessions; the gate starts none.
const DAEMON_SHELL: &str = "/bin/sh";

/// Longest a daemon may take to accept connections. It opens an empty store
/// and binds its socket within a few seconds; the bound ends a hung start on a
/// loaded runner instead of waiting forever.
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest the daemon may take to exit after SIGTERM: it stops its workers
/// and flushes its event log (itself bounded at 5 s).
const DAEMON_EXIT_TIMEOUT: Duration = Duration::from_mins(1);

/// Trailing daemon-log bytes a failure reports, after sanitization.
const DAEMON_LOG_TAIL_BYTES: u64 = 4 * 1024;

/// Publication counter of the Hermes gate's throwaway catalog. The gate's
/// prefix starts with an empty catalog state, so no high-water mark exists
/// and any positive value is accepted.
const CATALOG_SEQUENCE: u64 = 1;

/// The release binaries and the Hermes package archive the gate installs.
#[derive(Debug)]
pub(super) struct Inputs {
    bin_dir: PathBuf,
    package: PathBuf,
}

impl Inputs {
    /// Checks that `bin_dir` holds the three safe release executables and that
    /// `package` names a regular file, without reading the archive yet.
    pub(super) fn new(bin_dir: &Path, package: &Path) -> Result<Self, XtaskError> {
        if !bin_dir.is_absolute() || !package.is_absolute() {
            return Err(fail(
                "Hermes compatibility requires an absolute --bin-dir and --package",
            ));
        }
        for name in BINARIES {
            require_safe_pohunek_binary(&bin_dir.join(name))?;
        }
        let metadata = fs::symlink_metadata(package)
            .map_err(|_error| fail("the Hermes package archive does not exist"))?;
        if !metadata.is_file() {
            return Err(fail("the Hermes package archive is not a regular file"));
        }
        Ok(Self {
            bin_dir: bin_dir.to_path_buf(),
            package: package.to_path_buf(),
        })
    }

    /// Reads the archive and checks that it is the official Hermes package.
    pub(super) fn read_package(&self) -> Result<Package, XtaskError> {
        read_package(&self.package)
    }
}

/// A Hermes runtime package archive read into memory.
#[derive(Debug)]
pub(super) struct Package {
    bytes: Vec<u8>,
    digest: PackageDigest,
    entry: CatalogEntry,
}

impl Package {
    /// The archive digest every provenance report must name.
    pub(super) fn digest(&self) -> &PackageDigest {
        &self.digest
    }
}

/// Reads the archive at `path` and checks that it is the official Hermes
/// package: its descriptor declares the package and runtime id of the lock.
pub(super) fn read_package(path: &Path) -> Result<Package, XtaskError> {
    let limits = PackageLimits::DEFAULT;
    let bytes = read_input(path, limits.max_compressed_bytes)?;
    let verified = read_archive(&bytes, &limits).map_err(XtaskError::Package)?;
    let identity = archive_identity(&verified, path)?;
    if identity.package_id.as_str() != EXPECTED_PACKAGE_ID
        || identity.runtime_id.as_str() != EXPECTED_RUNTIME_ID
    {
        return Err(fail(format!(
            "the package archive is not the {EXPECTED_PACKAGE_ID} package of runtime `{EXPECTED_RUNTIME_ID}`"
        )));
    }
    let digest = verified.digest().clone();
    let entry = throwaway_trust::package_entry(identity, digest.clone())?;
    Ok(Package {
        bytes,
        digest,
        entry,
    })
}

/// Signs a catalog that authorizes exactly `package` with a fresh random key.
fn throwaway_trust(package: &Package) -> Result<Trust, XtaskError> {
    let release = CoreRelease {
        sequence: CATALOG_SEQUENCE,
        ..throwaway_trust::core_release(env!("CARGO_PKG_VERSION"))?
    };
    throwaway_trust::sign(std::slice::from_ref(&package.entry), &release)
}

/// Writes the anchor file and the signed catalog for `package` into
/// `output_dir`, which must not exist yet.
pub(crate) fn write_throwaway_trust(package: &Path, output_dir: &Path) -> Result<(), XtaskError> {
    let package = read_package(package)?;
    let trust = throwaway_trust(&package)?;
    fs::DirBuilder::new()
        .mode(PUBLIC_EXECUTABLE_MODE)
        .create(output_dir)
        .map_err(|error| {
            fail(format!(
                "cannot create the throwaway catalog directory `{}`: {error}",
                output_dir.display()
            ))
        })?;
    write_public(&output_dir.join(ANCHOR_FILE_NAME), &trust.anchor)?;
    write_public(&output_dir.join(CATALOG_FILE), &trust.catalog)
}

/// A running copy of the release daemon in the isolated prefix, and the CLI
/// copy that talks to it. Dropping it kills and reaps the daemon.
#[derive(Debug)]
pub(super) struct Host {
    prefix: PathBuf,
    root: PathBuf,
    archive: PathBuf,
    catalog: PathBuf,
    daemon: Option<Child>,
}

impl Host {
    /// Installs the binaries and the throwaway trust below `root` and starts
    /// the daemon with `env` (plus the subprocess launcher) in `cwd`.
    ///
    /// `socket` is the control socket the daemon binds under that environment.
    pub(super) fn start(
        inputs: &Inputs,
        package: &Package,
        root: &Path,
        env: &[(OsString, OsString)],
        cwd: &Path,
        socket: &Path,
    ) -> Result<Self, XtaskError> {
        let trust = throwaway_trust(package)?;
        let prefix = root.join(PREFIX_DIRECTORY);
        let catalog_dir = root.join(CATALOG_DIRECTORY);
        for directory in [&prefix, &catalog_dir] {
            fs::DirBuilder::new()
                .mode(PUBLIC_EXECUTABLE_MODE)
                .create(directory)
                .map_err(|error| fail(format!("cannot create the gate prefix: {error}")))?;
        }
        for name in BINARIES {
            let installed = prefix.join(name);
            fs::copy(inputs.bin_dir.join(name), &installed)
                .and_then(|_bytes| {
                    fs::set_permissions(
                        &installed,
                        fs::Permissions::from_mode(PUBLIC_EXECUTABLE_MODE),
                    )
                })
                .map_err(|error| {
                    fail(format!(
                        "cannot install `{name}` into the gate prefix: {error}"
                    ))
                })?;
        }
        write_public(&prefix.join(ANCHOR_FILE_NAME), &trust.anchor)?;
        let archive = catalog_dir.join(ARCHIVE_FILE);
        let catalog = catalog_dir.join(CATALOG_FILE);
        write_public(&archive, &package.bytes)?;
        write_public(&catalog, &trust.catalog)?;

        let log = File::create(root.join(DAEMON_LOG_FILE))
            .map_err(|error| fail(format!("cannot create the daemon log: {error}")))?;
        let log_err = log
            .try_clone()
            .map_err(|error| fail(format!("cannot duplicate the daemon log: {error}")))?;
        let mut command = Command::new(prefix.join("pohunekd"));
        command
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().cloned())
            .env("SHELL", DAEMON_SHELL)
            .env(LAUNCHER_VARIABLE, SUBPROCESS_LAUNCHER)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err);
        super::configure_process_group(&mut command);
        let daemon = command
            .spawn()
            .map_err(|error| fail(format!("failed to start the release daemon: {error}")))?;
        let mut host = Self {
            prefix,
            root: root.to_path_buf(),
            archive,
            catalog,
            daemon: Some(daemon),
        };
        host.wait_ready(socket)?;
        Ok(host)
    }

    /// The installed CLI copy.
    pub(super) fn pohunek(&self) -> PathBuf {
        self.prefix.join("pohunek")
    }

    /// Installs the archive through the CLI and the daemon, and checks that
    /// the daemon lists exactly that digest as the enabled, selected official
    /// Hermes package.
    pub(super) fn install_package(
        &self,
        package: &Package,
        env: &[(OsString, OsString)],
        cwd: &Path,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<(), XtaskError> {
        let pohunek = self.pohunek();
        let install = vec![
            "plugin".to_owned(),
            "install".to_owned(),
            self.archive.to_string_lossy().into_owned(),
            "--catalog".to_owned(),
            self.catalog.to_string_lossy().into_owned(),
            "--yes".to_owned(),
            "--json".to_owned(),
        ];
        let output = run_process(&pohunek, &install, cwd, env, timeout, max_bytes)?;
        let installed = self.json_result(
            &output,
            "Pohunek could not install the Hermes package from its throwaway catalog",
        )?;
        let digest = package.digest().as_str();
        if installed["status"] != "installed"
            || installed["package"]["origin"] != "official"
            || installed["package"]["digest"] != digest
        {
            return Err(fail(
                "`pohunek plugin install` did not report the official Hermes package archive as installed",
            ));
        }
        let list = vec!["plugin".to_owned(), "list".to_owned(), "--json".to_owned()];
        let output = run_process(&pohunek, &list, cwd, env, timeout, max_bytes)?;
        let listed = self.json_result(&output, "`pohunek plugin list` failed")?;
        let selected = listed["packages"].as_array().is_some_and(|packages| {
            packages.len() == 1
                && packages[0]["package"]["id"] == EXPECTED_PACKAGE_ID
                && packages[0]["origin"] == "official"
                && packages[0]["enabled"] == true
                && packages[0]["selected"] == true
                && packages[0]["digest"] == digest
        });
        if !selected {
            return Err(fail(
                "the daemon does not list the installed archive as the one enabled, selected official Hermes package",
            ));
        }
        Ok(())
    }

    /// The `ok` member of a successful `--json` document on stdout.
    fn json_result(&self, output: &ProcessOutput, what: &str) -> Result<Value, XtaskError> {
        if output.status.code() != Some(0) {
            return Err(bounded_process_failure(
                what,
                output,
                &self.sensitive_paths(),
            ));
        }
        let document: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_error| fail(format!("{what}: stdout is not one JSON document")))?;
        Ok(document["ok"].clone())
    }

    fn sensitive_paths(&self) -> [&Path; 2] {
        [self.root.as_path(), self.prefix.as_path()]
    }

    /// Polls the control socket until the daemon accepts a connection.
    fn wait_ready(&mut self, socket: &Path) -> Result<(), XtaskError> {
        let deadline = Instant::now() + DAEMON_READY_TIMEOUT;
        loop {
            if let Some(status) = self
                .daemon
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
            {
                return Err(fail(format!(
                    "the release daemon exited before it accepted connections ({}):\n{}",
                    super::exit_description(status),
                    self.log_tail()
                )));
            }
            if UnixStream::connect(socket).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(fail(format!(
                    "the release daemon did not accept connections within {DAEMON_READY_TIMEOUT:?}:\n{}",
                    self.log_tail()
                )));
            }
            thread::sleep(PROCESS_POLL_INTERVAL);
        }
    }

    /// Signals the daemon with SIGTERM and requires a successful exit within
    /// [`DAEMON_EXIT_TIMEOUT`].
    pub(super) fn stop(mut self) -> Result<(), XtaskError> {
        let Some(mut daemon) = self.daemon.take() else {
            return Ok(());
        };
        let pid = nix::unistd::Pid::from_raw(
            i32::try_from(daemon.id()).map_err(|_error| fail("the daemon pid is out of range"))?,
        );
        if let Err(error) = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM) {
            self.daemon = Some(daemon);
            return Err(fail(format!("cannot signal the release daemon: {error}")));
        }
        let deadline = Instant::now() + DAEMON_EXIT_TIMEOUT;
        loop {
            match daemon.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    return Err(fail(format!(
                        "the release daemon did not exit cleanly ({}):\n{}",
                        super::exit_description(status),
                        self.log_tail()
                    )))
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(PROCESS_POLL_INTERVAL),
                Ok(None) => {
                    self.daemon = Some(daemon);
                    return Err(fail(format!(
                        "the release daemon did not exit within {DAEMON_EXIT_TIMEOUT:?} of SIGTERM:\n{}",
                        self.log_tail()
                    )));
                }
                Err(error) => {
                    self.daemon = Some(daemon);
                    return Err(fail(format!("cannot wait for the release daemon: {error}")));
                }
            }
        }
    }

    /// The sanitized tail of the daemon's combined output.
    fn log_tail(&self) -> String {
        let path = self.root.join(DAEMON_LOG_FILE);
        let mut bytes = Vec::new();
        let read = File::open(&path).and_then(|mut file| {
            let length = file.metadata()?.len();
            file.seek(SeekFrom::Start(
                length.saturating_sub(DAEMON_LOG_TAIL_BYTES),
            ))?;
            file.read_to_end(&mut bytes)
        });
        if read.is_err() {
            return "daemon log unavailable".to_owned();
        }
        let mut paths = Vec::new();
        push_path_forms(&mut paths, &self.root, "<ISOLATED_ROOT>");
        if let Some(home) = std::env::var_os("HOME") {
            push_path_forms(&mut paths, Path::new(&home), "<USER_HOME>");
        }
        paths.sort_by_key(|path| std::cmp::Reverse(path.0.len()));
        sanitize_output(&bytes, &paths)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            // The daemon leads its own process group, so this also ends any
            // worker it started. It may have exited already; nothing needs it
            // past this point.
            super::kill_process_group(daemon.id());
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

/// Socket the daemon binds below the runtime directory `runtime_home`.
pub(super) fn daemon_socket(runtime_home: &Path) -> PathBuf {
    runtime_home.join(DAEMON_SOCKET_RELATIVE_PATH)
}

//! The official runtime step of an install or upgrade.
//!
//! The release's `pohunekd ensure-official-runtimes` plans, and then applies,
//! the official runtime packages the host uses and the host profiles that
//! pin to them (see `pohunek_service_config::runtimes`). A plan runs while the
//! previous daemon still runs; an apply runs only after that daemon is
//! stopped, and only once the state it may change is backed up, so a rollback
//! can restore exactly the bytes it replaced.

// Rust guideline compliant 2026-10-14

use std::fmt::Debug;
use std::future::Future;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use pohunek_paths::DAEMON_EXECUTABLE_NAME;
use pohunek_platform::filesystem::{AdvisoryLock, EntryKind, FsError, TrustedDir};
use pohunek_service_config::runtimes::{
    CatalogStatus, EnsureReport, IntegrationAction, PackageStatus, ProfileStatus, DRY_RUN_FLAG,
    ENSURE_REPORT_VERSION, ENSURE_RUNTIMES_ARG,
};
use tokio::io::AsyncReadExt as _;

use super::context::Context;
use super::engine::config_digest;
use super::error::Error;
use super::layout;
use super::preflight::set_search_path;
use super::record::{RuntimeBackup, RuntimeBackupEntry, RuntimeDir, Store};
use super::report::IntegrationReinstall;
use super::settings;
use crate::commands::integration::IntegrationEnv;
use crate::commands::plugin_profile::AGENTS_DIR;

/// Name of the registry record the package store keeps in its directory.
///
/// Matches `REGISTRY_NAME` of `pohunek_package::registry`.
const REGISTRY_FILE: &str = "registry.json";

/// Name of the catalog state file the package store keeps in its directory.
///
/// Matches `CATALOG_STATE_NAME` of `pohunek_package::catalog_state`.
const CATALOG_STATE_FILE: &str = "catalog-state.json";

/// Extension of a host profile file in the agents directory.
const PROFILE_EXTENSION: &str = "toml";

/// Mode of a state file the runtime step creates: owner read and write only.
const FILE_MODE_OWNER: u32 = 0o600;

/// The permission bits of a file mode.
const PERMISSION_BITS: u32 = 0o777;

/// The group and other write bits the daemon's profile reader refuses.
const GROUP_OR_OTHER_WRITE: u32 = 0o022;

/// Mode of a state directory the runtime step creates or restores into.
///
/// The package store and the profiles are owner-private; the daemon refuses
/// a profile wider than `0600`.
const STATE_DIR_MODE: u32 = 0o700;

/// Reinstalls the integration hooks a release lists, once its daemon serves.
pub trait IntegrationStep: Debug + Send + Sync {
    /// Returns one outcome per listed hook home, and the Hermes plugin
    /// outcomes, all resolved in `env`.
    fn reinstall<'a>(
        &'a self,
        env: &'a IntegrationEnv,
        actions: &'a [IntegrationAction],
    ) -> Pin<Box<dyn Future<Output = Vec<IntegrationReinstall>> + Send + 'a>>;
}

/// Reinstalls through the CLI's integration commands.
#[derive(Debug, Clone, Copy, Default)]
pub struct CliIntegrations;

impl IntegrationStep for CliIntegrations {
    fn reinstall<'a>(
        &'a self,
        env: &'a IntegrationEnv,
        actions: &'a [IntegrationAction],
    ) -> Pin<Box<dyn Future<Output = Vec<IntegrationReinstall>> + Send + 'a>> {
        Box::pin(crate::commands::integration::reinstall_after_upgrade(
            env, actions, true,
        ))
    }
}

/// A pending runtime step run.
pub type Running<'a> = Pin<Box<dyn Future<Output = Result<EnsureReport, Error>> + Send + 'a>>;

/// Runs `pohunekd ensure-official-runtimes` of the release in `dir`.
pub trait RuntimeStep: Debug + Send + Sync {
    /// Returns the report of a plan (`dry_run`) or of an apply in `dir`, run
    /// with the `PATH` `search_path` gives the daemon job.
    fn run<'a>(
        &'a self,
        context: &'a Context,
        dir: &'a Path,
        dry_run: bool,
        search_path: &'a str,
    ) -> Running<'a>;
}

/// Runs the daemon executable of the release as a child process.
#[derive(Debug, Clone, Copy)]
pub struct DaemonRuntimes {
    /// How long a plan may run before it is ended.
    pub plan_timeout: Duration,
    /// How long an apply may run before it is ended.
    pub apply_timeout: Duration,
    /// How long an ended child may take to be reaped before the step gives up
    /// on confirming its exit.
    pub exit_timeout: Duration,
}

impl Default for DaemonRuntimes {
    fn default() -> Self {
        Self {
            plan_timeout: settings::RUNTIMES_PLAN_TIMEOUT,
            apply_timeout: settings::RUNTIMES_APPLY_TIMEOUT,
            exit_timeout: settings::DAEMON_EXIT_TIMEOUT,
        }
    }
}

impl RuntimeStep for DaemonRuntimes {
    fn run<'a>(
        &'a self,
        context: &'a Context,
        dir: &'a Path,
        dry_run: bool,
        search_path: &'a str,
    ) -> Running<'a> {
        Box::pin(run_daemon(*self, context, dir, dry_run, search_path))
    }
}

/// Runs the step's daemon with the environment the installed daemon gets.
///
/// A run that outlives its timeout is ended and reaped before this returns, so
/// a caller that restores state after the error never races the process. When
/// the exit cannot be confirmed, the error is [`Error::UpgradeRuntimesUnconfirmed`],
/// which keeps the transaction instead of rolling it back.
async fn run_daemon(
    runtimes: DaemonRuntimes,
    context: &Context,
    dir: &Path,
    dry_run: bool,
    search_path: &str,
) -> Result<EnsureReport, Error> {
    let daemon = dir.join(DAEMON_EXECUTABLE_NAME);
    let failure = |detail: String| Error::UpgradeRuntimesFailed {
        binary: daemon.clone(),
        detail,
    };
    let mut command = tokio::process::Command::new(&daemon);
    command.arg(ENSURE_RUNTIMES_ARG);
    if dry_run {
        command.arg(DRY_RUN_FLAG);
    }
    command
        .env_clear()
        .envs(context.bootstrap_environment()?)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    set_search_path(&mut command, search_path);
    let mut child =
        layout::retry_while_exec_busy(settings::EXEC_BUSY_WAIT, settings::EXEC_BUSY_POLL, || {
            command.spawn()
        })
        .await
        .map_err(|error| failure(format!("cannot run it: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .expect("stdout was configured as a pipe");
    let stderr = child
        .stderr
        .take()
        .expect("stderr was configured as a pipe");
    let timeout = if dry_run {
        runtimes.plan_timeout
    } else {
        runtimes.apply_timeout
    };
    let run = async {
        let read = async {
            tokio::join!(
                read_bounded(stdout, settings::RUNTIMES_OUTPUT),
                read_bounded(stderr, settings::PREFLIGHT_DIAGNOSTICS),
            )
        };
        let ((report, diagnostics), status) = tokio::join!(read, child.wait());
        (report, diagnostics, status)
    };
    let (report, diagnostics, status) = match tokio::time::timeout(timeout, run).await {
        Ok(finished) => finished,
        Err(_elapsed) => {
            // End the daemon and reap it before reporting, so no caller acts
            // on its state while it still runs.
            let _ = child.start_kill();
            return match tokio::time::timeout(runtimes.exit_timeout, child.wait()).await {
                Ok(Ok(_)) => Err(failure(format!(
                    "it did not finish within {} s and was ended",
                    timeout.as_secs()
                ))),
                _ => Err(Error::UpgradeRuntimesUnconfirmed {
                    binary: daemon.clone(),
                    detail: format!(
                        "it did not finish within {} s and its exit was not confirmed within {} s",
                        timeout.as_secs(),
                        runtimes.exit_timeout.as_secs()
                    ),
                }),
            };
        }
    };
    let status = status.map_err(|error| failure(format!("cannot wait for it: {error}")))?;
    if !status.success() {
        let diagnostics = String::from_utf8_lossy(&diagnostics.unwrap_or_default())
            .trim()
            .to_owned();
        return Err(failure(format!("it exited with {status}: {diagnostics}")));
    }
    let report = report.map_err(|error| failure(format!("cannot read its report: {error}")))?;
    let report: EnsureReport = serde_json::from_slice(&report)
        .map_err(|error| failure(format!("its report is not valid: {error}")))?;
    if report.report_version != ENSURE_REPORT_VERSION {
        return Err(failure(format!(
            "its report has version {}, expected {ENSURE_REPORT_VERSION}",
            report.report_version
        )));
    }
    if report.dry_run != dry_run {
        return Err(failure(format!(
            "its report says dry_run={}, the step asked for dry_run={dry_run}",
            report.dry_run
        )));
    }
    Ok(report)
}

/// Keeps at most `limit` bytes of `reader` and discards the rest.
async fn read_bounded(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX);
    (&mut reader).take(cap).read_to_end(&mut bytes).await?;
    tokio::io::copy(&mut reader, &mut tokio::io::sink()).await?;
    Ok(bytes)
}

/// Whether applying `plan` changes anything: a package to install, a profile
/// to pin, or the release catalog's trust state to record. An empty plan lets
/// the step skip the daemon stop.
#[must_use]
pub fn needs_apply(plan: &EnsureReport) -> bool {
    plan.packages
        .iter()
        .any(|package| package.status == PackageStatus::Install)
        || plan
            .profiles
            .iter()
            .any(|profile| profile.status == ProfileStatus::Bind)
        || plan.catalog == CatalogStatus::Record
}

/// Checks that a plan carries no refusal, the precondition of any change.
///
/// # Errors
///
/// Returns [`Error::UpgradeRuntimesRefused`] with every refusal of `plan`.
pub fn check_plan(plan: &EnsureReport) -> Result<(), Error> {
    if plan.refusals.is_empty() {
        Ok(())
    } else {
        Err(Error::UpgradeRuntimesRefused {
            refusals: plan.refusals.clone(),
        })
    }
}

/// Checks that an apply finished: no refusal and no action left undone.
///
/// # Errors
///
/// Returns [`Error::UpgradeRuntimesRefused`] for a refusal and
/// [`Error::UpgradeRuntimesFailed`] for a package, profile, or catalog record
/// still to do.
pub fn check_applied(dir: &Path, report: &EnsureReport) -> Result<(), Error> {
    check_plan(report)?;
    let mut undone = report
        .packages
        .iter()
        .filter(|package| package.status == PackageStatus::Install)
        .map(|package| package.runtime.clone())
        .chain(
            report
                .profiles
                .iter()
                .filter(|profile| profile.status == ProfileStatus::Bind)
                .map(|profile| profile.name.clone()),
        )
        .collect::<Vec<_>>();
    if report.catalog == CatalogStatus::Record {
        undone.push("the release catalog".to_owned());
    }
    if undone.is_empty() {
        return Ok(());
    }
    Err(Error::UpgradeRuntimesFailed {
        binary: dir.join(DAEMON_EXECUTABLE_NAME),
        detail: format!("the apply left {} undone", undone.join(", ")),
    })
}

/// One state file the step may change, read before the step.
#[derive(Debug)]
pub struct Captured {
    /// The journal entry, digest included.
    pub entry: RuntimeBackupEntry,
    /// The bytes before the step; empty when the file did not exist.
    pub bytes: Vec<u8>,
}

/// Reads every state file the step may change: the package registry and the
/// catalog state, and each profile in the agents directory.
///
/// # Errors
///
/// Returns a filesystem error for an unsafe directory or file, and
/// [`Error::RuntimeBackup`] for a file over [`settings::RUNTIME_STATE_BYTES`].
pub fn capture(context: &Context) -> Result<Vec<Captured>, Error> {
    let plugins = context.paths().plugins_dir();
    let agents = context.paths().config_dir.join(AGENTS_DIR);
    let mut captured = Vec::new();
    if let Some(directory) = layout::open_existing_owner_dir(&plugins)? {
        for name in [REGISTRY_FILE, CATALOG_STATE_FILE] {
            captured.push(capture_file(
                &directory,
                RuntimeDir::Plugins,
                name,
                Accepts::ExactlyOwnerReadWrite,
            )?);
        }
    } else {
        for name in [REGISTRY_FILE, CATALOG_STATE_FILE] {
            captured.push(absent(RuntimeDir::Plugins, name));
        }
    }
    if let Some(directory) = layout::open_existing_owner_dir(&agents)? {
        for name in profile_names(&directory)? {
            captured.push(capture_file(
                &directory,
                RuntimeDir::Agents,
                &name,
                Accepts::NotWritableByOthers,
            )?);
        }
    }
    Ok(captured)
}

/// The profile file names in `directory`, sorted so the journal is stable.
fn profile_names(directory: &TrustedDir) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    for name in directory
        .entry_names()
        .map_err(|source| super::error::fs_error("list profiles", source))?
    {
        let Some(text) = name.to_str() else {
            continue;
        };
        if std::path::Path::new(text)
            .extension()
            .is_some_and(|extension| extension == PROFILE_EXTENSION)
        {
            names.push(text.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// What the reader of a state file accepts; a backup accepts exactly that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Accepts {
    /// The package store reads its registry and catalog state only with mode
    /// `0600`, so any other mode is refused by the store itself.
    ExactlyOwnerReadWrite,
    /// The daemon's profile reader and binder take a regular file owned by the
    /// user that no group or other user can write, whatever its other bits.
    NotWritableByOthers,
}

/// Reads one regular state file, or records its absence.
///
/// The file must be a regular file owned by the current user and accepted by
/// `accepts`. It is read with exactly the mode it has, and the backup keeps
/// that mode so a restore reproduces it.
fn capture_file(
    directory: &TrustedDir,
    dir: RuntimeDir,
    name: &str,
    accepts: Accepts,
) -> Result<Captured, Error> {
    let present = directory
        .entry_identity(name, EntryKind::RegularFile)
        .map_err(|source| super::error::fs_error("inspect runtime state", source))?
        .is_some();
    if !present {
        return Ok(absent(dir, name));
    }
    let path = directory.path().join(name);
    let metadata = std::fs::symlink_metadata(&path).map_err(|source| Error::RuntimeBackup {
        path: path.clone(),
        detail: source.to_string(),
    })?;
    let refused = |detail: String| Error::RuntimeBackup {
        path: path.clone(),
        detail,
    };
    if metadata.uid() != nix::unistd::Uid::effective().as_raw() {
        return Err(refused("it is not owned by the current user".to_owned()));
    }
    let mode = metadata.permissions().mode() & PERMISSION_BITS;
    match accepts {
        Accepts::ExactlyOwnerReadWrite if mode != FILE_MODE_OWNER => {
            return Err(refused(format!(
                "the package store reads it only with mode {FILE_MODE_OWNER:#o}, found {mode:#o}"
            )));
        }
        Accepts::NotWritableByOthers if mode & GROUP_OR_OTHER_WRITE != 0 => {
            return Err(refused(format!(
                "its mode {mode:#o} lets group or other users write it"
            )));
        }
        _ => {}
    }
    let bytes = directory
        .read_file(name, mode, settings::RUNTIME_STATE_BYTES)
        .map_err(|source| refused(source.to_string()))?;
    Ok(Captured {
        entry: RuntimeBackupEntry {
            dir,
            name: name.to_owned(),
            digest: Some(config_digest(&bytes)),
            mode,
        },
        bytes,
    })
}

fn absent(dir: RuntimeDir, name: &str) -> Captured {
    Captured {
        entry: RuntimeBackupEntry {
            dir,
            name: name.to_owned(),
            digest: None,
            mode: FILE_MODE_OWNER,
        },
        bytes: Vec::new(),
    }
}

/// Builds the journal of `captured` once the backup files are durable.
#[must_use]
pub fn journal(captured: &[Captured]) -> RuntimeBackup {
    RuntimeBackup {
        entries: captured.iter().map(|file| file.entry.clone()).collect(),
    }
}

/// Checks every backup file against its journaled digest.
///
/// # Errors
///
/// Returns [`Error::RuntimeBackup`] for a missing, oversized, or altered file.
pub fn verify_backup(backup: &RuntimeBackup, store: &Store) -> Result<(), Error> {
    for (index, entry) in backup.entries.iter().enumerate() {
        let Some(digest) = &entry.digest else {
            continue;
        };
        let bytes = store.read_runtime_backup(index, settings::RUNTIME_STATE_BYTES)?;
        if &config_digest(&bytes) != digest {
            return Err(Error::RuntimeBackup {
                path: store.runtime_backup_path(index),
                detail: "its bytes do not match the journaled digest".to_owned(),
            });
        }
    }
    Ok(())
}

/// Takes the daemon data authority lock without waiting for it.
///
/// The daemon holds this lock for its whole run and for the official runtime
/// step, so a held lock means a daemon may still write the state a restore is
/// about to replace. A data directory that does not exist holds no lock, and
/// the guard is released when it is dropped.
///
/// # Errors
///
/// Returns [`Error::UpgradeRuntimesUnconfirmed`] when the lock is held, and a
/// filesystem error when the data directory cannot be opened or locked.
fn acquire_data_authority(context: &Context, daemon: &Path) -> Result<Option<AdvisoryLock>, Error> {
    let path = context.paths().data_authority_lock_path();
    let unconfirmed = || Error::UpgradeRuntimesUnconfirmed {
        binary: daemon.to_path_buf(),
        detail: format!(
            "the data authority lock {} is held by a running daemon",
            path.display()
        ),
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(unconfirmed());
    };
    let directory = match TrustedDir::open_absolute(parent, STATE_DIR_MODE) {
        Ok(directory) => directory,
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => return Ok(None),
        Err(source) => return Err(super::error::fs_error("open the data directory", source)),
    };
    match directory.acquire_lock(name, FILE_MODE_OWNER) {
        Ok(lock) => Ok(Some(lock)),
        Err(FsError::LockContended { .. }) => Err(unconfirmed()),
        Err(source) => Err(super::error::fs_error("lock the data authority", source)),
    }
}

/// Puts every journaled state file back as it was before the step.
///
/// The data authority lock is taken first and held for the whole restore; it
/// is released when this returns, before the previous daemon starts. A file
/// that did not exist is removed; any other is rewritten from its verified
/// backup. The directory is created when it is missing.
///
/// # Errors
///
/// Returns [`Error::UpgradeRuntimesUnconfirmed`] while the data authority lock
/// is held, [`Error::RuntimeBackup`] for a backup that does not verify, before
/// anything is written, and filesystem errors for the state directories.
pub fn restore(
    context: &Context,
    daemon: &Path,
    store: &Store,
    backup: &RuntimeBackup,
) -> Result<(), Error> {
    let _authority = acquire_data_authority(context, daemon)?;
    verify_backup(backup, store)?;
    let plugins = context.paths().plugins_dir();
    let agents = context.paths().config_dir.join(AGENTS_DIR);
    for (index, entry) in backup.entries.iter().enumerate() {
        let root = match entry.dir {
            RuntimeDir::Plugins => &plugins,
            RuntimeDir::Agents => &agents,
        };
        let directory = layout::open_owner_dir(root, STATE_DIR_MODE)?;
        if entry.digest.is_none() {
            super::record::remove_file(&directory, &entry.name)?;
        } else {
            let bytes = store.read_runtime_backup(index, settings::RUNTIME_STATE_BYTES)?;
            let temporary = format!(".{}.{}.restore.tmp", entry.name, std::process::id());
            directory
                .replace_file(&entry.name, temporary, &bytes, entry.mode)
                .map_err(|source| {
                    super::error::replace_error("restore a runtime state file", source)
                })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::context::tests::{context, temp_root};

    /// Writes a `pohunekd` script that prints a report of `report_version`
    /// and answers a dry run with `dry_run` (or the request's own flag when
    /// `None`), then exits with `exit_code`.
    fn fake_daemon(dir: &Path, report_version: u32, dry_run: Option<bool>, exit_code: u8) {
        let dry_value = match dry_run {
            Some(true) => "true",
            Some(false) => "false",
            None => "$dry",
        };
        let script = format!(
            "#!/bin/sh\n\
             [ \"$1\" = {ENSURE_RUNTIMES_ARG} ] || exit 9\n\
             if [ \"$2\" = {DRY_RUN_FLAG} ]; then dry=true; else dry=false; fi\n\
             printf '{{\"report_version\":{report_version},\"daemon_version\":\"2.0.0\",\"dry_run\":%s,\"packages\":[],\"catalog\":\"current\",\"profiles\":[],\"integrations\":[],\"refusals\":[]}}\\n' \"{dry_value}\"\n\
             echo 'diagnostic line' >&2\n\
             exit {exit_code}\n"
        );
        let path = dir.join(DAEMON_EXECUTABLE_NAME);
        std::fs::write(&path, script).expect("write fake daemon");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
    }

    fn failure_detail(error: Error) -> String {
        match error {
            Error::UpgradeRuntimesFailed { detail, .. } => detail,
            other => panic!("expected a failed runtime step: {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_daemon_report_is_read_for_a_plan_and_for_an_apply() {
        let (_root, root) = temp_root();
        let context = context(root.as_path());
        fake_daemon(root.as_path(), ENSURE_REPORT_VERSION, None, 0);

        let plan = run_daemon(
            DaemonRuntimes::default(),
            &context,
            root.as_path(),
            true,
            "/usr/bin:/bin",
        )
        .await
        .expect("plan");
        assert!(plan.dry_run && !needs_apply(&plan));
        let applied = run_daemon(
            DaemonRuntimes::default(),
            &context,
            root.as_path(),
            false,
            "/usr/bin:/bin",
        )
        .await
        .expect("apply");
        assert!(!applied.dry_run);
    }

    #[tokio::test]
    async fn the_plan_and_the_apply_run_with_the_path_of_the_job() {
        let (_root, root) = temp_root();
        let context = context(root.as_path());
        let tools = root.join("tools");
        std::fs::create_dir_all(&tools).expect("tools dir");
        std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let search_path = tools.display().to_string();
        fake_daemon(root.as_path(), ENSURE_REPORT_VERSION, None, 0);
        // The daemon refuses to answer unless it has the job's PATH.
        let path = root.join(DAEMON_EXECUTABLE_NAME);
        let body = std::fs::read_to_string(&path).expect("read fake daemon");
        std::fs::write(
            &path,
            body.replacen(
                "#!/bin/sh\n",
                &format!(
                    "#!/bin/sh
[ \"$PATH\" = '{search_path}' ] || exit 7
"
                ),
                1,
            ),
        )
        .expect("write fake daemon");

        run_daemon(
            DaemonRuntimes::default(),
            &context,
            root.as_path(),
            true,
            &search_path,
        )
        .await
        .expect("the plan runs with the job PATH");
        run_daemon(
            DaemonRuntimes::default(),
            &context,
            root.as_path(),
            false,
            &search_path,
        )
        .await
        .expect("the apply runs with the job PATH");
        let detail = failure_detail(
            run_daemon(
                DaemonRuntimes::default(),
                &context,
                root.as_path(),
                true,
                "/usr/bin:/bin",
            )
            .await
            .expect_err("without the job PATH the daemon refuses"),
        );
        assert!(detail.contains("exit status: 7"), "{detail}");
    }

    #[tokio::test]
    async fn a_report_of_another_version_fails_the_step() {
        let (_root, root) = temp_root();
        let context = context(root.as_path());
        fake_daemon(root.as_path(), ENSURE_REPORT_VERSION + 1, Some(true), 0);

        let detail = failure_detail(
            run_daemon(
                DaemonRuntimes::default(),
                &context,
                root.as_path(),
                true,
                "/usr/bin:/bin",
            )
            .await
            .expect_err("the version is refused"),
        );
        assert!(detail.contains("expected 1"), "{detail}");
    }

    #[tokio::test]
    async fn a_report_answering_the_other_request_fails_the_step() {
        let (_root, root) = temp_root();
        let context = context(root.as_path());
        // An apply answered as a plan would let the step skip the change it asked for.
        fake_daemon(root.as_path(), ENSURE_REPORT_VERSION, Some(true), 0);

        let detail = failure_detail(
            run_daemon(
                DaemonRuntimes::default(),
                &context,
                root.as_path(),
                false,
                "/usr/bin:/bin",
            )
            .await
            .expect_err("the dry-run flag is refused"),
        );
        assert!(detail.contains("dry_run=true"), "{detail}");
    }

    #[tokio::test]
    async fn a_nonzero_exit_fails_the_step_with_its_diagnostics() {
        let (_root, root) = temp_root();
        let context = context(root.as_path());
        fake_daemon(root.as_path(), ENSURE_REPORT_VERSION, None, 3);

        let detail = failure_detail(
            run_daemon(
                DaemonRuntimes::default(),
                &context,
                root.as_path(),
                false,
                "/usr/bin:/bin",
            )
            .await
            .expect_err("the exit is refused"),
        );
        assert!(detail.contains("diagnostic line"), "{detail}");
    }

    /// Writes a `pohunekd` that records its pid in `pid_file` and then runs
    /// `sleep`, so an apply never finishes on its own.
    fn hanging_daemon(dir: &Path, pid_file: &Path) {
        let script = format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 600\n",
            pid_file.display()
        );
        let path = dir.join(DAEMON_EXECUTABLE_NAME);
        std::fs::write(&path, script).expect("write hanging daemon");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
    }

    fn short_timeouts(exit_timeout: Duration) -> DaemonRuntimes {
        DaemonRuntimes {
            plan_timeout: Duration::from_secs(10),
            apply_timeout: Duration::from_millis(300),
            exit_timeout,
        }
    }

    #[tokio::test]
    async fn an_apply_that_outlives_its_timeout_is_reaped_before_the_error_returns() {
        let (_root, root) = temp_root();
        let context = context(root.as_path());
        let pid_file = root.as_path().join("pid");
        hanging_daemon(root.as_path(), &pid_file);

        let error = short_timeouts(Duration::from_secs(10))
            .run(&context, root.as_path(), false, "/usr/bin:/bin")
            .await
            .expect_err("the apply does not finish");
        let detail = failure_detail(error);
        assert!(detail.contains("was ended"), "{detail}");

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
            "the daemon is gone and reaped when the error is returned"
        );
    }
}

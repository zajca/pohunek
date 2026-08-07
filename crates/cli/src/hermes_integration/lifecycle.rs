//! Transactional filesystem lifecycle for the Hermes operator plugin.
//!
//! The command layer validates target, policy, and confirmations before calling
//! this module. This module never launches Hermes; `HermesControl` is the
//! narrow boundary the fixed runner implements later.

// Rust guideline compliant 2026-08-07

#![expect(
    clippy::map_err_ignore,
    reason = "transaction errors intentionally collapse sensitive filesystem details"
)]
#![expect(
    clippy::struct_excessive_bools,
    reason = "lifecycle findings and transaction checkpoints are independent contract states"
)]
#![expect(
    clippy::too_many_arguments,
    reason = "the transaction constructor captures one complete atomic rollback record"
)]
#![expect(
    clippy::too_many_lines,
    reason = "the atomic install sequence is kept contiguous so rollback ordering remains auditable"
)]

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Component, Path, PathBuf};

use nix::unistd::Uid;

use super::assets::{self, Asset, Ownership, MARKER_NAME};
use super::error::Error;
use super::policy::Policy;
use super::target::ResolvedTarget;

/// Directories and files created by Pohunek are private to the local owner.
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
/// Immutable assets and the mutable external policy are owner-readable only.
const PRIVATE_FILE_MODE: u32 = 0o600;
/// Group and other bits would let another local account observe or replace assets.
const UNSAFE_PRIVATE_BITS: u32 = 0o077;
/// Stage names are deliberately unmistakable and only inspected below one plugin parent.
const STAGE_PREFIX: &str = ".pohunek-stage-";
/// Backups are retained only while a single update transaction is active.
const BACKUP_PREFIX: &str = ".pohunek-backup-";
/// Bounded collision retries avoid an unbounded loop in an adversarial directory.
const SIBLING_NAME_ATTEMPTS: u32 = 32;
/// The policy staging filename remains sibling-local and is never interpreted by Hermes.
const POLICY_STAGE_PREFIX: &str = ".pohunek-policy-stage-";
/// The policy backup filename remains sibling-local and is never interpreted by Hermes.
const POLICY_BACKUP_PREFIX: &str = ".pohunek-policy-backup-";
/// Status checks inspect only a bounded number of exact Pohunek transaction siblings.
const MAX_STALE_SIBLINGS: usize = 32;

/// Fixed Hermes operations needed by the filesystem lifecycle.
///
/// Implementations must perform no operation beyond validating a staged plugin
/// and querying or changing its fixed enabled state. In production the runner
/// owns this boundary; tests use a controlled in-memory implementation.
pub(crate) trait HermesControl {
    /// Validates the staged plugin with Hermes's fixed import/schema contract.
    fn validate_staged(
        &mut self,
        target: &ResolvedTarget,
        staged_root: &Path,
        staged_policy: &Path,
    ) -> Result<(), Error>;

    /// Reports whether the fixed Pohunek plugin is enabled for this target.
    fn is_enabled(&mut self, target: &ResolvedTarget) -> Result<bool, Error>;

    /// Enables the fixed Pohunek plugin for this target.
    fn enable(&mut self, target: &ResolvedTarget) -> Result<(), Error>;

    /// Disables the fixed Pohunek plugin for this target.
    fn disable(&mut self, target: &ResolvedTarget) -> Result<(), Error>;
}

/// Inputs for one install or update transaction.
#[derive(Debug)]
pub(crate) struct InstallRequest<'a> {
    target: &'a ResolvedTarget,
    policy_path: &'a Path,
    policy: &'a Policy,
    confirm_modified: bool,
}

impl<'a> InstallRequest<'a> {
    /// Creates a request from already validated command-layer inputs.
    #[must_use]
    pub(crate) const fn new(
        target: &'a ResolvedTarget,
        policy_path: &'a Path,
        policy: &'a Policy,
        confirm_modified: bool,
    ) -> Self {
        Self {
            target,
            policy_path,
            policy,
            confirm_modified,
        }
    }
}

/// Inputs for one uninstall transaction.
#[derive(Debug)]
pub(crate) struct UninstallRequest<'a> {
    target: &'a ResolvedTarget,
    policy_path: &'a Path,
    confirm_modified: bool,
}

impl<'a> UninstallRequest<'a> {
    /// Creates a request from already validated command-layer inputs.
    #[must_use]
    pub(crate) const fn new(
        target: &'a ResolvedTarget,
        policy_path: &'a Path,
        confirm_modified: bool,
    ) -> Self {
        Self {
            target,
            policy_path,
            confirm_modified,
        }
    }
}

/// Payload-free lifecycle state for later status and doctor rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LifecycleState {
    /// A valid matching ownership marker exists at the fixed plugin root.
    pub(crate) installed: bool,
    /// Hermes reported the fixed plugin as enabled.
    pub(crate) enabled: bool,
    /// A recorded asset or its private mode differs from the ownership marker.
    pub(crate) modified: bool,
    /// A matching Pohunek stage sibling remains from an interrupted transaction.
    pub(crate) stale_stage: bool,
    /// A matching Pohunek backup sibling remains from an interrupted transaction.
    pub(crate) stale_backup: bool,
}

/// Installs or atomically updates the managed plugin and external policy.
pub(crate) fn install(
    control: &mut impl HermesControl,
    request: &InstallRequest<'_>,
) -> Result<LifecycleState, Error> {
    install_with(control, request, &mut NativeFileOps)
}

fn install_with(
    control: &mut impl HermesControl,
    request: &InstallRequest<'_>,
    files: &mut impl FileOps,
) -> Result<LifecycleState, Error> {
    validate_request_target(request.target, request.policy_path)?;
    let assets = assets::render(request.policy_path)?;
    let desired = assets::ownership(request.target.hermes_home(), request.policy_path, &assets)?;
    let marker = assets::marker_bytes(&desired)?;
    assets::parse_marker(&marker)?;
    let policy_bytes = request.policy.to_json()?;
    let existing = installed(request.target, request.policy_path, &assets)?;
    if existing.as_ref().is_some_and(|value| value.modified) && !request.confirm_modified {
        return Err(Error::ConfirmationRequired);
    }
    if existing.is_none() && path_exists(request.policy_path)? {
        return Err(Error::Collision);
    }
    // Query Hermes before creating a transaction, so a runner failure leaves no residue.
    let was_enabled = control.is_enabled(request.target)?;

    let plugin_parent = request
        .target
        .plugin_root()
        .parent()
        .ok_or(Error::UnsafeTarget)?;
    ensure_private_tree(request.target.hermes_home(), plugin_parent)?;
    let policy_parent = request
        .policy_path
        .parent()
        .ok_or(Error::UnsafePolicyPath)?;
    ensure_private_parent(policy_parent)?;

    let stage_plugin = create_private_sibling(plugin_parent, STAGE_PREFIX)?;
    let stage_policy = match create_private_file_sibling(policy_parent, POLICY_STAGE_PREFIX) {
        Ok(path) => path,
        Err(error) => {
            remove_current_stage(&stage_plugin);
            return Err(error);
        }
    };
    let write_result = write_stage(files, &stage_plugin, &assets, &desired)
        .and_then(|()| files.write_reserved(&stage_policy, &policy_bytes))
        .and_then(|()| {
            existing.as_ref().map_or(Ok(()), |installed| {
                copy_unmanaged_entries(
                    request.target.plugin_root(),
                    &stage_plugin,
                    &installed.ownership,
                )
            })
        })
        .and_then(|()| control.validate_staged(request.target, &stage_plugin, &stage_policy));
    if let Err(error) = write_result {
        remove_current_stage(&stage_plugin);
        remove_owned_file(&stage_policy);
        return Err(error);
    }

    let plugin_backup = match existing
        .as_ref()
        .map(|_| create_private_sibling(plugin_parent, BACKUP_PREFIX))
        .transpose()
    {
        Ok(value) => value,
        Err(error) => {
            remove_current_stage(&stage_plugin);
            remove_owned_file(&stage_policy);
            return Err(error);
        }
    };
    let policy_backup = match existing
        .as_ref()
        .filter(|_| path_exists(request.policy_path).unwrap_or(false))
        .map(|_| create_private_file_sibling(policy_parent, POLICY_BACKUP_PREFIX))
        .transpose()
    {
        Ok(value) => value,
        Err(error) => {
            remove_current_stage(&stage_plugin);
            remove_owned_file(&stage_policy);
            if let Some(path) = plugin_backup.as_deref() {
                remove_owned_directory(path);
            }
            return Err(error);
        }
    };

    let mut transaction = InstallTransaction::new(
        request.target,
        request.policy_path,
        stage_plugin,
        stage_policy,
        plugin_backup,
        policy_backup,
        desired,
        policy_bytes,
        was_enabled,
        existing.as_ref().map(|value| value.ownership.clone()),
    );
    if let Err(error) = transaction.activate(existing.is_some(), files) {
        return transaction.fail(error, control, files);
    }
    if let Err(error) = control.enable(request.target) {
        return transaction.fail(error, control, files);
    }
    transaction.commit(files)?;
    Ok(LifecycleState {
        installed: true,
        enabled: true,
        modified: existing.is_some_and(|value| value.modified),
        stale_stage: false,
        stale_backup: false,
    })
}

/// Removes only the marker-listed plugin files and its exact external policy.
pub(crate) fn uninstall(
    control: &mut impl HermesControl,
    request: &UninstallRequest<'_>,
) -> Result<LifecycleState, Error> {
    uninstall_with(control, request, &mut NativeFileOps)
}

fn uninstall_with(
    control: &mut impl HermesControl,
    request: &UninstallRequest<'_>,
    files: &mut impl FileOps,
) -> Result<LifecycleState, Error> {
    validate_request_target(request.target, request.policy_path)?;
    let assets = assets::render(request.policy_path)?;
    let Some(existing) = installed(request.target, request.policy_path, &assets)? else {
        return inspect(control, request.target, request.policy_path);
    };
    if existing.modified && !request.confirm_modified {
        return Err(Error::ConfirmationRequired);
    }
    if !path_exists(request.policy_path)? {
        return Err(Error::InvalidMarker);
    }
    validate_private_file(request.policy_path, Error::UnsafePolicyPath)?;

    let was_enabled = control.is_enabled(request.target)?;
    if was_enabled {
        control.disable(request.target)?;
    }
    let trash = match create_private_sibling(
        request
            .target
            .plugin_root()
            .parent()
            .ok_or(Error::UnsafeTarget)?,
        STAGE_PREFIX,
    ) {
        Ok(path) => path,
        Err(error) => {
            if was_enabled && control.enable(request.target).is_err() {
                return Err(Error::RecoveryRequired);
            }
            return Err(error);
        }
    };
    let moves = move_managed_to_trash(
        files,
        request.target.plugin_root(),
        request.policy_path,
        &existing.ownership,
        &trash,
    );
    if let Err(failure) = moves {
        let recovery = restore_moves(files, &failure.moves, &trash).and_then(|()| {
            was_enabled
                .then(|| control.enable(request.target))
                .transpose()
        });
        if recovery.is_err() {
            return Err(Error::RecoveryRequired);
        }
        return Err(failure.error);
    }
    files
        .remove_dir_all(&trash)
        .map_err(|_| Error::RecoveryRequired)?;
    Ok(LifecycleState {
        installed: false,
        enabled: false,
        modified: false,
        stale_stage: false,
        stale_backup: false,
    })
}

/// Inspects the fixed managed tree without discovering unrelated Hermes state.
pub(crate) fn inspect(
    control: &mut impl HermesControl,
    target: &ResolvedTarget,
    policy_path: &Path,
) -> Result<LifecycleState, Error> {
    validate_request_target(target, policy_path)?;
    let assets = assets::render(policy_path)?;
    let current = installed(target, policy_path, &assets)?;
    let (stale_stage, stale_backup) = stale_siblings(target, policy_path, &assets)?;
    Ok(LifecycleState {
        installed: current.is_some(),
        enabled: control.is_enabled(target)?,
        modified: current.is_some_and(|value| value.modified),
        stale_stage,
        stale_backup,
    })
}

#[derive(Debug)]
struct Installed {
    ownership: Ownership,
    modified: bool,
}

/// Narrow filesystem seam for transactional writes and moves.
trait FileOps {
    fn write_new(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Error>;
    fn write_reserved(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Error>;
    fn rename(&mut self, source: &Path, destination: &Path) -> Result<(), Error>;
    fn rename_no_replace(&mut self, source: &Path, destination: &Path) -> Result<(), Error>;
    fn remove_file(&mut self, path: &Path) -> Result<(), Error>;
    fn remove_dir_all(&mut self, path: &Path) -> Result<(), Error>;
}

struct NativeFileOps;

impl FileOps for NativeFileOps {
    fn write_new(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Error> {
        write_private_file(path, bytes)
    }

    fn write_reserved(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Error> {
        write_reserved_private_file(path, bytes)
    }

    fn rename(&mut self, source: &Path, destination: &Path) -> Result<(), Error> {
        fs::rename(source, destination).map_err(Into::into)
    }

    fn rename_no_replace(&mut self, source: &Path, destination: &Path) -> Result<(), Error> {
        rename_no_replace(source, destination)
    }

    fn remove_file(&mut self, path: &Path) -> Result<(), Error> {
        fs::remove_file(path).map_err(Into::into)
    }

    fn remove_dir_all(&mut self, path: &Path) -> Result<(), Error> {
        fs::remove_dir_all(path).map_err(Into::into)
    }
}

fn installed(
    target: &ResolvedTarget,
    policy_path: &Path,
    expected_assets: &[Asset],
) -> Result<Option<Installed>, Error> {
    let root = target.plugin_root();
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(Error::Collision)
        }
        Ok(_) => validate_private_directory(root)?,
    }
    let ownership = read_matching_ownership(root, target, policy_path, expected_assets)?;
    let modified = managed_modified(root, &ownership)? || has_unmanaged_entries(root, &ownership)?;
    Ok(Some(Installed {
        ownership,
        modified,
    }))
}

fn validate_request_target(target: &ResolvedTarget, policy_path: &Path) -> Result<(), Error> {
    if !policy_path.is_absolute()
        || policy_path.starts_with(target.hermes_home())
        || policy_path.starts_with(target.plugin_root())
        || !target.plugin_root().starts_with(target.hermes_home())
    {
        return Err(Error::UnsafePolicyPath);
    }
    reject_symlink_components(policy_path, Error::UnsafePolicyPath)?;
    Ok(())
}

fn read_matching_ownership(
    root: &Path,
    target: &ResolvedTarget,
    policy_path: &Path,
    expected_assets: &[Asset],
) -> Result<Ownership, Error> {
    let marker_path = root.join(MARKER_NAME);
    if !path_exists(&marker_path)? {
        return Err(Error::Collision);
    }
    validate_private_file(&marker_path, Error::InvalidMarker)?;
    let ownership = assets::parse_marker(&fs::read(marker_path)?)?;
    let target_text = target.hermes_home().to_str().ok_or(Error::InvalidMarker)?;
    let policy_text = policy_path.to_str().ok_or(Error::InvalidMarker)?;
    let expected: BTreeSet<_> = expected_assets.iter().map(Asset::path).collect();
    let recorded: BTreeSet<_> = ownership.assets.keys().map(String::as_str).collect();
    if ownership.hermes_home != target_text
        || ownership.policy_path != policy_text
        || expected != recorded
    {
        return Err(Error::InvalidMarker);
    }
    Ok(ownership)
}

fn managed_modified(root: &Path, ownership: &Ownership) -> Result<bool, Error> {
    for (relative, checksum) in &ownership.assets {
        let path = root.join(relative);
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(Error::UnsafeTarget);
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error.into()),
        };
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file()
            || metadata.uid() != Uid::effective().as_raw()
            || metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE
            || assets::checksum(&bytes) != *checksum
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_unmanaged_entries(root: &Path, ownership: &Ownership) -> Result<bool, Error> {
    let known: BTreeSet<_> = ownership
        .assets
        .keys()
        .map(PathBuf::from)
        .chain(std::iter::once(PathBuf::from(MARKER_NAME)))
        .collect();
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    Ok(files.into_iter().any(|path| !known.contains(&path)))
}

fn copy_unmanaged_entries(root: &Path, stage: &Path, ownership: &Ownership) -> Result<(), Error> {
    let known: BTreeSet<_> = ownership
        .assets
        .keys()
        .map(PathBuf::from)
        .chain(std::iter::once(PathBuf::from(MARKER_NAME)))
        .collect();
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    for relative in files.into_iter().filter(|path| !known.contains(path)) {
        let source = root.join(&relative);
        let destination = stage.join(&relative);
        let parent = destination.parent().ok_or(Error::UnsafeTarget)?;
        ensure_private_tree(stage, parent)?;
        let metadata = fs::metadata(&source)?;
        if metadata.uid() != Uid::effective().as_raw() {
            return Err(Error::UnsafePermissions);
        }
        let mut input = fs::File::open(source)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&destination)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        fs::set_permissions(&destination, metadata.permissions())?;
    }
    Ok(())
}

fn collect_files(root: &Path, current: &Path, files: &mut Vec<PathBuf>) -> Result<(), Error> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(Error::UnsafeTarget);
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| Error::UnsafeTarget)?
            .to_owned();
        if metadata.is_dir() {
            collect_files(root, &path, files)?;
        } else if metadata.is_file() {
            files.push(relative);
        } else {
            return Err(Error::UnsafeTarget);
        }
    }
    Ok(())
}

fn write_stage(
    files: &mut impl FileOps,
    stage: &Path,
    assets: &[Asset],
    ownership: &Ownership,
) -> Result<(), Error> {
    for asset in assets {
        let path = stage.join(asset.path());
        let parent = path.parent().ok_or(Error::InvalidAsset)?;
        ensure_private_tree(stage, parent)?;
        files.write_new(&path, asset.bytes())?;
    }
    files.write_new(&stage.join(MARKER_NAME), &assets::marker_bytes(ownership)?)
}

/// Records only moves completed by one installation transaction.
struct InstallTransaction<'a> {
    target: &'a ResolvedTarget,
    policy_path: &'a Path,
    stage_plugin: PathBuf,
    stage_policy: PathBuf,
    plugin_backup: Option<PathBuf>,
    policy_backup: Option<PathBuf>,
    desired: Ownership,
    old_ownership: Option<Ownership>,
    policy_bytes: Vec<u8>,
    was_enabled: bool,
    old_plugin_moved: bool,
    old_policy_moved: bool,
    new_policy_moved: bool,
    new_plugin_moved: bool,
}

impl<'a> InstallTransaction<'a> {
    fn new(
        target: &'a ResolvedTarget,
        policy_path: &'a Path,
        stage_plugin: PathBuf,
        stage_policy: PathBuf,
        plugin_backup: Option<PathBuf>,
        policy_backup: Option<PathBuf>,
        desired: Ownership,
        policy_bytes: Vec<u8>,
        was_enabled: bool,
        old_ownership: Option<Ownership>,
    ) -> Self {
        Self {
            target,
            policy_path,
            stage_plugin,
            stage_policy,
            plugin_backup,
            policy_backup,
            desired,
            old_ownership,
            policy_bytes,
            was_enabled,
            old_plugin_moved: false,
            old_policy_moved: false,
            new_policy_moved: false,
            new_plugin_moved: false,
        }
    }

    fn activate(&mut self, replacing: bool, files: &mut impl FileOps) -> Result<(), Error> {
        if replacing {
            let backup = self.plugin_backup.as_deref().ok_or(Error::Collision)?;
            files.rename(self.target.plugin_root(), backup)?;
            self.old_plugin_moved = true;
            if path_exists(self.policy_path)? {
                files.rename(
                    self.policy_path,
                    self.policy_backup.as_deref().ok_or(Error::Collision)?,
                )?;
                self.old_policy_moved = true;
            }
        } else if path_exists(self.target.plugin_root())? || path_exists(self.policy_path)? {
            return Err(Error::Collision);
        }
        files.rename_no_replace(&self.stage_policy, self.policy_path)?;
        self.new_policy_moved = true;
        files.rename_no_replace(&self.stage_plugin, self.target.plugin_root())?;
        self.new_plugin_moved = true;
        Ok(())
    }

    fn commit(&mut self, files: &mut impl FileOps) -> Result<(), Error> {
        if let Some(path) = self.plugin_backup.as_deref() {
            if let Some(ownership) = self.old_ownership.as_ref() {
                if !matches_ownership(path, ownership)? {
                    return Err(Error::RecoveryRequired);
                }
                files
                    .remove_dir_all(path)
                    .map_err(|_| Error::RecoveryRequired)?;
            }
        }
        if let Some(path) = self.policy_backup.as_deref() {
            if path_exists(path)? {
                files
                    .remove_file(path)
                    .map_err(|_| Error::RecoveryRequired)?;
            }
        }
        Ok(())
    }

    fn fail(
        &mut self,
        original: Error,
        control: &mut impl HermesControl,
        files: &mut impl FileOps,
    ) -> Result<LifecycleState, Error> {
        self.rollback(control, files)
            .map_err(|_| Error::RecoveryRequired)?;
        Err(original)
    }

    fn rollback(
        &mut self,
        control: &mut impl HermesControl,
        files: &mut impl FileOps,
    ) -> Result<(), Error> {
        // A failed fresh/disabled install may have enabled the newly activated
        // plugin before returning an error. Disable and verify it while the
        // discoverable plugin still exists; pinned Hermes rejects disabling a
        // missing plugin. Activation failures before this move need no state
        // operation because Hermes could not have observed the new plugin.
        if !self.was_enabled && self.new_plugin_moved {
            control.disable(self.target)?;
        }
        if self.new_plugin_moved {
            if !matches_ownership(self.target.plugin_root(), &self.desired)? {
                return Err(Error::RecoveryRequired);
            }
            files.rename(self.target.plugin_root(), &self.stage_plugin)?;
            self.new_plugin_moved = false;
        }
        if self.new_policy_moved {
            if fs::read(self.policy_path)? != self.policy_bytes {
                return Err(Error::RecoveryRequired);
            }
            files.rename(self.policy_path, &self.stage_policy)?;
            self.new_policy_moved = false;
        }
        if self.old_policy_moved {
            files.rename(
                self.policy_backup
                    .as_deref()
                    .ok_or(Error::RecoveryRequired)?,
                self.policy_path,
            )?;
            self.old_policy_moved = false;
        }
        if self.old_plugin_moved {
            files.rename(
                self.plugin_backup
                    .as_deref()
                    .ok_or(Error::RecoveryRequired)?,
                self.target.plugin_root(),
            )?;
            self.old_plugin_moved = false;
        }
        remove_current_stage(&self.stage_plugin);
        remove_owned_file(&self.stage_policy);
        if let Some(path) = self.plugin_backup.as_deref() {
            remove_owned_directory(path);
        }
        if let Some(path) = self.policy_backup.as_deref() {
            remove_owned_file(path);
        }
        if self.was_enabled {
            control.enable(self.target)?;
        }
        Ok(())
    }
}

#[expect(
    unsafe_code,
    reason = "renameat2 is the Linux kernel boundary for atomic no-replace moves"
)]
fn rename_no_replace(source: &Path, destination: &Path) -> Result<(), Error> {
    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| Error::Io {
        kind: std::io::ErrorKind::InvalidInput,
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| Error::Io {
        kind: std::io::ErrorKind::InvalidInput,
    })?;
    // SAFETY: both path pointers reference owned NUL-terminated strings for the
    // duration of the call; directory descriptors and flags are scalar values.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EEXIST) {
        Err(Error::Collision)
    } else {
        Err(Error::Io { kind: error.kind() })
    }
}

fn matches_ownership(root: &Path, expected: &Ownership) -> Result<bool, Error> {
    let marker = root.join(MARKER_NAME);
    let bytes = match fs::read(marker) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(assets::parse_marker(&bytes).is_ok_and(|ownership| ownership == *expected))
}

#[derive(Debug)]
struct MoveRecord {
    source: PathBuf,
    destination: PathBuf,
}

struct MoveFailure {
    error: Error,
    moves: Vec<MoveRecord>,
}

fn move_managed_to_trash(
    files: &mut impl FileOps,
    root: &Path,
    policy_path: &Path,
    ownership: &Ownership,
    trash: &Path,
) -> Result<Vec<MoveRecord>, MoveFailure> {
    let mut moves = Vec::with_capacity(ownership.assets.len() + 2);
    for relative in ownership.assets.keys() {
        let source = root.join(relative);
        let destination = trash.join("plugin").join(relative);
        let Some(parent) = destination.parent() else {
            return Err(MoveFailure {
                error: Error::InvalidMarker,
                moves,
            });
        };
        if let Err(error) = ensure_private_tree(trash, parent) {
            return Err(MoveFailure { error, moves });
        }
        if let Err(error) = files.rename(&source, &destination) {
            return Err(MoveFailure { error, moves });
        }
        moves.push(MoveRecord {
            source,
            destination,
        });
    }
    let marker = root.join(MARKER_NAME);
    let marker_destination = trash.join("plugin").join(MARKER_NAME);
    let Some(marker_parent) = marker_destination.parent() else {
        return Err(MoveFailure {
            error: Error::InvalidMarker,
            moves,
        });
    };
    if let Err(error) = ensure_private_tree(trash, marker_parent) {
        return Err(MoveFailure { error, moves });
    }
    if let Err(error) = files.rename(&marker, &marker_destination) {
        return Err(MoveFailure { error, moves });
    }
    moves.push(MoveRecord {
        source: marker,
        destination: marker_destination,
    });
    let policy_destination = trash.join("policy");
    if let Err(error) = files.rename(policy_path, &policy_destination) {
        return Err(MoveFailure { error, moves });
    }
    moves.push(MoveRecord {
        source: policy_path.to_owned(),
        destination: policy_destination,
    });
    if let Err(error) = remove_empty_managed_directories(root) {
        return Err(MoveFailure { error, moves });
    }
    Ok(moves)
}

fn restore_moves(
    files: &mut impl FileOps,
    moves: &[MoveRecord],
    trash: &Path,
) -> Result<(), Error> {
    for move_record in moves.iter().rev() {
        let parent = move_record.source.parent().ok_or(Error::RecoveryRequired)?;
        if !parent.exists() {
            ensure_private_parent(parent)?;
        }
        if !path_exists(&move_record.destination)? {
            return Err(Error::RecoveryRequired);
        }
        files.rename_no_replace(&move_record.destination, &move_record.source)?;
    }
    files.remove_dir_all(trash)?;
    Ok(())
}

fn remove_empty_managed_directories(root: &Path) -> Result<(), Error> {
    let mut directories = Vec::new();
    collect_directories(root, root, &mut directories)?;
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for relative in directories {
        let path = root.join(relative);
        if fs::read_dir(&path)?.next().is_none() {
            fs::remove_dir(path)?;
        }
    }
    if fs::read_dir(root)?.next().is_none() {
        fs::remove_dir(root)?;
    }
    Ok(())
}

fn collect_directories(
    root: &Path,
    current: &Path,
    output: &mut Vec<PathBuf>,
) -> Result<(), Error> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(Error::UnsafeTarget);
        }
        if metadata.is_dir() {
            collect_directories(root, &path, output)?;
            output.push(
                path.strip_prefix(root)
                    .map_err(|_| Error::UnsafeTarget)?
                    .to_owned(),
            );
        }
    }
    Ok(())
}

fn stale_siblings(
    target: &ResolvedTarget,
    policy_path: &Path,
    assets: &[Asset],
) -> Result<(bool, bool), Error> {
    let Some(parent) = target.plugin_root().parent() else {
        return Err(Error::UnsafeTarget);
    };
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((false, false)),
        Err(error) => return Err(error.into()),
    };
    let mut stage = false;
    let mut backup = false;
    for entry in entries.take(MAX_STALE_SIBLINGS) {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_stage = name.starts_with(STAGE_PREFIX);
        let is_backup = name.starts_with(BACKUP_PREFIX);
        if !(is_stage || is_backup) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir()
            && validate_private_directory(&path).is_ok()
            && (read_matching_ownership(&path, target, policy_path, assets).is_ok() || is_stage)
        {
            stage |= is_stage;
            backup |= is_backup;
        }
    }
    let policy_parent = policy_path.parent().ok_or(Error::UnsafePolicyPath)?;
    if let Ok(entries) = fs::read_dir(policy_parent) {
        for entry in entries.take(MAX_STALE_SIBLINGS) {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with(POLICY_STAGE_PREFIX) || name.starts_with(POLICY_BACKUP_PREFIX)) {
                continue;
            }
            let path = entry.path();
            if validate_private_file(&path, Error::UnsafePolicyPath).is_ok() {
                stage |= name.starts_with(POLICY_STAGE_PREFIX);
                backup |= name.starts_with(POLICY_BACKUP_PREFIX);
            }
        }
    }
    Ok((stage, backup))
}

fn ensure_private_parent(path: &Path) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(Error::UnsafePolicyPath);
    }
    let existing = nearest_existing(path)?;
    validate_private_directory(&existing)?;
    let tail = path
        .strip_prefix(&existing)
        .map_err(|_| Error::UnsafePolicyPath)?;
    let mut current = existing;
    for component in tail.components() {
        let Component::Normal(component) = component else {
            return Err(Error::UnsafePolicyPath);
        };
        current.push(component);
        create_or_validate_private_directory(&current)?;
    }
    Ok(())
}

fn ensure_private_tree(base: &Path, leaf: &Path) -> Result<(), Error> {
    if !leaf.starts_with(base) {
        return Err(Error::UnsafeTarget);
    }
    if !path_exists(base)? {
        ensure_private_parent(base)?;
    }
    validate_private_directory(base)?;
    let tail = leaf.strip_prefix(base).map_err(|_| Error::UnsafeTarget)?;
    let mut current = base.to_owned();
    for component in tail.components() {
        let Component::Normal(component) = component else {
            return Err(Error::UnsafeTarget);
        };
        current.push(component);
        create_or_validate_private_directory(&current)?;
    }
    Ok(())
}

fn create_or_validate_private_directory(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::UnsafeTarget),
        Ok(_) => validate_private_directory(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(PRIVATE_DIRECTORY_MODE)
                .create(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))?;
            validate_private_directory(path)
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_private_directory(path: &Path) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != Uid::effective().as_raw()
        || metadata.permissions().mode() & UNSAFE_PRIVATE_BITS != 0
    {
        return Err(Error::UnsafePermissions);
    }
    Ok(())
}

fn validate_private_file(path: &Path, error: Error) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path).map_err(|_| error.clone())?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE
    {
        return Err(error);
    }
    Ok(())
}

fn create_private_sibling(parent: &Path, prefix: &str) -> Result<PathBuf, Error> {
    for attempt in 0..SIBLING_NAME_ATTEMPTS {
        let path = parent.join(format!("{prefix}{}-{attempt}", std::process::id()));
        match fs::DirBuilder::new()
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&path)
        {
            Ok(()) => {
                fs::set_permissions(&path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::Collision)
}

fn create_private_file_sibling(parent: &Path, prefix: &str) -> Result<PathBuf, Error> {
    for attempt in 0..SIBLING_NAME_ATTEMPTS {
        let path = parent.join(format!("{prefix}{}-{attempt}", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&path)
        {
            Ok(_file) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::Collision)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    Ok(())
}

fn write_reserved_private_file(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    Ok(())
}

fn nearest_existing(path: &Path) -> Result<PathBuf, Error> {
    let mut current = path;
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::UnsafePolicyPath)
            }
            Ok(_) => return Ok(current.to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current = current.parent().ok_or(Error::UnsafePolicyPath)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn reject_symlink_components(path: &Path, error: Error) -> Result<(), Error> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(error),
            Ok(_) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(error),
        }
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn remove_owned_directory(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn remove_current_stage(path: &Path) {
    let current = path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(STAGE_PREFIX));
    if current && validate_private_directory(path).is_ok() {
        let _ = fs::remove_dir_all(path);
    }
}

fn remove_owned_file(path: &Path) {
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::hermes_integration::policy::{
        AccessMode, PolicyInput, WildcardConfirmation, MAX_CONCURRENCY, MAX_OUTPUT_BYTES,
        MAX_SCREEN_BYTES, MAX_TIMEOUT_MS,
    };
    use crate::hermes_integration::runner::HermesRunner;
    use crate::hermes_integration::target::{ProfileName, TargetContext, TargetSelection};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(tag: &str) -> Fixture {
        loop {
            let path = crate::hermes_integration::target::isolated_test_temp_root().join(format!(
                "pohunek-hermes-lifecycle-{tag}-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                        .expect("private fixture");
                    return Fixture(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create fixture: {error}"),
            }
        }
    }

    fn setup(tag: &str) -> (Fixture, ResolvedTarget, PathBuf, Policy) {
        let root = fixture(tag);
        for name in ["hermes", "home", "workspace", "config"] {
            let path = root.0.join(name);
            fs::create_dir(&path).expect("fixture directory");
            fs::set_permissions(&path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                .expect("private directory");
        }
        let cli = root.0.join("pohunek");
        fs::write(&cli, b"#!/bin/sh\nexit 0\n").expect("cli");
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).expect("executable");
        let context = TargetContext::new(
            root.0.join("hermes"),
            root.0.join("home"),
            vec![root.0.join("workspace")],
        )
        .expect("context");
        let target = context
            .resolve(TargetSelection::Profile(ProfileName::default()))
            .expect("target");
        let policy = Policy::new(PolicyInput {
            pohunek_cli: cli,
            protocol_min: 1,
            protocol_max: 2,
            access_mode: AccessMode::Manage,
            allowed_hosts: vec!["local".to_owned()],
            tool_timeout_ms: MAX_TIMEOUT_MS,
            max_output_bytes: MAX_OUTPUT_BYTES,
            max_screen_bytes: MAX_SCREEN_BYTES,
            max_concurrency: MAX_CONCURRENCY,
            wildcard_confirmation: WildcardConfirmation::new(false),
        })
        .expect("policy");
        let policy_path = root.0.join("config/policy.json");
        (root, target, policy_path, policy)
    }

    fn write_executable(path: &Path, body: &str) -> PathBuf {
        fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write executable");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("executable mode");
        path.to_owned()
    }

    #[derive(Default)]
    struct ControlledHermes {
        enabled: bool,
        fail_validation: bool,
        fail_status: bool,
        fail_status_after_first: bool,
        status_calls: usize,
        fail_enable: bool,
        fail_disable: bool,
        saw_staged_policy: bool,
    }

    impl HermesControl for ControlledHermes {
        fn validate_staged(
            &mut self,
            _: &ResolvedTarget,
            _: &Path,
            staged_policy: &Path,
        ) -> Result<(), Error> {
            self.saw_staged_policy = staged_policy.is_file();
            (!self.fail_validation)
                .then_some(())
                .ok_or(Error::StagedValidation)
        }

        fn is_enabled(&mut self, _: &ResolvedTarget) -> Result<bool, Error> {
            self.status_calls += 1;
            (!(self.fail_status || self.fail_status_after_first && self.status_calls > 1))
                .then_some(self.enabled)
                .ok_or(Error::HermesCommand)
        }

        fn enable(&mut self, _: &ResolvedTarget) -> Result<(), Error> {
            if self.fail_enable {
                Err(Error::HermesCommand)
            } else {
                self.enabled = true;
                Ok(())
            }
        }

        fn disable(&mut self, _: &ResolvedTarget) -> Result<(), Error> {
            if self.fail_disable {
                Err(Error::HermesCommand)
            } else {
                self.enabled = false;
                Ok(())
            }
        }
    }

    struct FailingFileOps {
        failure_at: usize,
        second_failure_at: Option<usize>,
        calls: usize,
        native: NativeFileOps,
    }

    impl FailingFileOps {
        fn new(failure_at: usize) -> Self {
            Self {
                failure_at,
                second_failure_at: None,
                calls: 0,
                native: NativeFileOps,
            }
        }

        fn with_two_failures(first: usize, second: usize) -> Self {
            Self {
                failure_at: first,
                second_failure_at: Some(second),
                calls: 0,
                native: NativeFileOps,
            }
        }

        fn before(&mut self) -> Result<(), Error> {
            self.calls += 1;
            (self.calls != self.failure_at && self.second_failure_at != Some(self.calls))
                .then_some(())
                .ok_or(Error::Io {
                    kind: std::io::ErrorKind::Other,
                })
        }
    }

    impl FileOps for FailingFileOps {
        fn write_new(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Error> {
            self.before()?;
            self.native.write_new(path, bytes)
        }

        fn write_reserved(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Error> {
            self.before()?;
            self.native.write_reserved(path, bytes)
        }

        fn rename(&mut self, source: &Path, destination: &Path) -> Result<(), Error> {
            self.before()?;
            self.native.rename(source, destination)
        }

        fn rename_no_replace(&mut self, source: &Path, destination: &Path) -> Result<(), Error> {
            self.before()?;
            self.native.rename_no_replace(source, destination)
        }

        fn remove_file(&mut self, path: &Path) -> Result<(), Error> {
            self.before()?;
            self.native.remove_file(path)
        }

        fn remove_dir_all(&mut self, path: &Path) -> Result<(), Error> {
            self.before()?;
            self.native.remove_dir_all(path)
        }
    }

    struct FreshCollisionFileOps {
        native: NativeFileOps,
        collision_path: PathBuf,
        no_replace_calls: usize,
    }

    impl FileOps for FreshCollisionFileOps {
        fn write_new(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Error> {
            self.native.write_new(path, bytes)
        }

        fn write_reserved(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Error> {
            self.native.write_reserved(path, bytes)
        }

        fn rename(&mut self, source: &Path, destination: &Path) -> Result<(), Error> {
            self.native.rename(source, destination)
        }

        fn rename_no_replace(&mut self, source: &Path, destination: &Path) -> Result<(), Error> {
            self.no_replace_calls += 1;
            if self.no_replace_calls == 2 {
                fs::create_dir(&self.collision_path).map_err(Error::from)?;
                fs::set_permissions(
                    &self.collision_path,
                    fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
                )?;
            }
            self.native.rename_no_replace(source, destination)
        }

        fn remove_file(&mut self, path: &Path) -> Result<(), Error> {
            self.native.remove_file(path)
        }

        fn remove_dir_all(&mut self, path: &Path) -> Result<(), Error> {
            self.native.remove_dir_all(path)
        }
    }

    #[test]
    fn resolver_target_metadata_round_trips_through_canonical_marker_parser() {
        let (_root, target, policy_path, _policy) = setup("marker-roundtrip");
        let rendered = assets::render(&policy_path).expect("rendered assets");
        let ownership =
            assets::ownership(target.hermes_home(), &policy_path, &rendered).expect("ownership");
        let marker = assets::marker_bytes(&ownership).expect("marker bytes");
        assert_eq!(
            assets::parse_marker(&marker).expect("canonical marker parse"),
            ownership
        );
    }

    #[test]
    fn staged_marker_round_trips_through_canonical_parser() {
        let (root, target, policy_path, _policy) = setup("staged-marker");
        let rendered = assets::render(&policy_path).expect("rendered assets");
        let ownership =
            assets::ownership(target.hermes_home(), &policy_path, &rendered).expect("ownership");
        let stage = root.0.join("stage");
        fs::create_dir(&stage).expect("stage directory");
        fs::set_permissions(&stage, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
            .expect("private stage");
        write_stage(&mut NativeFileOps, &stage, &rendered, &ownership).expect("write stage");
        let marker = fs::read(stage.join(MARKER_NAME)).expect("marker bytes");
        assert_eq!(
            assets::parse_marker(&marker).expect("canonical staged marker parse"),
            ownership
        );
    }

    #[test]
    fn activated_marker_retains_private_mode_and_canonical_contents() {
        let (root, target, policy_path, _policy) = setup("activated-marker");
        let rendered = assets::render(&policy_path).expect("rendered assets");
        let ownership =
            assets::ownership(target.hermes_home(), &policy_path, &rendered).expect("ownership");
        let parent = target.plugin_root().parent().expect("plugin parent");
        ensure_private_tree(target.hermes_home(), parent).expect("private parent");
        let stage = root.0.join("stage");
        fs::create_dir(&stage).expect("stage directory");
        fs::set_permissions(&stage, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
            .expect("private stage");
        write_stage(&mut NativeFileOps, &stage, &rendered, &ownership).expect("write stage");
        fs::rename(stage, target.plugin_root()).expect("activate stage");
        let marker = target.plugin_root().join(MARKER_NAME);
        validate_private_file(&marker, Error::InvalidMarker).expect("private marker");
        assert_eq!(
            assets::parse_marker(&fs::read(marker).expect("marker bytes"))
                .expect("canonical activated marker parse"),
            ownership
        );
    }

    #[test]
    fn installs_idempotently_and_reports_private_modes() {
        let (_root, target, policy_path, policy) = setup("install");
        let request = InstallRequest::new(&target, &policy_path, &policy, false);
        let mut hermes = ControlledHermes::default();
        let first = install(&mut hermes, &request).expect("install");
        assert!(hermes.saw_staged_policy);
        assert_eq!(
            first,
            LifecycleState {
                installed: true,
                enabled: true,
                modified: false,
                stale_stage: false,
                stale_backup: false,
            }
        );
        assert_eq!(
            fs::metadata(target.plugin_root())
                .expect("plugin root")
                .permissions()
                .mode()
                & 0o777,
            PRIVATE_DIRECTORY_MODE
        );
        assert_eq!(
            fs::metadata(&policy_path)
                .expect("policy")
                .permissions()
                .mode()
                & 0o777,
            PRIVATE_FILE_MODE
        );
        assert!(!install(&mut hermes, &request).expect("idempotent").modified);
    }

    #[test]
    fn update_requires_confirmation_for_modified_assets_and_replaces_policy() {
        let (_root, target, policy_path, policy) = setup("update");
        let mut hermes = ControlledHermes::default();
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("install");
        fs::write(target.plugin_root().join("tools.py"), b"modified").expect("modify asset");
        assert_eq!(
            install(
                &mut hermes,
                &InstallRequest::new(&target, &policy_path, &policy, false),
            ),
            Err(Error::ConfirmationRequired)
        );
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, true),
        )
        .expect("confirmed update");
        assert!(fs::read(target.plugin_root().join("tools.py"))
            .expect("tools")
            .starts_with(b"\"\"\""));
    }

    #[test]
    fn confirmed_update_preserves_unrelated_owner_file() {
        let (_root, target, policy_path, policy) = setup("update-unrelated");
        let mut hermes = ControlledHermes::default();
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("install");
        let note = target.plugin_root().join("operator-note.txt");
        fs::write(&note, b"retain this operator note").expect("unrelated file");
        fs::set_permissions(&note, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .expect("private unrelated file");
        assert_eq!(
            install(
                &mut hermes,
                &InstallRequest::new(&target, &policy_path, &policy, false),
            ),
            Err(Error::ConfirmationRequired)
        );
        let state = install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, true),
        )
        .expect("confirmed update");
        assert!(state.modified);
        assert_eq!(
            fs::read(note).expect("preserved note"),
            b"retain this operator note"
        );
    }

    #[test]
    fn update_replaces_the_external_policy_for_an_explicit_access_mode_change() {
        let (root, target, policy_path, policy) = setup("access-mode-update");
        let mut hermes = ControlledHermes::default();
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("initial install");
        let full = Policy::new(PolicyInput {
            pohunek_cli: root.0.join("pohunek"),
            protocol_min: 1,
            protocol_max: 2,
            access_mode: AccessMode::Full,
            allowed_hosts: vec!["local".to_owned()],
            tool_timeout_ms: MAX_TIMEOUT_MS,
            max_output_bytes: MAX_OUTPUT_BYTES,
            max_screen_bytes: MAX_SCREEN_BYTES,
            max_concurrency: MAX_CONCURRENCY,
            wildcard_confirmation: WildcardConfirmation::new(false),
        })
        .expect("full policy");
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &full, false),
        )
        .expect("access mode update");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&policy_path).expect("policy")).expect("json");
        assert_eq!(value["access_mode"], "full");
    }

    #[test]
    fn installs_into_missing_custom_home_with_private_components() {
        let root = fixture("custom-home");
        for name in ["hermes", "home", "workspace", "config"] {
            let path = root.0.join(name);
            fs::create_dir(&path).expect("fixture directory");
            fs::set_permissions(&path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                .expect("private fixture directory");
        }
        let cli = root.0.join("pohunek");
        fs::write(&cli, b"#!/bin/sh\nexit 0\n").expect("cli");
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).expect("executable");
        let context = TargetContext::new(
            root.0.join("hermes"),
            root.0.join("home"),
            vec![root.0.join("workspace")],
        )
        .expect("context");
        let target = context
            .resolve(TargetSelection::CustomHome(
                root.0.join("custom/missing/hermes"),
            ))
            .expect("missing custom target");
        let policy_path = root.0.join("config/policy.json");
        let policy = Policy::new(PolicyInput {
            pohunek_cli: cli,
            protocol_min: 1,
            protocol_max: 2,
            access_mode: AccessMode::ReadOnly,
            allowed_hosts: vec!["local".to_owned()],
            tool_timeout_ms: MAX_TIMEOUT_MS,
            max_output_bytes: MAX_OUTPUT_BYTES,
            max_screen_bytes: MAX_SCREEN_BYTES,
            max_concurrency: MAX_CONCURRENCY,
            wildcard_confirmation: WildcardConfirmation::new(false),
        })
        .expect("policy");
        install(
            &mut ControlledHermes::default(),
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("install custom target");
        for path in [
            target.hermes_home(),
            target.hermes_home().parent().expect("custom parent"),
            target.plugin_root(),
        ] {
            assert_eq!(
                fs::metadata(path)
                    .expect("created private path")
                    .permissions()
                    .mode()
                    & 0o777,
                PRIVATE_DIRECTORY_MODE
            );
        }
    }

    #[test]
    fn rejects_collision_and_marker_policy_mismatch() {
        let (_root, target, policy_path, policy) = setup("collision");
        let parent = target.plugin_root().parent().expect("parent");
        fs::create_dir_all(parent).expect("parent");
        for path in [target.hermes_home(), parent] {
            fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                .expect("private");
        }
        fs::create_dir(target.plugin_root()).expect("collision root");
        fs::set_permissions(
            target.plugin_root(),
            fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .expect("private collision");
        assert_eq!(
            install(
                &mut ControlledHermes::default(),
                &InstallRequest::new(&target, &policy_path, &policy, false),
            ),
            Err(Error::Collision)
        );
    }

    #[test]
    fn rejects_a_valid_marker_bound_to_a_different_policy_path() {
        let (_root, target, policy_path, policy) = setup("marker-policy-mismatch");
        let mut hermes = ControlledHermes::default();
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("install");
        let assets = assets::render(&policy_path).expect("assets");
        let wrong = policy_path
            .parent()
            .expect("policy parent")
            .join("other.json");
        let ownership =
            assets::ownership(target.hermes_home(), &wrong, &assets).expect("ownership");
        fs::write(
            target.plugin_root().join(MARKER_NAME),
            assets::marker_bytes(&ownership).expect("marker"),
        )
        .expect("replace marker");
        assert_eq!(
            inspect(&mut hermes, &target, &policy_path),
            Err(Error::InvalidMarker)
        );
    }

    #[test]
    fn plugin_root_symlink_is_rejected_without_following_it() {
        let (root, target, policy_path, policy) = setup("plugin-symlink");
        let parent = target.plugin_root().parent().expect("plugin parent");
        ensure_private_tree(target.hermes_home(), parent).expect("private parent");
        let outside = root.0.join("outside");
        fs::create_dir(&outside).expect("outside directory");
        fs::set_permissions(&outside, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
            .expect("private outside");
        symlink(&outside, target.plugin_root()).expect("plugin symlink");
        assert_eq!(
            install(
                &mut ControlledHermes::default(),
                &InstallRequest::new(&target, &policy_path, &policy, false),
            ),
            Err(Error::Collision)
        );
        assert!(outside.is_dir());
    }

    #[test]
    fn fresh_activation_collision_created_before_no_replace_keeps_unknown_directory() {
        let (_root, target, policy_path, policy) = setup("fresh-activation-collision");
        let mut hermes = ControlledHermes::default();
        let mut files = FreshCollisionFileOps {
            native: NativeFileOps,
            collision_path: target.plugin_root().to_owned(),
            no_replace_calls: 0,
        };
        assert_eq!(
            install_with(
                &mut hermes,
                &InstallRequest::new(&target, &policy_path, &policy, false),
                &mut files,
            ),
            Err(Error::Collision)
        );
        assert!(target.plugin_root().is_dir());
        assert!(!policy_path.exists());
        assert!(!hermes.enabled);
    }

    #[test]
    fn rename_no_replace_rejects_interior_nul_paths() {
        let invalid = Path::new(std::ffi::OsStr::from_bytes(b"invalid\0path"));
        let valid = Path::new("valid");

        for (source, destination) in [(invalid, valid), (valid, invalid)] {
            assert_eq!(
                rename_no_replace(source, destination),
                Err(Error::Io {
                    kind: std::io::ErrorKind::InvalidInput,
                })
            );
        }
    }

    #[test]
    fn rolls_back_after_validation_or_enable_failure() {
        let (_root, target, policy_path, policy) = setup("rollback");
        let request = InstallRequest::new(&target, &policy_path, &policy, false);
        let mut invalid = ControlledHermes {
            fail_validation: true,
            ..ControlledHermes::default()
        };
        assert_eq!(
            install(&mut invalid, &request),
            Err(Error::StagedValidation)
        );
        assert!(!target.plugin_root().exists());
        assert!(!policy_path.exists());
        let mut failed_enable = ControlledHermes {
            fail_enable: true,
            ..ControlledHermes::default()
        };
        assert_eq!(
            install(&mut failed_enable, &request),
            Err(Error::HermesCommand)
        );
        assert!(!target.plugin_root().exists());
        assert!(!policy_path.exists());
    }

    #[test]
    fn fresh_runner_enable_failure_disables_before_removing_plugin() {
        let (root, target, policy_path, policy) = setup("runner-fresh-rollback");
        let binary_directory = root.0.join("installation/venv/bin");
        fs::create_dir_all(&binary_directory).expect("runner directories");
        for path in [
            root.0.join("installation"),
            root.0.join("installation/venv"),
            binary_directory.clone(),
        ] {
            fs::set_permissions(&path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                .expect("private runner directory");
        }

        let state = root.0.join("hermes-state");
        fs::write(&state, b"disabled\n").expect("initial state");
        fs::set_permissions(&state, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .expect("private state");
        let plugin_root = target.plugin_root();
        let hermes_body = format!(
            r#"case "$*" in
  --version)
    printf '%s\n' 'Hermes Agent v0.20.0 (2026.8.3)'
    ;;
  'plugins list --json')
    if [ ! -d '{plugin_root}' ]; then
      printf '%s\n' '[]'
      exit 0
    fi
    IFS= read -r state < '{state}'
    printf '[{{"name":"pohunek","status":"%s","source":"user"}}]\n' "$state"
    ;;
  'plugins enable pohunek --no-allow-tool-override')
    test -d '{plugin_root}'
    printf '%s\n' enabled > '{state}'
    exit 23
    ;;
  'plugins disable pohunek')
    test -d '{plugin_root}'
    printf '%s\n' disabled > '{state}'
    ;;
  *)
    exit 90
    ;;
esac"#,
            plugin_root = plugin_root.display(),
            state = state.display(),
        );
        let hermes = write_executable(&binary_directory.join("hermes"), &hermes_body);
        write_executable(
            &binary_directory.join("python3"),
            r#"exec /usr/bin/python3 "$@""#,
        );

        let mut runner = HermesRunner::new(&hermes).expect("runner");
        assert_eq!(
            install(
                &mut runner,
                &InstallRequest::new(&target, &policy_path, &policy, false),
            ),
            Err(Error::HermesCommand)
        );
        assert!(!target.plugin_root().exists());
        assert!(!policy_path.exists());
        assert_eq!(
            fs::read_to_string(&state).expect("rolled-back state"),
            "disabled\n"
        );
        for parent in [
            target.plugin_root().parent().expect("plugin parent"),
            policy_path.parent().expect("policy parent"),
        ] {
            assert!(
                fs::read_dir(parent)
                    .expect("transaction parent")
                    .all(|entry| {
                        let name = entry.expect("transaction entry").file_name();
                        let name = name.to_string_lossy();
                        !name.starts_with(STAGE_PREFIX)
                            && !name.starts_with(BACKUP_PREFIX)
                            && !name.starts_with(POLICY_STAGE_PREFIX)
                            && !name.starts_with(POLICY_BACKUP_PREFIX)
                    }),
                "rollback must not retain transaction artifacts"
            );
        }
    }

    #[test]
    fn status_failure_leaves_no_prepared_transaction() {
        let (_root, target, policy_path, policy) = setup("status-failure");
        let request = InstallRequest::new(&target, &policy_path, &policy, false);
        let mut hermes = ControlledHermes {
            fail_status: true,
            ..ControlledHermes::default()
        };
        assert_eq!(install(&mut hermes, &request), Err(Error::HermesCommand));
        assert!(!target.plugin_root().exists());
        assert!(!policy_path.exists());
    }

    #[test]
    fn injected_first_update_rename_failure_preserves_original_bytes_and_enablement() {
        let (_root, target, policy_path, policy) = setup("rename-failure");
        let mut hermes = ControlledHermes::default();
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("initial install");
        let original_tool = fs::read(target.plugin_root().join("tools.py")).expect("original tool");
        let original_policy = fs::read(&policy_path).expect("original policy");
        let mut files = FailingFileOps::new(11);
        assert_eq!(
            install_with(
                &mut hermes,
                &InstallRequest::new(&target, &policy_path, &policy, false),
                &mut files,
            ),
            Err(Error::Io {
                kind: std::io::ErrorKind::Other,
            })
        );
        assert_eq!(
            fs::read(target.plugin_root().join("tools.py")).expect("restored tool"),
            original_tool
        );
        assert_eq!(
            fs::read(&policy_path).expect("restored policy"),
            original_policy
        );
        assert!(hermes.enabled);
    }

    #[test]
    fn every_staged_write_and_activation_rename_failure_restores_exact_update_state() {
        // Eight assets, one marker, and one policy write precede four activation moves.
        const TRANSACTION_FILE_OPS: usize = 14;
        for failure_at in 1..=TRANSACTION_FILE_OPS {
            let (_root, target, policy_path, policy) = setup("all-install-failures");
            let mut hermes = ControlledHermes::default();
            install(
                &mut hermes,
                &InstallRequest::new(&target, &policy_path, &policy, false),
            )
            .expect("initial install");
            let original_tool =
                fs::read(target.plugin_root().join("tools.py")).expect("original tool");
            let original_policy = fs::read(&policy_path).expect("original policy");
            let mut files = FailingFileOps::new(failure_at);
            assert!(
                install_with(
                    &mut hermes,
                    &InstallRequest::new(&target, &policy_path, &policy, false),
                    &mut files,
                )
                .is_err(),
                "failure index {failure_at} must fail"
            );
            assert_eq!(
                fs::read(target.plugin_root().join("tools.py")).expect("restored tool"),
                original_tool,
                "failure index {failure_at}"
            );
            assert_eq!(
                fs::read(&policy_path).expect("restored policy"),
                original_policy,
                "failure index {failure_at}"
            );
            assert!(hermes.enabled, "failure index {failure_at}");
            let parent = target.plugin_root().parent().expect("plugin parent");
            assert!(
                fs::read_dir(parent)
                    .expect("plugin parent")
                    .filter_map(Result::ok)
                    .all(|entry| !entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(STAGE_PREFIX)
                        && !entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(BACKUP_PREFIX)),
                "failure index {failure_at} left an unexpected plugin recovery artifact"
            );
        }
    }

    #[test]
    fn exhausted_private_backup_names_leave_the_original_update_state_unchanged() {
        let (_root, target, policy_path, policy) = setup("backup-exhaustion");
        let mut hermes = ControlledHermes::default();
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("initial install");
        let original_tool = fs::read(target.plugin_root().join("tools.py")).expect("original tool");
        let original_policy = fs::read(&policy_path).expect("original policy");
        let parent = target.plugin_root().parent().expect("plugin parent");
        for attempt in 0..SIBLING_NAME_ATTEMPTS {
            let path = parent.join(format!("{BACKUP_PREFIX}{}-{attempt}", std::process::id()));
            fs::create_dir(&path).expect("reserve backup name");
            fs::set_permissions(&path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                .expect("private backup reservation");
        }
        assert_eq!(
            install(
                &mut hermes,
                &InstallRequest::new(&target, &policy_path, &policy, false),
            ),
            Err(Error::Collision)
        );
        assert_eq!(
            fs::read(target.plugin_root().join("tools.py")).expect("tool"),
            original_tool
        );
        assert_eq!(fs::read(&policy_path).expect("policy"), original_policy);
        assert!(hermes.enabled);
    }

    #[test]
    fn commit_cleanup_failure_preserves_backup_and_reports_recovery_required() {
        let (_root, target, policy_path, policy) = setup("commit-cleanup");
        let mut hermes = ControlledHermes::default();
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("initial install");
        // Fourteen writes/moves precede the first committed backup deletion.
        let mut files = FailingFileOps::new(15);
        assert_eq!(
            install_with(
                &mut hermes,
                &InstallRequest::new(&target, &policy_path, &policy, false),
                &mut files,
            ),
            Err(Error::RecoveryRequired)
        );
        assert!(target.plugin_root().exists());
        assert!(fs::read_dir(target.plugin_root().parent().expect("parent"))
            .expect("parent")
            .any(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(BACKUP_PREFIX)
            }));
    }

    #[test]
    fn uninstall_does_not_query_hermes_after_irreversible_trash_commit() {
        let (_root, target, policy_path, policy) = setup("uninstall-final-state");
        let mut hermes = ControlledHermes {
            fail_status_after_first: true,
            ..ControlledHermes::default()
        };
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("install");
        hermes.status_calls = 0;
        assert_eq!(
            uninstall(
                &mut hermes,
                &UninstallRequest::new(&target, &policy_path, false),
            )
            .expect("uninstall final state"),
            LifecycleState {
                installed: false,
                enabled: false,
                modified: false,
                stale_stage: false,
                stale_backup: false,
            }
        );
        assert_eq!(hermes.status_calls, 1);
    }

    #[test]
    fn every_uninstall_trash_move_failure_restores_exact_state() {
        // Eight immutable assets, the marker, then the external policy are moved.
        const UNINSTALL_MOVES: usize = 10;
        for failure_at in 1..=UNINSTALL_MOVES {
            let (_root, target, policy_path, policy) = setup("all-uninstall-failures");
            let mut hermes = ControlledHermes::default();
            install(
                &mut hermes,
                &InstallRequest::new(&target, &policy_path, &policy, false),
            )
            .expect("initial install");
            let original_tool =
                fs::read(target.plugin_root().join("tools.py")).expect("original tool");
            let original_policy = fs::read(&policy_path).expect("original policy");
            let mut files = FailingFileOps::new(failure_at);
            assert!(
                uninstall_with(
                    &mut hermes,
                    &UninstallRequest::new(&target, &policy_path, false),
                    &mut files,
                )
                .is_err(),
                "failure index {failure_at} must fail"
            );
            assert_eq!(
                fs::read(target.plugin_root().join("tools.py")).expect("restored tool"),
                original_tool,
                "failure index {failure_at}"
            );
            assert_eq!(
                fs::read(&policy_path).expect("restored policy"),
                original_policy,
                "failure index {failure_at}"
            );
            assert!(hermes.enabled, "failure index {failure_at}");
        }
    }

    #[test]
    fn uninstall_restore_collision_preserves_transaction_trash_for_recovery() {
        let (_root, target, policy_path, policy) = setup("restore-collision");
        let mut hermes = ControlledHermes::default();
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("initial install");
        // The second move fails, then the first restore move fails.
        let mut files = FailingFileOps::with_two_failures(2, 3);
        assert_eq!(
            uninstall_with(
                &mut hermes,
                &UninstallRequest::new(&target, &policy_path, false),
                &mut files,
            ),
            Err(Error::RecoveryRequired)
        );
        let parent = target.plugin_root().parent().expect("plugin parent");
        assert!(fs::read_dir(parent)
            .expect("plugin parent")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(STAGE_PREFIX)));
    }

    #[test]
    fn restore_move_destination_collision_preserves_unknown_and_transaction_copy() {
        let root = fixture("restore-destination-collision");
        let source_parent = root.0.join("source");
        let trash = root.0.join("trash");
        fs::create_dir(&source_parent).expect("source parent");
        fs::create_dir(&trash).expect("trash");
        for path in [&source_parent, &trash] {
            fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                .expect("private directory");
        }
        let source = source_parent.join("asset");
        let destination = trash.join("asset");
        fs::write(&source, b"unknown").expect("unknown destination");
        fs::write(&destination, b"transaction copy").expect("transaction copy");
        for path in [&source, &destination] {
            fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
                .expect("private file");
        }
        assert_eq!(
            restore_moves(
                &mut NativeFileOps,
                &[MoveRecord {
                    source: source.clone(),
                    destination: destination.clone(),
                }],
                &trash,
            ),
            Err(Error::Collision)
        );
        assert_eq!(fs::read(&source).expect("unknown survives"), b"unknown");
        assert_eq!(
            fs::read(&destination).expect("transaction copy survives"),
            b"transaction copy"
        );
        assert!(trash.exists());
    }

    #[test]
    fn uninstalls_only_managed_files_and_preserves_unrelated_entries() {
        let (_root, target, policy_path, policy) = setup("uninstall");
        let mut hermes = ControlledHermes::default();
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("install");
        fs::write(target.plugin_root().join("operator-note.txt"), b"keep").expect("unrelated");
        fs::set_permissions(
            target.plugin_root().join("operator-note.txt"),
            fs::Permissions::from_mode(PRIVATE_FILE_MODE),
        )
        .expect("private note");
        let state = uninstall(
            &mut hermes,
            &UninstallRequest::new(&target, &policy_path, true),
        )
        .expect("uninstall");
        assert!(!state.installed);
        assert!(!hermes.enabled);
        assert_eq!(
            fs::read(target.plugin_root().join("operator-note.txt")).expect("preserved note"),
            b"keep"
        );
        assert!(!policy_path.exists());
    }

    #[test]
    fn uninstall_requires_confirmation_and_disable_failure_keeps_files() {
        let (_root, target, policy_path, policy) = setup("uninstall-confirmation");
        let mut hermes = ControlledHermes::default();
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("install");
        fs::write(target.plugin_root().join("tools.py"), b"modified").expect("modify");
        assert_eq!(
            uninstall(
                &mut hermes,
                &UninstallRequest::new(&target, &policy_path, false),
            ),
            Err(Error::ConfirmationRequired)
        );
        hermes.fail_disable = true;
        assert_eq!(
            uninstall(
                &mut hermes,
                &UninstallRequest::new(&target, &policy_path, true),
            ),
            Err(Error::HermesCommand)
        );
        assert!(target.plugin_root().exists());
        assert!(policy_path.exists());
    }

    #[test]
    fn rejects_symlinked_policy_and_reports_matching_stale_siblings() {
        let (root, target, policy_path, policy) = setup("stale");
        let mut hermes = ControlledHermes::default();
        install(
            &mut hermes,
            &InstallRequest::new(&target, &policy_path, &policy, false),
        )
        .expect("install");
        let parent = target.plugin_root().parent().expect("parent");
        let stale = parent.join(format!("{STAGE_PREFIX}stale"));
        fs::rename(target.plugin_root(), &stale).expect("move stage");
        let assets = assets::render(&policy_path).expect("assets");
        let lifecycle = inspect(&mut hermes, &target, &policy_path).expect("inspect");
        assert!(lifecycle.stale_stage);
        assert!(!lifecycle.installed);
        fs::rename(&stale, target.plugin_root()).expect("restore plugin");
        let backup = parent.join(format!("{BACKUP_PREFIX}stale"));
        fs::rename(target.plugin_root(), &backup).expect("move backup");
        let backup_state = inspect(&mut hermes, &target, &policy_path).expect("backup inspect");
        assert!(backup_state.stale_backup);
        fs::rename(&backup, target.plugin_root()).expect("restore backup");
        let policy_stage = policy_path
            .parent()
            .expect("policy parent")
            .join(format!("{POLICY_STAGE_PREFIX}pre-marker"));
        fs::write(&policy_stage, b"partial").expect("policy stage");
        fs::set_permissions(&policy_stage, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .expect("private policy stage");
        assert!(
            inspect(&mut hermes, &target, &policy_path)
                .expect("policy stale inspect")
                .stale_stage
        );
        fs::remove_file(&policy_stage).expect("remove policy stage");
        let link = root.0.join("link-policy.json");
        symlink(&policy_path, &link).expect("symlink");
        assert_eq!(
            inspect(&mut hermes, &target, &link),
            Err(Error::UnsafePolicyPath)
        );
        assert_eq!(assets.len(), 8);
    }
}

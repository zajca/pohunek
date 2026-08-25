//! Owner-private persistent cache for standalone host discovery.
//!
//! Cache data is derived and non-authoritative. Invalid, stale, or unsafe data
//! is ignored and a bounded refresh is performed; failed refreshes never replace
//! an existing completed snapshot.

use std::fs;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix::fcntl::{Flock, FlockArg};
use nix::unistd::Uid;

use protocol::{HostRecord, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};

use crate::error::CliError;

/// Cache format revision; changing it invalidates incompatible derived data.
const CACHE_SCHEMA: u32 = 1;
/// Cache directory under the pohunek XDG cache root.
const CACHE_SUBDIR: &str = "host-discovery";
/// Completed cache filename.
const CACHE_FILE: &str = "records.json";
/// Cross-process refresh lock filename.
const LOCK_FILE: &str = "refresh.lock";
/// Mode for derived cache files: only the invoking owner may read/write them.
const PRIVATE_FILE_MODE: u32 = 0o600;
/// Mode for the cache directory, preventing traversal by other local users.
const PRIVATE_DIR_MODE: u32 = 0o700;
/// Reject abnormal cache files before decoding untrusted persisted data.
const MAX_CACHE_BYTES: u64 = 1024 * 1024;
/// Sleep briefly between lock attempts while allowing a cold-call burst to coalesce.
const LOCK_RETRY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snapshot {
    schema: u32,
    fetched_unix_nanos: u128,
    protocol_version: u32,
    remote_port: u16,
    records: Vec<HostRecord>,
}

/// Return a fresh standalone discovery snapshot, refreshing the cache if needed.
pub(crate) async fn records(cache_root: &Path, refresh: bool) -> Result<Vec<HostRecord>, CliError> {
    records_with(cache_root, refresh, |options| async move {
        Ok(pohunek_client::discover_hosts_with_options(options).await?)
    })
    .await
}

/// Cache orchestration with an injected discovery operation.
///
/// Keeping the I/O-producing operation at this internal boundary makes cache
/// correctness deterministic in tests without changing production behavior.
async fn records_with<F, Fut>(
    cache_root: &Path,
    refresh: bool,
    discover: F,
) -> Result<Vec<HostRecord>, CliError>
where
    F: Fn(pohunek_client::DiscoveryOptions) -> Fut,
    Fut: Future<Output = Result<Vec<HostRecord>, CliError>>,
{
    let options = pohunek_client::DiscoveryOptions::new(
        netbird::remote_port().map_err(pohunek_client::ClientError::Netbird)?,
    )?;
    let port = options.port();
    let dir = cache_root.join(CACHE_SUBDIR);
    ensure_private_dir(&dir)?;
    let before = load_fresh(&dir, port)?;
    if !refresh {
        if let Some(snapshot) = before.as_ref() {
            return Ok(snapshot.records.clone());
        }
    }

    let before_fetched = before.map_or(0, |snapshot| snapshot.fetched_unix_nanos);
    let _lock = acquire_lock(&dir, lock_wait(options.deadline())).await?;
    // A competing caller may have completed a refresh while we waited. This
    // re-check coalesces explicit `--refresh` callers too, but a lone refresh
    // always reaches the network.
    if let Some(snapshot) = load_fresh(&dir, port)? {
        if snapshot.fetched_unix_nanos > before_fetched {
            return Ok(snapshot.records);
        }
    }

    let records = discover(options).await?;
    let snapshot = Snapshot {
        schema: CACHE_SCHEMA,
        fetched_unix_nanos: unix_nanos()?,
        protocol_version: PROTOCOL_VERSION.get(),
        remote_port: port,
        records: records.clone(),
    };
    store_atomic(&dir, &snapshot)?;
    Ok(records)
}

fn load_fresh(dir: &Path, port: u16) -> Result<Option<Snapshot>, CliError> {
    let path = dir.join(CACHE_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file()
        || metadata.uid() != Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > MAX_CACHE_BYTES
    {
        return Ok(None);
    }
    let snapshot: Snapshot = match serde_json::from_slice(&fs::read(path)?) {
        Ok(snapshot) => snapshot,
        Err(_) => return Ok(None),
    };
    let now = unix_nanos()?;
    let fresh = snapshot.schema == CACHE_SCHEMA
        && snapshot.protocol_version == PROTOCOL_VERSION.get()
        && snapshot.remote_port == port
        && snapshot.fetched_unix_nanos <= now
        && now.saturating_sub(snapshot.fetched_unix_nanos)
            < pohunek_client::DISCOVERY_CACHE_TTL.as_nanos();
    Ok(fresh.then_some(snapshot))
}

fn ensure_private_dir(dir: &Path) -> Result<(), CliError> {
    let created = match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_dir() => false,
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "host discovery cache directory is not a directory",
            )
            .into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(dir)?;
            true
        }
        Err(error) => return Err(error.into()),
    };
    if created {
        fs::set_permissions(dir, fs::Permissions::from_mode(PRIVATE_DIR_MODE))?;
    }
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "host discovery cache directory is not owner-private",
        )
        .into());
    }
    Ok(())
}

struct RefreshLock {
    _file: Flock<File>,
}

fn lock_wait(deadline: Duration) -> Duration {
    deadline
        .checked_add(pohunek_client::DISCOVERY_LOCK_WAIT_MARGIN)
        .expect("validated deadline leaves room for the lock wait margin")
}

async fn acquire_lock(dir: &Path, wait: Duration) -> Result<RefreshLock, CliError> {
    let path = dir.join(LOCK_FILE);
    let started = tokio::time::Instant::now();
    loop {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file()
            || metadata.uid() != Uid::effective().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "host discovery refresh lock is not owner-private",
            )
            .into());
        }
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(file) => return Ok(RefreshLock { _file: file }),
            Err((_, nix::errno::Errno::EWOULDBLOCK)) => {
                if started.elapsed() >= wait {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out waiting for host discovery cache refresh",
                    )
                    .into());
                }
                tokio::time::sleep(LOCK_RETRY).await;
            }
            Err((_, error)) => return Err(std::io::Error::other(error.to_string()).into()),
        }
    }
}

fn store_atomic(dir: &Path, snapshot: &Snapshot) -> Result<(), CliError> {
    let final_path = dir.join(CACHE_FILE);
    let temp_path = dir.join(format!(
        ".{CACHE_FILE}.{}",
        pohunek_client::next_request_id("cache")
    ));
    let encoded = serde_json::to_vec(snapshot)?;
    let result = (|| -> Result<(), CliError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&temp_path)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
        fs::rename(&temp_path, &final_path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn unix_nanos() -> Result<u128, CliError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .as_nanos())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pohunek-discovery-cache-{tag}-{}",
            std::process::id()
        ))
    }

    fn snapshot(port: u16, fetched_unix_nanos: u128, records: Vec<HostRecord>) -> Snapshot {
        Snapshot {
            schema: CACHE_SCHEMA,
            fetched_unix_nanos,
            protocol_version: PROTOCOL_VERSION.get(),
            remote_port: port,
            records,
        }
    }

    fn record(name: &str) -> HostRecord {
        HostRecord {
            name: Some(name.to_owned()),
            fqdn: Some(format!("{name}.example")),
            address: Some("100.64.0.1".to_owned()),
            overlay: "netbird".to_owned(),
            class: protocol::HostClass::Unreachable,
        }
    }

    #[test]
    fn fresh_cache_requires_matching_schema_protocol_and_port() {
        let root = temp_dir("fresh");
        let dir = root.join(CACHE_SUBDIR);
        ensure_private_dir(&dir).expect("private directory");
        let snapshot = snapshot(18722, unix_nanos().expect("clock"), Vec::new());
        store_atomic(&dir, &snapshot).expect("store");
        assert!(load_fresh(&dir, 18722).expect("load").is_some());
        assert!(load_fresh(&dir, 18723).expect("load").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_and_open_cache_are_ignored() {
        let root = temp_dir("unsafe");
        let dir = root.join(CACHE_SUBDIR);
        ensure_private_dir(&dir).expect("private directory");
        let path = dir.join(CACHE_FILE);
        fs::write(&path, b"not-json").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(load_fresh(&dir, 18722).expect("load").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stale_future_schema_and_protocol_snapshots_are_ignored() {
        let root = temp_dir("invalid");
        let dir = root.join(CACHE_SUBDIR);
        ensure_private_dir(&dir).expect("private directory");
        let now = unix_nanos().expect("clock");
        let cases = [
            snapshot(
                18722,
                now.saturating_sub(pohunek_client::DISCOVERY_CACHE_TTL.as_nanos() + 1),
                Vec::new(),
            ),
            snapshot(
                18722,
                now + pohunek_client::DISCOVERY_CACHE_TTL.as_nanos(),
                Vec::new(),
            ),
        ];
        for item in cases {
            store_atomic(&dir, &item).expect("store");
            assert!(load_fresh(&dir, 18722).expect("load").is_none());
        }
        let mut wrong_schema = snapshot(18722, now, Vec::new());
        wrong_schema.schema += 1;
        store_atomic(&dir, &wrong_schema).expect("store");
        assert!(load_fresh(&dir, 18722).expect("load").is_none());
        let mut wrong_protocol = snapshot(18722, now, Vec::new());
        wrong_protocol.protocol_version += 1;
        store_atomic(&dir, &wrong_protocol).expect("store");
        assert!(load_fresh(&dir, 18722).expect("load").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn symlink_cache_is_ignored_without_following_it() {
        let root = temp_dir("symlink");
        let dir = root.join(CACHE_SUBDIR);
        ensure_private_dir(&dir).expect("private directory");
        let target = root.join("target");
        fs::write(&target, b"{}").expect("target");
        std::os::unix::fs::symlink(&target, dir.join(CACHE_FILE)).expect("symlink");
        assert!(load_fresh(&dir, 18722).expect("load").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn atomic_store_replaces_a_complete_previous_snapshot() {
        let root = temp_dir("atomic");
        let dir = root.join(CACHE_SUBDIR);
        ensure_private_dir(&dir).expect("private directory");
        let now = unix_nanos().expect("clock");
        store_atomic(&dir, &snapshot(18722, now, vec![record("old")])).expect("first store");
        store_atomic(&dir, &snapshot(18722, now + 1, vec![record("new")])).expect("second store");
        let loaded = load_fresh(&dir, 18722).expect("load").expect("fresh");
        assert_eq!(loaded.records[0].name.as_deref(), Some("new"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lock_wait_is_strictly_longer_than_discovery_deadline() {
        let deadline = pohunek_client::DEFAULT_DISCOVERY_DEADLINE;
        assert!(lock_wait(deadline) > deadline);
        let maximum = pohunek_client::MAX_DISCOVERY_DEADLINE;
        assert!(lock_wait(maximum) > maximum);
    }

    #[tokio::test]
    async fn fresh_hit_refresh_and_failed_refresh_preserve_snapshot() {
        let root = temp_dir("refresh");
        let dir = root.join(CACHE_SUBDIR);
        ensure_private_dir(&dir).expect("private directory");
        let port = netbird::remote_port().expect("port");
        store_atomic(
            &dir,
            &snapshot(port, unix_nanos().expect("clock"), vec![record("cached")]),
        )
        .expect("store");
        let calls = Arc::new(AtomicUsize::new(0));
        let hit_calls = Arc::clone(&calls);
        let hit = records_with(&root, false, move |_| {
            let calls = Arc::clone(&hit_calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![record("unexpected")])
            }
        })
        .await
        .expect("hit");
        assert_eq!(hit[0].name.as_deref(), Some("cached"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let refresh_calls = Arc::clone(&calls);
        let refreshed = records_with(&root, true, move |_| {
            let calls = Arc::clone(&refresh_calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![record("refreshed")])
            }
        })
        .await
        .expect("refresh");
        assert_eq!(refreshed[0].name.as_deref(), Some("refreshed"));
        let before = fs::read(dir.join(CACHE_FILE)).expect("before failed refresh");
        let failed = records_with(&root, true, |_| async {
            Err(CliError::Spawn("discovery failed".to_owned()))
        })
        .await;
        let _error = failed.expect_err("refresh must fail");
        assert_eq!(
            fs::read(dir.join(CACHE_FILE)).expect("after failure"),
            before
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn concurrent_cold_refreshes_coalesce() {
        let root = temp_dir("coalesce");
        let calls = Arc::new(AtomicUsize::new(0));
        let left_root = root.clone();
        let left_calls = Arc::clone(&calls);
        let left = async move {
            records_with(&left_root, false, move |_| {
                let calls = Arc::clone(&left_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    Ok(vec![record("once")])
                }
            })
            .await
        };
        let right_root = root.clone();
        let right_calls = Arc::clone(&calls);
        let right = async move {
            records_with(&right_root, false, move |_| {
                let calls = Arc::clone(&right_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![record("twice")])
                }
            })
            .await
        };
        let (left, right) = tokio::join!(left, right);
        let _left = left.expect("left refresh");
        let _right = right.expect("right refresh");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _ = fs::remove_dir_all(root);
    }
}

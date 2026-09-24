//! Origin-bound OS keyring records and cross-process command exclusion.

use nix::{
    fcntl::{Flock, FlockArg},
    unistd::Uid,
};
use pohunek_relay_client::config::Origin;
use relay_protocol::{
    CredentialId, CredentialKind, DeviceCredential, PrincipalId, RotateCredentialRequest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::Arc,
};
use zeroize::Zeroizing;

use super::{config, Error};

pub(super) trait Store {
    async fn load(&self, origin: &Origin) -> Result<Option<DeviceCredential>, Error>;
    async fn save(&self, origin: &Origin, credential: &DeviceCredential) -> Result<(), Error>;
    async fn remove(&self, origin: &Origin) -> Result<(), Error>;
    async fn pending(&self, origin: &Origin) -> Result<Option<PendingRotation>, Error>;
    async fn save_pending(&self, origin: &Origin, pending: &PendingRotation) -> Result<(), Error>;
    async fn clear_pending(&self, origin: &Origin) -> Result<(), Error>;
}

/// Persisted before POST so a lost one-time delivery can be reconciled by exact retry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingRotation {
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    pub principal_id: PrincipalId,
    pub credential_id: CredentialId,
    pub request: RotateCredentialRequest,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RotationRecord {
    version: u16,
    origin: String,
    pending: PendingRotation,
}

fn decode_pending(origin: &Origin, encoded: &str) -> Result<PendingRotation, Error> {
    if encoded.len() > config::KEYRING_BYTES {
        return Err(Error::CorruptKeyring);
    }
    let record: RotationRecord =
        serde_json::from_str(encoded).map_err(|_error| Error::CorruptKeyring)?;
    let lifetime = record.pending.request.expires_at - record.pending.created_at;
    if record.version != config::KEYRING_VERSION
        || record.origin != origin.as_str()
        || !(config::MIN_OVERLAP_SECONDS..=config::MAX_OVERLAP_SECONDS)
            .contains(&record.pending.request.overlap_seconds)
        || lifetime <= time::Duration::ZERO
        || lifetime > pohunek_relay_client::config::MAX_CREDENTIAL_LIFETIME
        || record.pending.request.expires_at.nanosecond() != 0
        || record.pending.created_at.nanosecond() != 0
        || time::Duration::seconds(i64::from(record.pending.request.overlap_seconds)) > lifetime
    {
        return Err(Error::CorruptKeyring);
    }
    Ok(record.pending)
}

#[derive(Debug)]
pub(super) struct Keyring {
    lock: Arc<Flock<File>>,
}

impl Keyring {
    pub(super) fn new(lock: Flock<File>) -> Self {
        Self {
            lock: Arc::new(lock),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u16,
    kind: CredentialKind,
    origin: String,
    credential: DeviceCredential,
}

impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored")
            .field("version", &self.version)
            .field("kind", &self.kind)
            .field("origin", &"[REDACTED]")
            .field("credential", &self.credential)
            .finish()
    }
}

#[derive(Serialize)]
struct ToStore<'a> {
    version: u16,
    kind: CredentialKind,
    origin: &'a str,
    credential: &'a DeviceCredential,
}

fn namespace(origin: &Origin) -> String {
    let mut hash = Sha256::new();
    hash.update(config::KEYRING_DOMAIN);
    hash.update(origin.as_str().as_bytes());
    format!("{:x}", hash.finalize())
}

fn decode(origin: &Origin, encoded: &str) -> Result<DeviceCredential, Error> {
    if encoded.len() > config::KEYRING_BYTES {
        return Err(Error::CorruptKeyring);
    }
    let stored: Stored = serde_json::from_str(encoded).map_err(|_error| Error::CorruptKeyring)?;
    let saved_origin = Origin::parse(&stored.origin).map_err(|_error| Error::CorruptKeyring)?;
    if saved_origin != *origin
        || stored.version != config::KEYRING_VERSION
        || stored.kind != CredentialKind::Human
    {
        return Err(Error::CorruptKeyring);
    }
    pohunek_relay_client::validate_secret(&stored.credential.secret)
        .map_err(|_error| Error::CorruptKeyring)?;
    Ok(stored.credential)
}

async fn keyring_call<T: Send + 'static>(
    lock: Arc<Flock<File>>,
    operation: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    // A platform keyring mutation cannot be cancelled after submission. Await
    // its definitive result, retaining exclusion even if the caller is dropped.
    tokio::task::spawn_blocking(move || {
        let _lock = lock;
        operation()
    })
    .await
    .map_err(|_error| Error::Keyring)?
}

impl Store for Keyring {
    async fn pending(&self, origin: &Origin) -> Result<Option<PendingRotation>, Error> {
        let account = format!("{}-rotation", namespace(origin));
        let encoded = keyring_call(Arc::clone(&self.lock), move || {
            let entry = keyring::Entry::new(config::KEYRING_SERVICE, &account)
                .map_err(|_error| Error::Keyring)?;
            match entry.get_password() {
                Ok(value) => Ok(Some(Zeroizing::new(value))),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(_error) => Err(Error::Keyring),
            }
        })
        .await?;
        encoded
            .map(|encoded| decode_pending(origin, &encoded))
            .transpose()
    }

    async fn save_pending(&self, origin: &Origin, pending: &PendingRotation) -> Result<(), Error> {
        let account = format!("{}-rotation", namespace(origin));
        let encoded = serde_json::to_string(&RotationRecord {
            version: config::KEYRING_VERSION,
            origin: origin.as_str().to_owned(),
            pending: pending.clone(),
        })
        .map_err(|_error| Error::CorruptKeyring)?;
        let _validated = decode_pending(origin, &encoded)?;
        keyring_call(Arc::clone(&self.lock), move || {
            keyring::Entry::new(config::KEYRING_SERVICE, &account)
                .map_err(|_error| Error::Keyring)?
                .set_password(&encoded)
                .map_err(|_error| Error::Keyring)
        })
        .await
    }

    async fn clear_pending(&self, origin: &Origin) -> Result<(), Error> {
        let account = format!("{}-rotation", namespace(origin));
        keyring_call(Arc::clone(&self.lock), move || {
            let entry = keyring::Entry::new(config::KEYRING_SERVICE, &account)
                .map_err(|_error| Error::Keyring)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(_error) => Err(Error::Keyring),
            }
        })
        .await
    }

    async fn load(&self, origin: &Origin) -> Result<Option<DeviceCredential>, Error> {
        let account = namespace(origin);
        let value = keyring_call(Arc::clone(&self.lock), move || {
            let entry = keyring::Entry::new(config::KEYRING_SERVICE, &account)
                .map_err(|_error| Error::Keyring)?;
            match entry.get_password() {
                Ok(secret) => Ok(Some(Zeroizing::new(secret))),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(_error) => Err(Error::Keyring),
            }
        })
        .await?;
        value.map(|value| decode(origin, &value)).transpose()
    }

    async fn save(&self, origin: &Origin, credential: &DeviceCredential) -> Result<(), Error> {
        pohunek_relay_client::validate_secret(&credential.secret)
            .map_err(|_error| Error::CorruptKeyring)?;
        let account = namespace(origin);
        let encoded = Zeroizing::new(
            serde_json::to_string(&ToStore {
                version: config::KEYRING_VERSION,
                kind: CredentialKind::Human,
                origin: origin.as_str(),
                credential,
            })
            .map_err(|_error| Error::CorruptKeyring)?,
        );
        if encoded.len() > config::KEYRING_BYTES {
            return Err(Error::CorruptKeyring);
        }
        keyring_call(Arc::clone(&self.lock), move || {
            keyring::Entry::new(config::KEYRING_SERVICE, &account)
                .map_err(|_error| Error::Keyring)?
                .set_password(&encoded)
                .map_err(|_error| Error::Keyring)
        })
        .await
    }

    async fn remove(&self, origin: &Origin) -> Result<(), Error> {
        let account = namespace(origin);
        keyring_call(Arc::clone(&self.lock), move || {
            let entry = keyring::Entry::new(config::KEYRING_SERVICE, &account)
                .map_err(|_error| Error::Keyring)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(_error) => Err(Error::Keyring),
            }
        })
        .await
    }
}

/// No secret enters disk; this owner-private file only serializes keyring mutations.
pub(super) fn lock(directory: &Path, origin: &Origin) -> Result<Flock<File>, Error> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(config::DIRECTORY_MODE)
        .create(directory)
        .map_err(|_error| Error::Local)?;
    let metadata = fs::symlink_metadata(directory).map_err(|_error| Error::Local)?;
    if !metadata.is_dir()
        || metadata.uid() != Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(Error::Local);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(config::FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join(namespace(origin)))
        .map_err(|_error| Error::Local)?;
    let metadata = file.metadata().map_err(|_error| Error::Local)?;
    if !metadata.is_file()
        || metadata.uid() != Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(Error::Local);
    }
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(guard) => Ok(guard),
        Err((_file, nix::errno::Errno::EWOULDBLOCK)) => Err(Error::Busy),
        Err((_file, _error)) => Err(Error::Local),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_protocol::Secret;

    #[test]
    fn pending_record_preserves_original_policy_time_and_rejects_corruption() {
        let origin = Origin::parse("https://relay.example").expect("origin");
        let mut plan = super::super::rotation_request(60, 60).expect("minimum policy");
        // The journal may be read long after request preparation and expiry;
        // only the server can determine whether the request was committed.
        plan.created_at = time::OffsetDateTime::UNIX_EPOCH;
        plan.request.expires_at = plan.created_at + time::Duration::seconds(60);
        let record = RotationRecord {
            version: config::KEYRING_VERSION,
            origin: origin.as_str().to_owned(),
            pending: PendingRotation {
                created_at: plan.created_at,
                principal_id: PrincipalId::from_uuid(uuid::Uuid::nil()),
                credential_id: CredentialId::from_uuid(uuid::Uuid::nil()),
                request: plan.request.clone(),
            },
        };
        let wire = serde_json::to_value(&record).expect("journal JSON");
        assert_eq!(
            decode_pending(&origin, &wire.to_string())
                .expect("old journal remains resumable")
                .request,
            plan.request
        );
        for (pointer, value) in [
            ("/version", serde_json::json!(999)),
            ("/origin", serde_json::json!("https://other.example/")),
            ("/pending/request/overlap_seconds", serde_json::json!(0)),
            (
                "/pending/request/expires_at",
                serde_json::json!("2099-01-01T00:00:00Z"),
            ),
            (
                "/pending/request/expires_at",
                serde_json::json!("1970-01-01T00:01:00.001Z"),
            ),
        ] {
            let mut invalid = wire.clone();
            *invalid.pointer_mut(pointer).expect("journal field") = value;
            assert!(matches!(
                decode_pending(&origin, &invalid.to_string()),
                Err(Error::CorruptKeyring)
            ));
        }
    }

    #[test]
    fn keyring_record_and_namespace_are_bound_to_canonical_origin() {
        let origin = Origin::parse("https://relay.example").expect("origin");
        let other = Origin::parse("https://other.example").expect("other origin");
        let credential = DeviceCredential {
            credential_id: CredentialId::from_uuid(uuid::Uuid::nil()),
            secret: Secret::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned()),
            expires_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let encoded = serde_json::to_string(&ToStore {
            version: config::KEYRING_VERSION,
            kind: CredentialKind::Human,
            origin: origin.as_str(),
            credential: &credential,
        })
        .expect("keyring encoding");
        assert_eq!(
            decode(&origin, &encoded).expect("matching origin"),
            credential
        );
        assert!(matches!(
            decode(&other, &encoded),
            Err(Error::CorruptKeyring)
        ));
        assert_ne!(namespace(&origin), namespace(&other));
        assert_eq!(
            namespace(&origin),
            namespace(&Origin::parse("https://RELAY.example:443/").expect("same origin"))
        );
        let decoded: Stored = serde_json::from_str(&encoded).expect("record");
        assert!(!format!("{decoded:?}").contains(credential.secret.expose()));
        for (field, value) in [
            ("kind", serde_json::json!("service")),
            ("version", serde_json::json!(999)),
        ] {
            let mut invalid: serde_json::Value =
                serde_json::from_str(&encoded).expect("record JSON");
            invalid[field] = value;
            assert!(matches!(
                decode(&origin, &invalid.to_string()),
                Err(Error::CorruptKeyring)
            ));
        }
        let mut invalid: serde_json::Value = serde_json::from_str(&encoded).expect("record JSON");
        invalid["credential"]["secret"] = serde_json::json!("invalid-secret");
        assert!(matches!(
            decode(&origin, &invalid.to_string()),
            Err(Error::CorruptKeyring)
        ));
    }

    #[test]
    fn credential_operations_exclude_concurrent_writers_and_reject_symlinks() {
        let directory = tempfile::tempdir().expect("lock directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private fixture directory");
        let origin = Origin::parse("https://relay.example").expect("origin");
        let first = lock(directory.path(), &origin).expect("first operation");
        assert!(matches!(lock(directory.path(), &origin), Err(Error::Busy)));
        drop(first);
        drop(lock(directory.path(), &origin).expect("next operation"));
        let path = directory.path().join(namespace(&origin));
        fs::remove_file(&path).expect("replace lock fixture");
        std::os::unix::fs::symlink("missing-target", &path).expect("symlink fixture");
        assert!(matches!(lock(directory.path(), &origin), Err(Error::Local)));
    }

    #[tokio::test(start_paused = true)]
    async fn submitted_keyring_mutations_keep_exclusion_until_the_backend_finishes() {
        for cancel_caller in [false, true] {
            let directory = tempfile::tempdir().expect("lock directory");
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private directory");
            let origin = Origin::parse("https://relay.example").expect("origin");
            let held = Arc::new(lock(directory.path(), &origin).expect("first writer"));
            let (started, ready) = tokio::sync::oneshot::channel();
            let (finish, waiting) = std::sync::mpsc::channel();
            let operation = tokio::spawn(keyring_call(held, move || {
                started.send(()).expect("signal backend submission");
                waiting.recv().expect("release blocked backend");
                Ok(())
            }));
            ready.await.expect("backend is running");
            tokio::time::advance(std::time::Duration::from_secs(20)).await;
            assert!(
                !operation.is_finished(),
                "no artificial timeout after submission"
            );
            if cancel_caller {
                operation.abort();
            }
            assert!(matches!(lock(directory.path(), &origin), Err(Error::Busy)));
            finish.send(()).expect("finish submitted mutation");
            if cancel_caller {
                assert!(operation
                    .await
                    .expect_err("caller cancelled")
                    .is_cancelled());
            } else {
                operation
                    .await
                    .expect("operation task")
                    .expect("keyring result");
            }
            // With the caller cancelled, the submitted blocking operation still
            // owns the lock until it has returned from the platform API.
            tokio::time::resume();
            let next = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    match lock(directory.path(), &origin) {
                        Ok(next) => break next,
                        Err(Error::Busy) => tokio::task::yield_now().await,
                        Err(error) => panic!("unexpected lock error: {error}"),
                    }
                }
            })
            .await
            .expect("completed backend releases exclusion");
            drop(next);
            tokio::time::pause();
        }
    }
}

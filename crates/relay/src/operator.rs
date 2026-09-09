//! Protected local relay administration without public HTTP or OIDC exchanges.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    auth::DigestKey,
    config::{read_private_bytes, read_private_text, Config, ConfigError},
    lifecycle::{
        canonical_initial_provision_request, validate_bootstrap, BootstrapRequest,
        InitialProvisionRequest, Lifecycle, LifecycleError,
    },
    recovery::{RecoveryError, WitnessEvent, WitnessStore},
    runtime::{local_dependencies, RuntimeError},
};

/// Local-only operations; possession of the private configuration and witness key
/// is required before any operation reaches `PostgreSQL`.
#[derive(Debug)]
pub enum Action {
    Migrate,
    Bootstrap {
        identity_file: PathBuf,
    },
    Provision {
        identity_file: PathBuf,
        team_name: String,
        service_account_name: String,
        expires_at: String,
        credential_output: PathBuf,
    },
    Manifest,
    AdvanceRestore {
        reviewed_digest: String,
    },
    Quarantine,
    Reopen {
        reviewed_digest: String,
    },
    ResumeReopen,
}

/// Safe local command failures exclude database URLs, identity values and keys.
#[derive(Debug, Error)]
pub enum OperatorError {
    #[error("local relay dependencies are unavailable")]
    Runtime(#[from] RuntimeError),
    #[error("local relay configuration is unavailable")]
    Config(#[from] ConfigError),
    #[error("local relay recovery transition was rejected")]
    Lifecycle(#[from] LifecycleError),
    #[error("local relay witness is unavailable")]
    Witness(#[from] RecoveryError),
    #[error("local relay identity does not match the configured relay")]
    Identity,
    #[error("bootstrap identity file must contain only a nonempty subject")]
    BootstrapIdentity,
    #[error("initial provisioning input is invalid")]
    ProvisionInput,
    #[error(
        "credential output must be a new owner-private regular file in an owner-private directory"
    )]
    CredentialOutput,
    #[error("credential output already exists but is not bound to this exact local provisioning operation")]
    CredentialOutputCollision,
    #[error(
        "a witnessed provisioning operation requires its exact owner-private credential output"
    )]
    CredentialOutputMissing,
    #[error("reviewed digest must be the exact lower-case SHA-256 from recovery manifest")]
    Digest,
    #[error("local relay result could not be encoded")]
    Output,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    subject: String,
}

/// Bounded credential artifact written only to a new owner-private file.
struct CredentialArtifact {
    team_id: Uuid,
    owner_membership_id: Uuid,
    service_principal_id: Uuid,
    service_membership_id: Uuid,
    credential_id: Uuid,
    audit_id: Uuid,
    expires_at: OffsetDateTime,
    secret: Zeroizing<String>,
}

#[derive(Serialize)]
struct CredentialArtifactWrite<'a> {
    version: u8,
    team_id: Uuid,
    owner_membership_id: Uuid,
    service_principal_id: Uuid,
    service_membership_id: Uuid,
    credential_id: Uuid,
    audit_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    secret: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialArtifactRead {
    version: u8,
    team_id: Uuid,
    owner_membership_id: Uuid,
    service_principal_id: Uuid,
    service_membership_id: Uuid,
    credential_id: Uuid,
    audit_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    secret: String,
}

fn digest(value: &str) -> Result<[u8; 32], OperatorError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(OperatorError::Digest);
    }
    let mut decoded = [0; 32];
    hex::decode_to_slice(value, &mut decoded).map_err(|_error| OperatorError::Digest)?;
    Ok(decoded)
}

/// Credential artifacts are a fixed private interchange between the local operator and relay.
const CREDENTIAL_ARTIFACT_VERSION: u8 = 1;
/// Service secrets use thirty-two random bytes before URL-safe encoding.
const SERVICE_SECRET_BYTES: usize = 32;
/// Owner-private relay artifacts use the same mode as protected configuration input.
const PRIVATE_FILE_MODE: u32 = 0o600;
/// A parent directory must exclude group and world traversal before it holds a credential.
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
/// A credential artifact is a fixed compact local interchange, never an unbounded input.
const MAX_CREDENTIAL_ARTIFACT_BYTES: u64 = 4 * 1024;

fn identity(path: &Path) -> Result<Identity, OperatorError> {
    let encoded = read_private_text(path, "provision.identity_file")?;
    let identity: Identity =
        serde_json::from_str(&encoded).map_err(|_error| OperatorError::BootstrapIdentity)?;
    if identity.subject.is_empty() || identity.subject.contains('\0') {
        return Err(OperatorError::BootstrapIdentity);
    }
    Ok(identity)
}

fn expires_at(value: &str, config: &Config) -> Result<OffsetDateTime, OperatorError> {
    let expires_at =
        OffsetDateTime::parse(value, &Rfc3339).map_err(|_error| OperatorError::ProvisionInput)?;
    if expires_at <= OffsetDateTime::now_utc()
        || expires_at.nanosecond() % 1_000 != 0
        || expires_at > OffsetDateTime::now_utc() + config.auth.service_credential_lifetime
    {
        return Err(OperatorError::ProvisionInput);
    }
    Ok(expires_at)
}

fn private_output_parent(path: &Path) -> Result<File, OperatorError> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(OperatorError::CredentialOutput);
    }
    let parent = path.parent().ok_or(OperatorError::CredentialOutput)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent)
        .map_err(|_error| OperatorError::CredentialOutput)?;
    let metadata = directory
        .metadata()
        .map_err(|_error| OperatorError::CredentialOutput)?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_DIRECTORY_MODE
    {
        return Err(OperatorError::CredentialOutput);
    }
    Ok(directory)
}

fn credential_output_name(path: &Path) -> Result<&std::ffi::OsStr, OperatorError> {
    let name = path.file_name().ok_or(OperatorError::CredentialOutput)?;
    if name == "." || name == ".." {
        return Err(OperatorError::CredentialOutput);
    }
    Ok(name)
}

fn artifact_digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn parse_artifact(bytes: &[u8]) -> Result<CredentialArtifact, OperatorError> {
    let artifact: CredentialArtifactRead =
        serde_json::from_slice(bytes).map_err(|_error| OperatorError::CredentialOutput)?;
    let decoded = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(&artifact.secret)
            .map_err(|_error| OperatorError::CredentialOutput)?,
    );
    if artifact.version != CREDENTIAL_ARTIFACT_VERSION
        || decoded.len() != SERVICE_SECRET_BYTES
        || artifact.expires_at.nanosecond() % 1_000 != 0
    {
        return Err(OperatorError::CredentialOutput);
    }
    Ok(CredentialArtifact {
        team_id: artifact.team_id,
        owner_membership_id: artifact.owner_membership_id,
        service_principal_id: artifact.service_principal_id,
        service_membership_id: artifact.service_membership_id,
        credential_id: artifact.credential_id,
        audit_id: artifact.audit_id,
        expires_at: artifact.expires_at,
        secret: Zeroizing::new(artifact.secret),
    })
}

fn write_new_artifact(
    path: &Path,
    expires_at: OffsetDateTime,
) -> Result<(CredentialArtifact, [u8; 32]), OperatorError> {
    let parent = private_output_parent(path)?;
    let mut secret_bytes = Zeroizing::new([0_u8; SERVICE_SECRET_BYTES]);
    getrandom::getrandom(secret_bytes.as_mut())
        .map_err(|_error| OperatorError::CredentialOutput)?;
    let artifact = CredentialArtifact {
        team_id: Uuid::now_v7(),
        owner_membership_id: Uuid::now_v7(),
        service_principal_id: Uuid::now_v7(),
        service_membership_id: Uuid::now_v7(),
        credential_id: Uuid::now_v7(),
        audit_id: Uuid::now_v7(),
        expires_at,
        secret: Zeroizing::new(URL_SAFE_NO_PAD.encode(secret_bytes.as_ref())),
    };
    let encoded = serde_json::to_vec(&CredentialArtifactWrite {
        version: CREDENTIAL_ARTIFACT_VERSION,
        team_id: artifact.team_id,
        owner_membership_id: artifact.owner_membership_id,
        service_principal_id: artifact.service_principal_id,
        service_membership_id: artifact.service_membership_id,
        credential_id: artifact.credential_id,
        audit_id: artifact.audit_id,
        expires_at: artifact.expires_at,
        secret: &artifact.secret,
    })
    .map_err(|_error| OperatorError::CredentialOutput)?;
    let encoded = Zeroizing::new(encoded);
    let mut file = File::from(
        rustix::fs::openat(
            &parent,
            credential_output_name(path)?,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                OperatorError::CredentialOutputCollision
            } else {
                OperatorError::CredentialOutput
            }
        })?,
    );
    let metadata = file
        .metadata()
        .map_err(|_error| OperatorError::CredentialOutput)?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE
        || metadata.nlink() != 1
    {
        return Err(OperatorError::CredentialOutput);
    }
    file.write_all(&encoded)
        .and_then(|()| file.sync_all())
        .and_then(|()| parent.sync_all())
        .map_err(|_error| OperatorError::CredentialOutput)?;
    Ok((artifact, artifact_digest(&encoded)))
}

fn read_artifact(path: &Path) -> Result<(CredentialArtifact, [u8; 32]), OperatorError> {
    let parent = private_output_parent(path)?;
    let file = File::from(
        rustix::fs::openat(
            &parent,
            credential_output_name(path)?,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_error| OperatorError::CredentialOutput)?,
    );
    let metadata = file
        .metadata()
        .map_err(|_error| OperatorError::CredentialOutput)?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE
        || metadata.nlink() != 1
        || metadata.len() > MAX_CREDENTIAL_ARTIFACT_BYTES
    {
        return Err(OperatorError::CredentialOutput);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_CREDENTIAL_ARTIFACT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_error| OperatorError::CredentialOutput)?;
    if bytes.len() as u64 > MAX_CREDENTIAL_ARTIFACT_BYTES {
        return Err(OperatorError::CredentialOutput);
    }
    let digest = artifact_digest(&bytes);
    Ok((parse_artifact(&bytes)?, digest))
}

fn provision_request(
    config: &Config,
    identity: &Identity,
    team_name: String,
    service_account_name: String,
    artifact: &CredentialArtifact,
    artifact_digest: [u8; 32],
) -> Result<InitialProvisionRequest, OperatorError> {
    let key = DigestKey::new(
        config.digest_key_id.clone(),
        read_private_bytes(&config.digest_key_file, "digest_key_file")?,
    );
    Ok(InitialProvisionRequest {
        relay_id: config.relay_id.clone(),
        issuer: config.issuer.as_str().to_owned(),
        subject: identity.subject.clone(),
        team_name,
        service_account_name,
        expires_at: artifact.expires_at,
        artifact_digest,
        credential_secret_digest: key.digest(&artifact.secret),
        digest_key_id: config.digest_key_id.clone(),
        team_id: artifact.team_id,
        owner_membership_id: artifact.owner_membership_id,
        service_principal_id: artifact.service_principal_id,
        service_membership_id: artifact.service_membership_id,
        credential_id: artifact.credential_id,
        audit_id: artifact.audit_id,
    })
}

fn witness_matches_provision(
    witness: &WitnessStore,
    request: &InitialProvisionRequest,
) -> Result<bool, OperatorError> {
    let commitment = witness.provision_commitment(&canonical_initial_provision_request(request))?;
    let (version, key_id) = witness.provision_commitment_coordinate();
    Ok(matches!(
        witness.latest()?,
        Some(checkpoint) if matches!(checkpoint.event, WitnessEvent::Provision {
            commitment_version,
            ref commitment_key_id,
            request_commitment,
            artifact_digest,
            credential_secret_digest,
            team_id,
            owner_membership_id,
            service_principal_id,
            service_membership_id,
            credential_id,
            audit_id,
        } if commitment_version == version
            && commitment_key_id == &key_id
            && request_commitment == commitment
            && artifact_digest == request.artifact_digest
            && credential_secret_digest == request.credential_secret_digest
            && team_id == request.team_id
            && owner_membership_id == request.owner_membership_id
            && service_principal_id == request.service_principal_id
            && service_membership_id == request.service_membership_id
            && credential_id == request.credential_id
            && audit_id == request.audit_id)
    ))
}

fn witnessed_logical_provision(
    witness: &WitnessStore,
    config: &Config,
    identity: &Identity,
    team_name: &str,
    service_account_name: &str,
    expires_at: OffsetDateTime,
) -> Result<bool, OperatorError> {
    let placeholder = InitialProvisionRequest {
        relay_id: config.relay_id.clone(),
        issuer: config.issuer.as_str().to_owned(),
        subject: identity.subject.clone(),
        team_name: team_name.to_owned(),
        service_account_name: service_account_name.to_owned(),
        expires_at,
        artifact_digest: [0; 32],
        credential_secret_digest: [0; 32],
        digest_key_id: config.digest_key_id.clone(),
        team_id: Uuid::nil(),
        owner_membership_id: Uuid::nil(),
        service_principal_id: Uuid::nil(),
        service_membership_id: Uuid::nil(),
        credential_id: Uuid::nil(),
        audit_id: Uuid::nil(),
    };
    let commitment =
        witness.provision_commitment(&canonical_initial_provision_request(&placeholder))?;
    let (version, key_id) = witness.provision_commitment_coordinate();
    Ok(matches!(
        witness.latest()?,
        Some(checkpoint) if matches!(checkpoint.event, WitnessEvent::Provision {
            commitment_version,
            ref commitment_key_id,
            request_commitment,
            ..
        } if commitment_version == version
            && commitment_key_id == &key_id
            && request_commitment == commitment)
    ))
}

fn prepare_provision_request(
    config: &Config,
    witness: &WitnessStore,
    identity: &Identity,
    team_name: String,
    service_account_name: String,
    expires_at: OffsetDateTime,
    credential_output: &Path,
) -> Result<InitialProvisionRequest, OperatorError> {
    if witnessed_logical_provision(
        witness,
        config,
        identity,
        &team_name,
        &service_account_name,
        expires_at,
    )? {
        let (artifact, digest) = read_artifact(credential_output)
            .map_err(|_error| OperatorError::CredentialOutputMissing)?;
        if artifact.expires_at != expires_at {
            return Err(OperatorError::CredentialOutputCollision);
        }
        let request = provision_request(
            config,
            identity,
            team_name,
            service_account_name,
            &artifact,
            digest,
        )?;
        return if witness_matches_provision(witness, &request)? {
            Ok(request)
        } else {
            Err(OperatorError::CredentialOutputCollision)
        };
    }
    let (artifact, digest) = match write_new_artifact(credential_output, expires_at) {
        Ok(artifact) => artifact,
        Err(OperatorError::CredentialOutputCollision) => {
            let (artifact, digest) = read_artifact(credential_output)
                .map_err(|_error| OperatorError::CredentialOutputCollision)?;
            let request = provision_request(
                config,
                identity,
                team_name.clone(),
                service_account_name.clone(),
                &artifact,
                digest,
            )?;
            if artifact.expires_at != expires_at || !witness_matches_provision(witness, &request)? {
                return Err(OperatorError::CredentialOutputCollision);
            }
            return Ok(request);
        }
        Err(error) => return Err(error),
    };
    provision_request(
        config,
        identity,
        team_name,
        service_account_name,
        &artifact,
        digest,
    )
}

/// Executes one explicitly requested local procedure and returns its reviewable result.
pub async fn execute(config: &Config, action: Action) -> Result<serde_json::Value, OperatorError> {
    let code = action.code();
    let result = execute_inner(config, action).await;
    log_result(code, result.is_ok());
    result
}

fn log_result(action: &'static str, succeeded: bool) {
    let outcome = if succeeded { "succeeded" } else { "failed" };
    tracing::info!(name: "relay.operator.result", action, outcome, "protected local relay operation finished");
}

impl Action {
    const fn code(&self) -> &'static str {
        match self {
            Self::Migrate => "migrate",
            Self::Bootstrap { .. } => "bootstrap",
            Self::Provision { .. } => "provision",
            Self::Manifest => "recovery.manifest",
            Self::AdvanceRestore { .. } => "recovery.advance",
            Self::Quarantine => "recovery.quarantine",
            Self::Reopen { .. } => "recovery.reopen",
            Self::ResumeReopen => "recovery.resume_reopen",
        }
    }
}

async fn execute_inner(
    config: &Config,
    action: Action,
) -> Result<serde_json::Value, OperatorError> {
    let (store, witness) = local_dependencies(config).await?;
    if witness
        .latest()?
        .is_some_and(|checkpoint| checkpoint.relay_id != config.relay_id)
    {
        return Err(OperatorError::Identity);
    }
    let lifecycle = Lifecycle::new(store, Arc::clone(&witness));
    let result = match action {
        Action::Migrate => {
            lifecycle.migrate_local(&config.relay_id).await?;
            serde_json::json!({"action": "migrate", "completed": true})
        }
        Action::Bootstrap { identity_file } => {
            let identity = identity(&identity_file)?;
            let request = BootstrapRequest {
                relay_id: config.relay_id.clone(),
                issuer: config.issuer.as_str().to_owned(),
                subject: identity.subject,
            };
            validate_bootstrap(&request)?;
            if witness.latest()?.is_none() {
                lifecycle.migrate_local(&config.relay_id).await?;
            }
            let checkpoint = lifecycle.bootstrap_local(request).await?;
            serde_json::to_value(checkpoint).map_err(|_error| OperatorError::Output)?
        }
        Action::Provision {
            identity_file,
            team_name,
            service_account_name,
            expires_at: expiry,
            credential_output,
        } => {
            let identity = identity(&identity_file)?;
            let request = prepare_provision_request(
                config,
                &witness,
                &identity,
                team_name,
                service_account_name,
                expires_at(&expiry, config)?,
                &credential_output,
            )?;
            let checkpoint = lifecycle
                .provision_initial_operator_local(request.clone())
                .await?;
            serde_json::json!({
                "action": "provision",
                "team_id": request.team_id,
                "service_principal_id": request.service_principal_id,
                "credential_id": request.credential_id,
                "expires_at": request.expires_at,
                "witness_sequence": checkpoint.sequence,
            })
        }
        Action::Manifest => {
            let manifest = lifecycle.snapshot_authority_manifest().await?;
            serde_json::json!({"reviewed_digest": hex::encode(manifest.digest()), "manifest": manifest})
        }
        Action::AdvanceRestore { reviewed_digest } => serde_json::to_value(
            lifecycle
                .advance_restore_local(&digest(&reviewed_digest)?)
                .await?,
        )
        .map_err(|_error| OperatorError::Output)?,
        Action::Quarantine => {
            lifecycle.quarantine_pending().await?;
            serde_json::json!({"action": "quarantine", "completed": true})
        }
        Action::Reopen { reviewed_digest } => {
            let reviewed = lifecycle.snapshot_authority_manifest().await?;
            if reviewed.digest() != &digest(&reviewed_digest)? {
                return Err(LifecycleError::ManifestMismatch.into());
            }
            let checkpoint = witness.latest()?.ok_or(OperatorError::Identity)?;
            serde_json::to_value(lifecycle.reopen_local(&checkpoint, &reviewed).await?)
                .map_err(|_error| OperatorError::Output)?
        }
        Action::ResumeReopen => serde_json::to_value(lifecycle.resume_reopen_local().await?)
            .map_err(|_error| OperatorError::Output)?,
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_logs_only_fixed_action_and_outcome_coordinates() {
        #[derive(Clone)]
        struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("capture lock").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&bytes);
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || Capture(Arc::clone(&captured)))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for action in [
                Action::Migrate,
                Action::Bootstrap {
                    identity_file: PathBuf::from("sensitive-identity-path"),
                },
                Action::Provision {
                    identity_file: PathBuf::from("sensitive-identity-path"),
                    team_name: "sensitive-team".into(),
                    service_account_name: "sensitive-service".into(),
                    expires_at: "sensitive-expiry".into(),
                    credential_output: PathBuf::from("sensitive-credential-output"),
                },
                Action::Manifest,
                Action::AdvanceRestore {
                    reviewed_digest: "sensitive-review-digest".into(),
                },
                Action::Quarantine,
                Action::Reopen {
                    reviewed_digest: "sensitive-review-digest".into(),
                },
                Action::ResumeReopen,
            ] {
                log_result(action.code(), true);
                log_result(action.code(), false);
            }
        });
        let captured = bytes.lock().expect("capture lock");
        let output = std::str::from_utf8(&captured).expect("UTF-8 JSON");
        assert!(!output.contains("sensitive-"));
        let rows: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).expect("structured log"))
            .collect();
        assert_eq!(rows.len(), 16);
        for (index, row) in rows.iter().enumerate() {
            let fields = row["fields"].as_object().expect("log fields");
            assert_eq!(fields.len(), 3);
            assert!(fields["action"].as_str().is_some());
            assert_eq!(
                fields["outcome"],
                if index % 2 == 0 {
                    "succeeded"
                } else {
                    "failed"
                }
            );
            assert_eq!(
                fields["message"],
                "protected local relay operation finished"
            );
        }
    }

    #[test]
    fn reviewed_digest_is_canonical_and_bootstrap_identity_is_explicit() {
        assert_eq!(
            digest(&"ab".repeat(32)).expect("canonical digest"),
            [0xab; 32]
        );
        for invalid in ["ab".repeat(31), "AB".repeat(32), "x".repeat(64)] {
            assert!(matches!(digest(&invalid), Err(OperatorError::Digest)));
        }
        serde_json::from_str::<Identity>(r#"{"subject":"stable-subject"}"#)
            .expect("stable identity");
        assert!(matches!(
            serde_json::from_str::<Identity>(r#"{"email":"user@example.test"}"#),
            Err(_error)
        ));
    }

    #[test]
    fn credential_artifact_is_private_fsynced_and_never_overwritten() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let directory = tempfile::tempdir().expect("private output directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private output directory mode");
        let path = directory.path().join("service-credential.json");
        let expiry = OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .expect("whole-second expiry")
            + time::Duration::hours(1);
        let (created, digest) = write_new_artifact(&path, expiry).expect("new private artifact");
        let metadata = fs::metadata(&path).expect("artifact metadata");
        assert_eq!(metadata.permissions().mode() & 0o777, PRIVATE_FILE_MODE);
        assert_eq!(metadata.nlink(), 1);
        let (read, read_digest) = read_artifact(&path).expect("read exact artifact");
        assert_eq!(digest, read_digest);
        assert_eq!(created.credential_id, read.credential_id);
        assert_eq!(created.team_id, read.team_id);
        assert_eq!(created.expires_at, read.expires_at);
        assert!(matches!(
            write_new_artifact(&path, expiry),
            Err(OperatorError::CredentialOutputCollision)
        ));
    }

    #[test]
    fn credential_artifact_rejects_public_parent_and_symlink_output() {
        use std::{
            fs,
            os::unix::fs::{symlink, PermissionsExt},
        };

        let directory = tempfile::tempdir().expect("output directory");
        let expiry = OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .expect("whole-second expiry")
            + time::Duration::hours(1);
        assert!(matches!(
            write_new_artifact(&directory.path().join("public-parent"), expiry),
            Err(OperatorError::CredentialOutput)
        ));
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private output directory");
        let target = directory.path().join("target");
        fs::write(&target, "private").expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("target mode");
        let link = directory.path().join("link");
        symlink(&target, &link).expect("credential output symlink");
        assert!(matches!(
            write_new_artifact(&link, expiry),
            Err(OperatorError::CredentialOutputCollision)
        ));
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "One operator command sequence covers the complete stopped relay recovery lifecycle"
    )]
    async fn protected_commands_bootstrap_review_quarantine_and_reopen_without_oidc() {
        use crate::store::Store;
        use sqlx::AssertSqlSafe;
        use std::{fs, os::unix::fs::PermissionsExt};
        use uuid::Uuid;

        let url =
            std::env::var("POHUNEK_RELAY_TEST_DATABASE_URL").expect("explicit PostgreSQL fixture");
        let bootstrap = Store::connect(&url, 1).await.expect("fixture connection");
        let schema = format!("relay_operator_{}", Uuid::now_v7().simple());
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(bootstrap.pool())
            .await
            .expect("isolated schema");
        let (directory, path, _source) = crate::config::tests::config_fixture();
        let mut config = Config::load(&path).expect("complete private configuration");
        config.limits.database_connections = 2;
        config.limits.reserved_connections = 1;
        config.database_url_file = directory.path().join("database-url");
        let scoped_url = format!("{url}?options[search_path]={schema}");
        fs::write(&config.database_url_file, &scoped_url).expect("fixture database address");
        fs::set_permissions(&config.database_url_file, fs::Permissions::from_mode(0o600))
            .expect("private address");
        config.witness_dir = directory.path().join("witness");
        fs::create_dir(&config.witness_dir).expect("witness directory");
        fs::set_permissions(&config.witness_dir, fs::Permissions::from_mode(0o700))
            .expect("private witness directory");
        let identity_file = directory.path().join("identity.json");
        fs::write(&identity_file, r#"{"subject":"bootstrap-subject"}"#).expect("explicit subject");
        fs::set_permissions(&identity_file, fs::Permissions::from_mode(0o600))
            .expect("private identity");
        execute(&config, Action::Migrate)
            .await
            .expect("initial migrations");
        execute(
            &config,
            Action::Bootstrap {
                identity_file: identity_file.clone(),
            },
        )
        .await
        .expect("local bootstrap");
        execute(&config, Action::Bootstrap { identity_file })
            .await
            .expect("exact completed bootstrap replay");
        let store = Store::connect(&scoped_url, 1)
            .await
            .expect("inspect isolated authority");
        let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM principals")
            .fetch_all(store.pool())
            .await
            .expect("principal kinds");
        assert_eq!(kinds, ["infrastructure"]);
        let teams: i64 = sqlx::query_scalar("SELECT count(*) FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("teams");
        assert_eq!(teams, 0);
        let credential_output = directory.path().join("initial-service-credential.json");
        let expiry = (OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .expect("whole-second expiry")
            + time::Duration::hours(1))
        .format(&Rfc3339)
        .expect("RFC 3339 expiry");
        let provision = |identity_file: std::path::PathBuf,
                         credential_output: std::path::PathBuf| {
            Action::Provision {
                identity_file,
                team_name: "initial team".into(),
                service_account_name: "initial service".into(),
                expires_at: expiry.clone(),
                credential_output,
            }
        };
        let wrong_identity_file = directory.path().join("wrong-identity.json");
        fs::write(&wrong_identity_file, r#"{"subject":"another-subject"}"#)
            .expect("wrong explicit subject");
        fs::set_permissions(&wrong_identity_file, fs::Permissions::from_mode(0o600))
            .expect("private wrong identity");
        assert!(matches!(
            execute(
                &config,
                provision(
                    wrong_identity_file,
                    directory.path().join("wrong-owner-credential.json"),
                ),
            )
            .await,
            Err(OperatorError::Lifecycle(LifecycleError::InvalidState))
        ));
        let pre_provision_teams: i64 = sqlx::query_scalar("SELECT count(*) FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("failed attempts leave no team");
        assert_eq!(pre_provision_teams, 0);
        sqlx::query(
            "CREATE FUNCTION reject_initial_provision_audit() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'reject initial provision audit'; END; $$",
        )
        .execute(store.pool())
        .await
        .expect("install audit rejection function");
        sqlx::query(
            "CREATE TRIGGER reject_initial_provision_audit BEFORE INSERT ON audit_events \
             FOR EACH ROW EXECUTE FUNCTION reject_initial_provision_audit()",
        )
        .execute(store.pool())
        .await
        .expect("install audit rejection trigger");
        let audit_error = execute(
            &config,
            provision(
                directory.path().join("identity.json"),
                credential_output.clone(),
            ),
        )
        .await
        .expect_err("audit rejection fails provision");
        assert!(
            matches!(
                audit_error,
                OperatorError::Lifecycle(LifecycleError::Durable)
            ),
            "unexpected audit rejection result: {audit_error:?}"
        );
        let failed_audit_teams: i64 = sqlx::query_scalar("SELECT count(*) FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("audit failure rolls back provision");
        assert_eq!(failed_audit_teams, 0);
        sqlx::query("DROP TRIGGER reject_initial_provision_audit ON audit_events")
            .execute(store.pool())
            .await
            .expect("remove audit rejection trigger");
        sqlx::query("DROP FUNCTION reject_initial_provision_audit()")
            .execute(store.pool())
            .await
            .expect("remove audit rejection function");
        let first = execute(
            &config,
            provision(
                directory.path().join("identity.json"),
                credential_output.clone(),
            ),
        )
        .await
        .expect("provision explicit initial authority");
        assert!(
            first.get("secret").is_none(),
            "generic result never carries a secret"
        );
        let lease = store
            .acquire_lease(&config.relay_id, Uuid::now_v7(), 1)
            .await
            .expect("serving lease blocks provision replay");
        assert!(matches!(
            execute(
                &config,
                provision(
                    directory.path().join("identity.json"),
                    credential_output.clone()
                ),
            )
            .await,
            Err(OperatorError::Lifecycle(LifecycleError::InvalidState))
        ));
        store
            .release_lease(&lease)
            .await
            .expect("stop lease before exact replay");
        assert_eq!(
            first,
            execute(
                &config,
                provision(
                    directory.path().join("identity.json"),
                    credential_output.clone()
                ),
            )
            .await
            .expect("exact provision replay")
        );
        let team_count: i64 = sqlx::query_scalar("SELECT count(*) FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("one team");
        let owner_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM memberships WHERE builtin_role='owner' AND state='active'",
        )
        .fetch_one(store.pool())
        .await
        .expect("one explicit owner");
        let account_count: i64 = sqlx::query_scalar("SELECT count(*) FROM service_accounts")
            .fetch_one(store.pool())
            .await
            .expect("one service account");
        assert_eq!((team_count, owner_count, account_count), (1, 1, 1));
        let (artifact, _) = read_artifact(&credential_output).expect("read owner-private artifact");
        let (runtime_store, runtime_witness) = crate::runtime::local_dependencies(&config)
            .await
            .expect("runtime dependencies");
        let runtime = crate::runtime::RuntimePreparation::acquire(
            runtime_store.clone(),
            runtime_witness,
            crate::admission::AuthorityLimits {
                global: config.limits.connections,
                per_team: config.limits.per_team,
                per_principal: config.limits.per_principal,
            },
            &config.relay_id,
        )
        .await
        .expect("acquire serving authority after local provision");
        let auth = crate::auth::AuthService::new(
            runtime_store,
            crate::auth::DigestKey::new(
                config.digest_key_id.clone(),
                crate::config::read_private_bytes(&config.digest_key_file, "digest_key_file")
                    .expect("digest key"),
            ),
            config.auth.pending_transactions,
            config.auth_limits().expect("auth limits"),
            config.login_policy.clone(),
            runtime.authority(),
        );
        let actor = auth
            .authenticate_bearer(crate::auth::RelayBearerCredential::new(format!(
                "{}.{}",
                artifact.credential_id,
                artifact.secret.as_str()
            )))
            .await
            .expect("issued service credential authenticates");
        assert!(matches!(
            actor.actor().kind(),
            crate::store::ActorKind::Service
        ));
        drop(auth);
        runtime.shutdown().await.expect("clean runtime shutdown");
        execute(&config, Action::Migrate)
            .await
            .expect("stopped initialized migrations");
        let lease = store
            .acquire_lease(&config.relay_id, Uuid::now_v7(), 1)
            .await
            .expect("serving lease");
        assert!(matches!(
            execute(&config, Action::Migrate).await,
            Err(OperatorError::Lifecycle(LifecycleError::InvalidState))
        ));
        let review = execute(&config, Action::Manifest)
            .await
            .expect("review manifest");
        let reviewed_digest = review["reviewed_digest"]
            .as_str()
            .expect("review digest")
            .to_owned();
        assert!(matches!(
            execute(
                &config,
                Action::AdvanceRestore {
                    reviewed_digest: reviewed_digest.clone()
                }
            )
            .await,
            Err(OperatorError::Lifecycle(LifecycleError::InvalidState))
        ));
        store
            .release_lease(&lease)
            .await
            .expect("stop serving lease");
        assert!(matches!(
            execute(
                &config,
                Action::AdvanceRestore {
                    reviewed_digest: "00".repeat(32)
                }
            )
            .await,
            Err(OperatorError::Lifecycle(LifecycleError::ManifestMismatch))
        ));
        let advanced = execute(
            &config,
            Action::AdvanceRestore {
                reviewed_digest: reviewed_digest.clone(),
            },
        )
        .await
        .expect("advance before restore");
        assert_eq!(
            advanced,
            execute(&config, Action::AdvanceRestore { reviewed_digest })
                .await
                .expect("lost advance response retry")
        );
        execute(&config, Action::Quarantine)
            .await
            .expect("quarantine restored database");
        execute(&config, Action::Quarantine)
            .await
            .expect("lost quarantine response retry");
        let review = execute(&config, Action::Manifest)
            .await
            .expect("review quarantine manifest");
        execute(
            &config,
            Action::Reopen {
                reviewed_digest: review["reviewed_digest"]
                    .as_str()
                    .expect("digest")
                    .to_owned(),
            },
        )
        .await
        .expect("audited reviewed reopen");
        let state: String = sqlx::query_scalar("SELECT state FROM relay_identity")
            .fetch_one(store.pool())
            .await
            .expect("current state");
        assert_eq!(state, "normal");
        let generation: i64 = sqlx::query_scalar("SELECT recovery_generation FROM relay_identity")
            .fetch_one(store.pool())
            .await
            .expect("generation");
        assert_eq!(generation, 2);
        sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(bootstrap.pool())
            .await
            .expect("cleanup isolated schema");
    }
}

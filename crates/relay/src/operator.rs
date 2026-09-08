//! Protected local relay administration without public HTTP or OIDC exchanges.

use std::{path::PathBuf, sync::Arc};

use serde::Deserialize;
use thiserror::Error;

use crate::{
    config::{read_private_text, Config, ConfigError},
    lifecycle::{validate_bootstrap, BootstrapRequest, Lifecycle, LifecycleError},
    recovery::RecoveryError,
    runtime::{local_dependencies, RuntimeError},
};

/// Local-only operations; possession of the private configuration and witness key
/// is required before any operation reaches `PostgreSQL`.
#[derive(Debug)]
pub enum Action {
    Migrate,
    Bootstrap { identity_file: PathBuf },
    Manifest,
    AdvanceRestore { reviewed_digest: String },
    Quarantine,
    Reopen { reviewed_digest: String },
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
            let encoded = read_private_text(&identity_file, "bootstrap.identity_file")?;
            let identity: Identity = serde_json::from_str(&encoded)
                .map_err(|_error| OperatorError::BootstrapIdentity)?;
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
        assert_eq!(rows.len(), 14);
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

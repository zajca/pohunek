//! Native relay login and origin-bound OS keyring credential lifecycle.

mod config;
mod flows;
mod store;

use clap::{Args, Subcommand};
use pohunek_relay_client::{
    config::{Limits, Origin},
    Client,
};
use relay_protocol::{Idempotency, RotateCredentialRequest};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};
use uuid::Uuid;

use crate::{error::CliError, paths::Paths};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Remote(#[from] pohunek_relay_client::Error),
    #[error("OS keyring operation failed")]
    Keyring,
    #[error("stored relay authentication record is malformed or belongs to another origin")]
    CorruptKeyring,
    #[error("relay credential operation could not obtain an owner-private local lock")]
    Local,
    #[error("another credential operation is already running for this relay")]
    Busy,
    #[error("no credential is stored for this relay; run relay login")]
    NotLoggedIn,
    #[error("already signed in to this relay; use relay rotate or relay logout")]
    AlreadyLoggedIn,
    #[error("relay device login expired")]
    Expired,
    #[error("relay device login was denied")]
    Denied,
    #[error("relay device login was cancelled")]
    Cancelled,
    #[error("keyring write failed; the newly issued credential was revoked")]
    StorageRevoked,
    #[error("keyring write and credential revocation failed; revoke the newly issued credential from your relay account")]
    StorageUnrevoked,
    #[error("the replacement secret was already delivered and has been revoked; sign in again when the previous credential expires")]
    DeliveryConsumed,
    #[error("an earlier credential rotation still needs reconciliation; run relay resume-rotation with the same relay account")]
    RotationPending,
    #[error("no pending credential rotation exists for this relay")]
    NoPendingRotation,
    #[error("relay rejected the rotation policy without issuing a credential; choose a shorter lifetime or overlap")]
    RotationRejected,
    #[error("relay response did not identify an active human account and credential")]
    AccountKind,
    #[error("the relay returned an invalid account or credential and compensation failed; revoke the newly issued credential from your relay account")]
    DeliveryUnrevoked,
    #[error("invalid relay credential expiry or rotation overlap")]
    Lifetime,
    #[error("failed to read a bounded relay CA certificate")]
    Certificate,
    #[error("failed to write relay command output")]
    Output,
}

#[derive(Debug, Args)]
pub(crate) struct Connection {
    /// Exact HTTPS origin of the relay, without a path, query, or credentials.
    #[arg(long)]
    origin: String,
    /// Additional public CA certificate for a privately operated relay.
    #[arg(long)]
    ca_file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// Sign in using the OIDC device flow and store the credential in OS keyring.
    Login(Connection),
    /// Show the account authenticated by this origin's stored credential.
    Status {
        #[command(flatten)]
        connection: Connection,
        #[arg(long)]
        json: bool,
    },
    /// Revoke the stored credential before deleting it from OS keyring.
    Logout(Connection),
    /// Reconcile an interrupted rotation using its original policy and request.
    ResumeRotation(Connection),
    /// Replace the stored credential with explicit expiry and overlap limits.
    Rotate {
        #[command(flatten)]
        connection: Connection,
        #[arg(long)]
        expires_in_seconds: u64,
        #[arg(long)]
        overlap_seconds: u32,
    },
}

impl Action {
    pub(crate) const fn wants_json(&self) -> bool {
        matches!(self, Self::Status { json: true, .. })
    }
    fn connection(&self) -> &Connection {
        match self {
            Self::Login(connection)
            | Self::Logout(connection)
            | Self::ResumeRotation(connection)
            | Self::Status { connection, .. }
            | Self::Rotate { connection, .. } => connection,
        }
    }
}

fn client(connection: &Connection) -> Result<Client, Error> {
    let origin = Origin::parse(&connection.origin)?;
    let ca = connection
        .ca_file
        .as_ref()
        .map(|path| {
            // Reject special files without waiting for a FIFO writer or device data.
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .map_err(|_error| Error::Certificate)?;
            let metadata = file.metadata().map_err(|_error| Error::Certificate)?;
            if !metadata.is_file() || metadata.len() > config::CA_BYTES {
                return Err(Error::Certificate);
            }
            let mut bytes = Vec::new();
            file.take(config::CA_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_error| Error::Certificate)?;
            if bytes.len() as u64 > config::CA_BYTES {
                return Err(Error::Certificate);
            }
            reqwest::Certificate::from_pem(&bytes).map_err(|_error| Error::Certificate)
        })
        .transpose()?;
    Client::new(
        origin,
        Limits {
            request_timeout: config::REQUEST_TIMEOUT,
            response_bytes: config::RESPONSE_BYTES,
        },
        ca,
    )
    .map_err(Into::into)
}

#[derive(Debug, Clone)]
struct RotationPlan {
    created_at: time::OffsetDateTime,
    request: RotateCredentialRequest,
}

fn rotation_request(expires_in_seconds: u64, overlap_seconds: u32) -> Result<RotationPlan, Error> {
    let seconds = i64::try_from(expires_in_seconds).map_err(|_error| Error::Lifetime)?;
    let lifetime = time::Duration::seconds(seconds);
    if seconds <= 0
        || lifetime > pohunek_relay_client::config::MAX_CREDENTIAL_LIFETIME
        || !(config::MIN_OVERLAP_SECONDS..=config::MAX_OVERLAP_SECONDS).contains(&overlap_seconds)
        || u64::from(overlap_seconds) > expires_in_seconds
    {
        return Err(Error::Lifetime);
    }
    let now = time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .map_err(|_error| Error::Lifetime)?;
    Ok(RotationPlan {
        created_at: now,
        request: RotateCredentialRequest {
            expires_at: now + lifetime,
            overlap_seconds,
            idempotency: Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        },
    })
}

pub(crate) async fn run(action: Action) -> Result<(), CliError> {
    let client = client(action.connection())?;
    let directory = Paths::data_home_only()?
        .join(pohunek_paths::APP_DIR)
        .join(config::LOCK_DIRECTORY);
    let keyring = store::Keyring::new(store::lock(&directory, client.origin())?);
    match action {
        Action::Login(_) => {
            flows::login(&client, &keyring, |uri, code| {
                writeln!(std::io::stderr(), "Open {uri} and enter code {code}.")
                    .map_err(|_error| Error::Output)
            })
            .await?;
            println!("Signed in to {}.", client.origin().as_str());
        }
        Action::Status { json, .. } => {
            let account = flows::status(&client, &keyring).await?;
            if json {
                print!("{}", super::render_json(&account)?);
            } else {
                println!(
                    "Authenticated as {} ({:?}).",
                    account.principal_id, account.kind
                );
            }
        }
        Action::Logout(_) => {
            flows::logout(&client, &keyring).await?;
            println!("Signed out of {}.", client.origin().as_str());
        }
        Action::ResumeRotation(_) => {
            flows::resume_rotation(&client, &keyring).await?;
            println!(
                "Previous credential rotation reconciled for {}.",
                client.origin().as_str()
            );
        }
        Action::Rotate {
            expires_in_seconds,
            overlap_seconds,
            ..
        } => {
            flows::rotate(
                &client,
                &keyring,
                &rotation_request(expires_in_seconds, overlap_seconds)?,
            )
            .await?;
            println!("Credential rotated for {}.", client.origin().as_str());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn relay_commands_require_an_explicit_origin_and_rotation_policy() {
        for command in ["login", "status", "logout", "rotate", "resume-rotation"] {
            let error = crate::Cli::try_parse_from(["pohunek", "relay", command])
                .expect_err("missing origin");
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument
            );
        }
        for command in ["login", "status", "logout", "resume-rotation"] {
            crate::Cli::try_parse_from([
                "pohunek",
                "relay",
                command,
                "--origin",
                "https://relay.example",
            ])
            .expect("valid relay command");
        }
        let cli = crate::Cli::try_parse_from([
            "pohunek",
            "relay",
            "status",
            "--origin",
            "https://relay.example",
            "--json",
        ])
        .expect("JSON status");
        assert!(cli.command.wants_json());
        crate::Cli::try_parse_from([
            "pohunek",
            "relay",
            "rotate",
            "--origin",
            "https://relay.example",
            "--expires-in-seconds",
            "3600",
            "--overlap-seconds",
            "60",
        ])
        .expect("explicit rotation policy");
    }

    #[test]
    fn rotation_policy_rejects_overflow_and_invalid_overlap() {
        for (lifetime, overlap) in [(0, 0), (u64::MAX, 0), (60, 61), (3600, 301), (3600, 0)] {
            assert!(matches!(
                rotation_request(lifetime, overlap),
                Err(Error::Lifetime)
            ));
        }
        let request = rotation_request(3600, 60).expect("valid policy");
        assert_eq!(request.request.expires_at.nanosecond(), 0);
        assert_eq!(request.request.overlap_seconds, 60);
    }

    #[test]
    fn custom_ca_rejects_special_files_and_oversized_bundles() {
        let directory = tempfile::tempdir().expect("CA fixture");
        let path = directory.path().join("ca.pem");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRUSR).expect("FIFO fixture");
        let connection = Connection {
            origin: "https://relay.example".to_owned(),
            ca_file: Some(path.clone()),
        };
        assert!(matches!(client(&connection), Err(Error::Certificate)));
        std::fs::remove_file(&path).expect("remove FIFO");
        std::fs::write(
            &path,
            vec![0; usize::try_from(config::CA_BYTES + 1).expect("CA bound")],
        )
        .expect("oversized fixture");
        assert!(matches!(client(&connection), Err(Error::Certificate)));
    }
}

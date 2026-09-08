//! Coordinates fenced relay startup around one shared admission authority.

// Rust guideline compliant 2026-09-08

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum_server::Handle;
use tokio::time::{interval, timeout, MissedTickBehavior};
use zeroize::Zeroizing;

use thiserror::Error;
use uuid::Uuid;

use crate::{
    admission::{Authority, AuthorityError, AuthorityLimits},
    auth::{AuthError, AuthService, DigestKey, OidcClient, OidcClientConfig},
    config::{read_private_bytes, read_private_text, Config, ConfigError, Tls},
    lifecycle::{Lifecycle, LifecycleError},
    recovery::{RecoveryError, WitnessStore},
    server::{router, transport::BoundedAddress, ServerState},
    store::{Store, StoreError},
};

/// Holds the sole fenced authority used by HTTP and all future relay ingress.
#[derive(Debug)]
pub struct RuntimePreparation {
    authority: Arc<Authority>,
}

/// Reports safe runtime startup failures.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("relay recovery evidence is unavailable")]
    Recovery(#[from] RecoveryError),
    #[error("relay lifecycle state is not eligible for normal startup")]
    Lifecycle(#[from] LifecycleError),
    #[error("relay fence is unavailable")]
    Lease(#[from] StoreError),
    #[error("relay authority is unavailable")]
    Authority(#[from] AuthorityError),
    #[error("relay requires local bootstrap or reviewed recovery")]
    MissingCheckpoint,
    #[error("configured relay identity does not match recovery evidence")]
    IdentityMismatch,
    #[error("relay configuration rejected: {0}")]
    Config(#[from] ConfigError),
    #[error("relay authentication initialization failed")]
    Auth(#[from] AuthError),
    #[error("relay TLS initialization failed")]
    Tls,
    #[error("relay HTTP transport stopped unexpectedly")]
    Transport,
    #[error("relay logging initialization failed")]
    Logging,
    #[error("relay shutdown exceeded its deadline; recovery review is required")]
    Shutdown,
    #[error("relay signal handler could not be installed")]
    Signal,
}

impl RuntimePreparation {
    /// Validates recovery evidence, fences this process, and creates one authority.
    pub async fn acquire(
        store: Store,
        witness: Arc<WitnessStore>,
        limits: AuthorityLimits,
        expected_relay_id: &str,
    ) -> Result<Self, RuntimeError> {
        let checkpoint = witness.latest()?.ok_or(RuntimeError::MissingCheckpoint)?;
        if checkpoint.relay_id != expected_relay_id {
            return Err(RuntimeError::IdentityMismatch);
        }
        let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
        let generation = lifecycle.validate_normal_start(&checkpoint).await?;
        let latch = witness.begin_run(Some(&checkpoint), &checkpoint.relay_id, generation)?;
        store.migrate().await?;
        let lease = store
            .acquire_lease(&checkpoint.relay_id, Uuid::now_v7(), generation)
            .await?;
        lifecycle.finalize_normal_start(&lease).await?;
        let authority = Arc::new(Authority::new(store, lease, witness, latch, limits)?);
        authority.watch_fence().await?;
        Ok(Self { authority })
    }

    /// Returns the sole admission and fencing authority.
    #[must_use]
    pub fn authority(&self) -> Arc<Authority> {
        Arc::clone(&self.authority)
    }

    /// Marks the completed active run clean after all ingress tasks have stopped.
    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.authority.release_clean().await?;
        Ok(())
    }
}

/// Starts a configured relay and drains all ingress before publishing a clean stop.
pub async fn run(
    config: Config,
    shutdown: impl Future<Output = Result<(), RuntimeError>>,
) -> Result<(), RuntimeError> {
    run_notifying(config, shutdown, None).await
}

async fn run_notifying(
    config: Config,
    shutdown: impl Future<Output = Result<(), RuntimeError>>,
    listening: Option<tokio::sync::oneshot::Sender<std::net::SocketAddr>>,
) -> Result<(), RuntimeError> {
    let Prepared {
        store,
        witness,
        digest,
        oidc,
        tls,
    } = prepare(&config).await?;
    let runtime = RuntimePreparation::acquire(
        store.clone(),
        witness,
        AuthorityLimits {
            global: config.limits.connections,
            per_team: config.limits.per_team,
            per_principal: config.limits.per_principal,
        },
        &config.relay_id,
    )
    .await?;
    let authority = runtime.authority();
    let handle = Handle::new();
    let connections_stop = tokio_util::sync::CancellationToken::new();
    let mut failure = StopGuard {
        authority: Arc::clone(&authority),
        handle: handle.clone(),
        clean: false,
        connections_stop: connections_stop.clone(),
    };
    let auth = AuthService::new(
        store.clone(),
        digest,
        config.auth.pending_transactions,
        config.auth_limits()?,
        config.login_policy.clone(),
        Arc::clone(&authority),
    );
    let app = router(ServerState::new(
        config.clone(),
        store,
        auth,
        oidc,
        Arc::clone(&authority),
    ));
    let server = serve_http(&config, tls, app, handle.clone(), connections_stop);
    tokio::pin!(server);
    let notify_listening = async {
        if let Some(address) = handle.listening().await {
            tracing::info!(name: "relay.runtime.listening", address = %address.socket, "relay listener is ready");
            if let Some(listening) = listening {
                let _receiver_closed = listening.send(address.socket);
            }
        }
        std::future::pending::<()>().await;
    };
    tracing::info!(name: "relay.runtime.start", relay_id = config.relay_id, "starting fenced relay ingress");
    tokio::select! {
        result = &mut server => { result?; return Err(RuntimeError::Transport); }
        result = renew(&authority, config.limits.lease_renew) => { result?; return Err(RuntimeError::Transport); }
        result = watch(&authority, config.limits.idle_recheck) => { result?; return Err(RuntimeError::Transport); }
        result = shutdown => result?,
        () = notify_listening => unreachable!("listener notification never completes"),
    }
    // Refresh immediately before closing ingress so the bounded drain has a current fence.
    timeout(config.limits.lease_renew, authority.renew_once())
        .await
        .map_err(|_error| RuntimeError::Shutdown)??;
    authority.begin_stop();
    handle.graceful_shutdown(Some(config.limits.shutdown_timeout));
    timeout(config.limits.shutdown_timeout, &mut server)
        .await
        .map_err(|_error| RuntimeError::Shutdown)??;
    if handle.connection_count() != 0 {
        return Err(RuntimeError::Shutdown);
    }
    timeout(config.limits.shutdown_timeout, runtime.shutdown())
        .await
        .map_err(|_error| RuntimeError::Shutdown)??;
    failure.clean = true;
    tracing::info!(name: "relay.runtime.stop", "relay stopped with a clean recovery checkpoint");
    Ok(())
}

/// HMAC and Ed25519 keys require 256 bits of independently generated entropy.
const KEY_BYTES: usize = 32;

#[derive(Debug)]
struct Prepared {
    store: Store,
    witness: Arc<WitnessStore>,
    digest: DigestKey,
    oidc: OidcClient,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
}

pub(crate) async fn local_dependencies(
    config: &Config,
) -> Result<(Store, Arc<WitnessStore>), RuntimeError> {
    let database_url = Zeroizing::new(read_private_text(
        &config.database_url_file,
        "database_url_file",
    )?);
    let witness_bytes = Zeroizing::new(read_private_bytes(
        &config.witness_key_file,
        "witness_key_file",
    )?);
    let witness_key: &[u8; KEY_BYTES] =
        witness_bytes
            .as_slice()
            .try_into()
            .map_err(|_error| ConfigError::Invalid {
                field: "witness_key_file",
            })?;
    let store = Store::connect_with_limits(
        database_url.trim(),
        config.limits.database_connections,
        config.limits.fence_connections,
        config.limits.reserved_connections,
    )
    .await?;
    let witness = Arc::new(WitnessStore::open(
        &config.witness_dir,
        ed25519_dalek::SigningKey::from_bytes(witness_key),
        config.witness_key_id.clone(),
    )?);
    Ok((store, witness))
}

async fn prepare(config: &Config) -> Result<Prepared, RuntimeError> {
    let (store, witness) = local_dependencies(config).await?;
    let digest = read_private_bytes(&config.digest_key_file, "digest_key_file")?;
    if digest.len() < KEY_BYTES {
        return Err(ConfigError::Invalid {
            field: "digest_key_file",
        }
        .into());
    }
    // Network discovery and TLS parsing happen before the five-second serving lease is acquired.
    let oidc = OidcClient::discover(OidcClientConfig::new(
        config.issuer.clone(),
        config.client_id.clone(),
        config.callback.clone(),
        config.limits.request_timeout,
        config.ca_file.clone(),
        config.limits.response_bytes,
    )?)
    .await?;
    let tls = match &config.tls {
        Tls::LoopbackProxy => None,
        Tls::Native {
            certificate_file,
            private_key_file,
        } => {
            let certificate = read_private_bytes(certificate_file, "tls.certificate_file")?;
            let key = read_private_bytes(private_key_file, "tls.private_key_file")?;
            Some(
                axum_server::tls_rustls::RustlsConfig::from_pem(certificate, key)
                    .await
                    .map_err(|_error| RuntimeError::Tls)?,
            )
        }
    };
    Ok(Prepared {
        store,
        witness,
        digest: DigestKey::new(config.digest_key_id.clone(), digest),
        oidc,
        tls,
    })
}

#[derive(Debug)]
struct StopGuard {
    authority: Arc<Authority>,
    handle: Handle<BoundedAddress>,
    clean: bool,
    connections_stop: tokio_util::sync::CancellationToken,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        if !self.clean {
            self.authority.close_all();
            self.connections_stop.cancel();
            self.handle.shutdown();
            tracing::error!(name: "relay.runtime.fail_stop", "relay stopped; recovery review is required");
        }
    }
}

async fn renew(authority: &Authority, period: Duration) -> Result<(), RuntimeError> {
    let mut ticks = interval(period);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        timeout(period, authority.renew_once())
            .await
            .map_err(|_error| RuntimeError::Shutdown)??;
    }
}

async fn watch(authority: &Authority, period: Duration) -> Result<(), RuntimeError> {
    let mut ticks = interval(period);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        if authority.is_closed() {
            return Err(RuntimeError::Authority(AuthorityError::Cancelled));
        }
        timeout(period, authority.watch_fence())
            .await
            .map_err(|_error| RuntimeError::Shutdown)??;
    }
}

async fn serve_http(
    config: &Config,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
    app: axum::Router,
    handle: Handle<BoundedAddress>,
    connections_stop: tokio_util::sync::CancellationToken,
) -> Result<(), RuntimeError> {
    let address = BoundedAddress {
        socket: config.bind,
        limits: config.limits.clone(),
        shutdown: connections_stop,
    };
    let result = if let Some(tls) = tls {
        let acceptor = axum_server::tls_rustls::RustlsAcceptor::new(tls).handshake_timeout(
            config
                .limits
                .request_timeout
                .min(config.limits.connection_lifetime),
        );
        axum_server::bind(address)
            .acceptor(acceptor)
            .handle(handle)
            .serve(app.into_make_service())
            .await
    } else {
        axum_server::bind(address)
            .handle(handle)
            .serve(app.into_make_service())
            .await
    };
    result.map_err(|_error| RuntimeError::Transport)
}

/// Installs process-wide structured logging once, from the executable entry point.
pub fn init_logging(config: &Config) -> Result<(), RuntimeError> {
    let files = pohunek_logging::Files::new("relay.jsonl", pohunek_logging::Legacy::None)
        .map_err(|_error| RuntimeError::Logging)?;
    let policy =
        pohunek_logging::Policy::new(config.logging.max_file_bytes, config.logging.max_files)
            .map_err(|_error| RuntimeError::Logging)?;
    let writer = pohunek_logging::Writer::open(&config.logging.directory, files, policy)
        .map_err(|_error| RuntimeError::Logging)?;
    tracing_subscriber::fmt()
        .json()
        .with_writer(Mutex::new(writer))
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .map_err(|_error| RuntimeError::Logging)
}

/// Waits for a local termination request without performing privileged work.
pub async fn shutdown_signal() -> Result<(), RuntimeError> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_error| RuntimeError::Signal)?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.map_err(|_error| RuntimeError::Signal),
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(all(test, feature = "postgres-tests"))]
mod tests;

//! Relay configuration validation.

// Rust guideline compliant 2026-09-08

use std::fmt::{Debug, Formatter};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use url::Url;

/// Default path served after a successful browser callback.
pub const BROWSER_SUCCESS_PATH: &str = "/account/ready";
/// RFC lease lifetime.
pub const LEASE_LIFETIME: Duration = Duration::from_secs(5);
/// RFC maximum lease renewal interval.
pub const MAX_LEASE_RENEW: Duration = Duration::from_secs(1);
/// RFC maximum idle authority recheck interval.
pub const MAX_IDLE_RECHECK: Duration = Duration::from_millis(100);
/// Limits configuration and secret-reference files before parsing them.
///
/// Relay configuration is deliberately compact. This bound prevents a private
/// but accidentally enormous file from consuming unbounded startup memory.
const MAX_PRIVATE_FILE_BYTES: u64 = 64 * 1024;
/// Owner-only mode required for private files.
const PRIVATE_FILE_MODE: u32 = 0o600;
/// Owner-only mode required for private directories.
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

// Deployment ceilings keep a misconfigured metadata-only relay finite. Raising
// them requires revisiting the aggregate memory, database, and replay budgets.
const MAX_CONNECTIONS: usize = 1024;
const MAX_BACKLOG: u32 = 1024;
const MAX_DATABASE_CONNECTIONS: u32 = 64;
const MAX_CONTROL_CONNECTIONS: u32 = 8;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_PENDING_LOGINS: usize = 1024;
const MAX_REQUESTS_PER_WINDOW: u32 = 10_000;
/// Configured ceiling for one in-bound HTTP request; also the router-test budget.
pub(crate) const MAX_REQUEST_TIMEOUT_MS: u64 = 30_000;
const MAX_CONNECTION_LIFETIME_MS: u64 = 3_600_000;
const MAX_RATE_WINDOW_MS: u64 = 60_000;
const MAX_LOGIN_LIFETIME_MS: u64 = 15 * 60_000;
const MAX_BROWSER_LIFETIME_MS: u64 = 24 * 3_600_000;
const MAX_BROWSER_IDLE_MS: u64 = 3_600_000;
const MAX_HUMAN_LIFETIME_MS: u64 = 90 * 24 * 3_600_000;
/// Service credentials may be long-lived, but never indefinite.
const MAX_SERVICE_CREDENTIAL_LIFETIME_MS: u64 = 90 * 24 * 3_600_000;
/// Rotation overlap must remain short enough to bound dual-credential exposure.
const MAX_ROTATION_OVERLAP_MS: u64 = 5 * 60_000;
const MAX_CREDENTIALS_PER_PRINCIPAL: usize = 128;
const MAX_SERVICE_ACCOUNTS_PER_TEAM: usize = 1_024;
const MAX_DEVICE_POLL_LEASE_MS: u64 = 30_000;
const MAX_DEVICE_INTERVAL_MS: u64 = 60_000;
/// One-use evidence challenges stay short-lived so replay exposure is bounded.
const MAX_EVIDENCE_CHALLENGE_MS: u64 = 900_000;
/// Default one-use evidence challenge TTL when the operator omits the knob.
const DEFAULT_EVIDENCE_CHALLENGE_MS: u64 = 300_000;
/// Provider attestations stay small enough to keep ingress memory finite.
const MAX_EVIDENCE_BYTES: usize = 64 * 1024;
/// Default attestation cap when the operator omits the knob.
const DEFAULT_EVIDENCE_BYTES: usize = 16 * 1024;
const MAX_LOG_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LOG_FILES: usize = 32;

/// Key identifiers are small audit-safe coordinates, never key material.
const MAX_KEY_ID_BYTES: usize = 64;

/// Explicit retention and destination for structured relay logs.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    pub directory: PathBuf,
    pub max_file_bytes: u64,
    pub max_files: usize,
}

impl Debug for LogConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogConfig")
            .field("directory", &"REDACTED_PATH")
            .field("max_file_bytes", &self.max_file_bytes)
            .field("max_files", &self.max_files)
            .finish()
    }
}

/// Loaded relay configuration.
#[derive(Clone)]
pub struct Config {
    pub relay_id: String,
    pub bind: std::net::SocketAddr,
    pub public_origin: Url,
    pub callback: Url,
    pub tls: Tls,
    pub database_url_file: PathBuf,
    pub digest_key_file: PathBuf,
    pub digest_key_id: String,
    pub witness_key_file: PathBuf,
    pub witness_key_id: String,
    pub issuer: Url,
    pub client_id: String,
    pub ca_file: Option<PathBuf>,
    pub witness_dir: PathBuf,
    pub limits: Limits,
    pub logging: LogConfig,
    pub auth: AuthConfig,
    pub evidence: EvidenceConfig,
    pub login_policy: LoginPolicy,
}

impl Debug for Config {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("relay_id", &self.relay_id)
            .field("bind", &self.bind)
            .field("public_origin", &self.public_origin)
            .field("callback", &self.callback)
            .field("tls", &self.tls)
            .field("database_url_file", &"REDACTED_PATH")
            .field("digest_key_file", &"REDACTED_PATH")
            .field("witness_key_file", &"REDACTED_PATH")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("ca_file", &self.ca_file.as_ref().map(|_| "REDACTED_PATH"))
            .field("witness_dir", &"REDACTED_PATH")
            .field("limits", &self.limits)
            .field("auth", &self.auth)
            .field("evidence", &self.evidence)
            .field("login_policy", &self.login_policy)
            .finish_non_exhaustive()
    }
}

/// Transport termination mode.
#[derive(Clone)]
pub enum Tls {
    Native {
        certificate_file: PathBuf,
        private_key_file: PathBuf,
    },
    LoopbackProxy,
}

/// Explicit human login admission policy.
#[derive(Clone)]
pub enum LoginPolicy {
    AnyAuthenticatedSubject,
    AllowedSubjects(Vec<String>),
}

impl Debug for Tls {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native { .. } => f.write_str("Tls::Native(REDACTED_PATHS)"),
            Self::LoopbackProxy => f.write_str("Tls::LoopbackProxy"),
        }
    }
}

impl Debug for LoginPolicy {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AnyAuthenticatedSubject => f.write_str("AnyAuthenticatedSubject"),
            Self::AllowedSubjects(subjects) => f
                .debug_struct("AllowedSubjects")
                .field("count", &subjects.len())
                .finish_non_exhaustive(),
        }
    }
}

/// Bounded public ingress limits.
#[derive(Debug, Clone)]
pub struct Limits {
    pub body_bytes: usize,
    pub response_bytes: usize,
    pub connections: usize,
    pub backlog: u32,
    pub database_connections: u32,
    pub fence_connections: u32,
    pub reserved_connections: u32,
    pub per_team: usize,
    pub per_principal: usize,
    pub requests_per_window: u32,
    pub rate_window: Duration,
    pub connection_lifetime: Duration,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub lease_renew: Duration,
    pub idle_recheck: Duration,
}

/// Reports only the unsafe configuration field.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid relay configuration field: {field}")]
    Invalid { field: &'static str },
    #[error("failed to read relay configuration field: {field}")]
    Read { field: &'static str },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    relay_id: String,
    bind: String,
    public_origin: String,
    database_url_file: PathBuf,
    digest_key_file: PathBuf,
    digest_key_id: String,
    witness_key_file: PathBuf,
    witness_key_id: String,
    oidc: Oidc,
    tls: RawTls,
    witness_dir: PathBuf,
    limits: RawLimits,
    logging: LogConfig,
    auth: RawAuth,
    evidence: RawEvidence,
    login_policy: RawLoginPolicy,
}
/// Required bounded evidence-admission knobs for the internal mTLS endpoint.
///
/// The pinned files authenticate the evidence broker before any attestation is
/// accepted. The challenge TTL bounds one-use replay exposure, and the byte cap
/// keeps provider attestations finite at ingress.
#[derive(Clone)]
pub struct EvidenceConfig {
    pub signing_keys_file: PathBuf,
    pub internal_bind: std::net::SocketAddr,
    pub internal_client_ca_file: PathBuf,
    pub challenge_lifetime: Duration,
    pub max_evidence_bytes: usize,
}

impl Debug for EvidenceConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvidenceConfig")
            .field("signing_keys_file", &"REDACTED_PATH")
            .field("internal_bind", &self.internal_bind)
            .field("internal_client_ca_file", &"REDACTED_PATH")
            .field("challenge_lifetime", &self.challenge_lifetime)
            .field("max_evidence_bytes", &self.max_evidence_bytes)
            .finish()
    }
}

impl EvidenceConfig {
    /// Validates explicitly configured evidence-admission knobs.
    ///
    /// # Errors
    /// Returns [`ConfigError::Invalid`] when a pinned path is empty, the
    /// challenge TTL is zero or above its ceiling, or the attestation cap is
    /// zero or above its ceiling.
    pub fn new(
        signing_keys_file: PathBuf,
        internal_bind: std::net::SocketAddr,
        internal_client_ca_file: PathBuf,
        challenge_lifetime: Duration,
        max_evidence_bytes: usize,
    ) -> Result<Self, ConfigError> {
        if signing_keys_file.as_os_str().is_empty()
            || internal_client_ca_file.as_os_str().is_empty()
            || challenge_lifetime.is_zero()
            || challenge_lifetime > Duration::from_millis(MAX_EVIDENCE_CHALLENGE_MS)
            || max_evidence_bytes == 0
            || max_evidence_bytes > MAX_EVIDENCE_BYTES
        {
            return Err(ConfigError::Invalid { field: "evidence" });
        }
        Ok(Self {
            signing_keys_file,
            internal_bind,
            internal_client_ca_file,
            challenge_lifetime,
            max_evidence_bytes,
        })
    }
}

/// Required bounded authentication lifetimes and transaction budgets.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub pending_transactions: usize,
    pub login_lifetime: Duration,
    pub browser_session_lifetime: Duration,
    pub browser_session_idle: Duration,
    pub human_credential_lifetime: Duration,
    pub service_credential_lifetime: Duration,
    pub max_rotation_overlap: Duration,
    pub credentials_per_principal: usize,
    pub service_accounts_per_team: usize,
    pub device_poll_lease: Duration,
    pub device_slow_down_increment: Duration,
    pub max_device_poll_interval: Duration,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuth {
    pending_transactions: usize,
    login_lifetime_ms: u64,
    browser_session_lifetime_ms: u64,
    browser_session_idle_ms: u64,
    human_credential_lifetime_ms: u64,
    service_credential_lifetime_ms: u64,
    max_rotation_overlap_ms: u64,
    credentials_per_principal: usize,
    service_accounts_per_team: usize,
    device_poll_lease_ms: u64,
    device_slow_down_increment_ms: u64,
    max_device_poll_interval_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEvidence {
    signing_keys_file: PathBuf,
    internal_bind: String,
    internal_client_ca_file: PathBuf,
    #[serde(default = "default_evidence_challenge_ms")]
    challenge_lifetime_ms: u64,
    #[serde(default = "default_evidence_bytes")]
    max_evidence_bytes: usize,
}

fn default_evidence_challenge_ms() -> u64 {
    DEFAULT_EVIDENCE_CHALLENGE_MS
}

fn default_evidence_bytes() -> usize {
    DEFAULT_EVIDENCE_BYTES
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Oidc {
    issuer: String,
    client_id: String,
    ca_file: Option<PathBuf>,
}
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum RawTls {
    Native {
        certificate_file: PathBuf,
        private_key_file: PathBuf,
    },
    LoopbackProxy,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    body_bytes: usize,
    response_bytes: usize,
    connections: usize,
    backlog: u32,
    database_connections: u32,
    fence_connections: u32,
    reserved_connections: u32,
    per_team: usize,
    per_principal: usize,
    requests_per_window: u32,
    rate_window_ms: u64,
    connection_lifetime_ms: u64,
    request_timeout_ms: u64,
    shutdown_timeout_ms: u64,
    lease_renew_ms: u64,
    idle_recheck_ms: u64,
}
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum RawLoginPolicy {
    AnyAuthenticatedSubject,
    AllowedSubjects { subjects: Vec<String> },
}

impl Config {
    /// Loads and validates one owner-private TOML configuration file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw: Raw =
            toml::from_str(&read_private_text(path, "config_file")?).map_err(|_error| {
                ConfigError::Invalid {
                    field: "config_file",
                }
            })?;
        validate_runtime_settings(&raw)?;
        let public_origin = exact_https(&raw.public_origin, "public_origin")?;
        let callback = public_origin
            .join("v1/auth/oidc/callback")
            .map_err(|_error| ConfigError::Invalid {
                field: "public_origin",
            })?;
        let bind: std::net::SocketAddr = raw
            .bind
            .parse()
            .map_err(|_error| ConfigError::Invalid { field: "bind" })?;
        require_private_file(&raw.database_url_file, "database_url_file")?;
        require_private_file(&raw.digest_key_file, "digest_key_file")?;
        require_private_file(&raw.witness_key_file, "witness_key_file")?;
        require_private_dir(&raw.witness_dir, "witness_dir")?;
        let tls = match raw.tls {
            RawTls::Native {
                certificate_file,
                private_key_file,
            } => {
                require_private_file(&certificate_file, "tls.certificate_file")?;
                require_private_file(&private_key_file, "tls.private_key_file")?;
                Tls::Native {
                    certificate_file,
                    private_key_file,
                }
            }
            RawTls::LoopbackProxy => {
                if !bind.ip().is_loopback() {
                    return Err(ConfigError::Invalid { field: "tls.mode" });
                }
                Tls::LoopbackProxy
            }
        };
        let issuer = issuer_https(&raw.oidc.issuer, "oidc.issuer")?;
        if raw.oidc.client_id.is_empty() {
            return Err(ConfigError::Invalid {
                field: "oidc.client_id",
            });
        }
        if let Some(path) = &raw.oidc.ca_file {
            require_private_file(path, "oidc.ca_file")?;
        }
        let limits = limits(&raw.limits)?;
        let auth = auth_limits(&raw.auth)?;
        let evidence = evidence_limits(&raw.evidence)?;
        let login_policy = match raw.login_policy {
            RawLoginPolicy::AnyAuthenticatedSubject => LoginPolicy::AnyAuthenticatedSubject,
            RawLoginPolicy::AllowedSubjects { subjects } => {
                if subjects.is_empty() || subjects.iter().any(String::is_empty) {
                    return Err(ConfigError::Invalid {
                        field: "login_policy.subjects",
                    });
                }
                LoginPolicy::AllowedSubjects(subjects)
            }
        };
        if relay_protocol::RelayId::parse(&raw.relay_id).is_err() {
            return Err(ConfigError::Invalid { field: "relay_id" });
        }
        Ok(Self {
            relay_id: raw.relay_id,
            bind,
            public_origin,
            callback,
            tls,
            database_url_file: raw.database_url_file,
            digest_key_file: raw.digest_key_file,
            digest_key_id: raw.digest_key_id,
            witness_key_file: raw.witness_key_file,
            witness_key_id: raw.witness_key_id,
            issuer,
            client_id: raw.oidc.client_id,
            ca_file: raw.oidc.ca_file,
            witness_dir: raw.witness_dir,
            limits,
            logging: raw.logging,
            auth,
            evidence,
            login_policy,
        })
    }

    /// Builds the exact authentication limits consumed by the auth service.
    pub fn auth_limits(&self) -> Result<crate::auth::AuthLimits, ConfigError> {
        crate::auth::AuthLimits::new(
            self.auth.login_lifetime,
            self.auth.browser_session_lifetime,
            self.auth.browser_session_idle,
            self.auth.human_credential_lifetime,
            self.auth.service_credential_lifetime,
            self.auth.max_rotation_overlap,
            self.auth.credentials_per_principal,
            self.auth.service_accounts_per_team,
            self.auth.device_poll_lease,
            self.auth.device_slow_down_increment,
            self.auth.max_device_poll_interval,
            self.evidence.challenge_lifetime,
        )
        .map_err(|_error| ConfigError::Invalid { field: "auth" })
    }
}
fn validate_runtime_settings(raw: &Raw) -> Result<(), ConfigError> {
    for (value, field) in [
        (&raw.digest_key_id, "digest_key_id"),
        (&raw.witness_key_id, "witness_key_id"),
    ] {
        if value.is_empty()
            || value.len() > MAX_KEY_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        {
            return Err(ConfigError::Invalid { field });
        }
    }
    if raw.logging.max_file_bytes == 0
        || raw.logging.max_file_bytes > MAX_LOG_FILE_BYTES
        || raw.logging.max_files > MAX_LOG_FILES
        || raw.logging.max_files == 0
        || !raw.logging.directory.is_absolute()
    {
        return Err(ConfigError::Invalid { field: "logging" });
    }
    Ok(())
}

fn auth_limits(raw: &RawAuth) -> Result<AuthConfig, ConfigError> {
    if raw.pending_transactions == 0
        || raw.pending_transactions > MAX_PENDING_LOGINS
        || raw.login_lifetime_ms > MAX_LOGIN_LIFETIME_MS
        || raw.browser_session_lifetime_ms > MAX_BROWSER_LIFETIME_MS
        || raw.browser_session_idle_ms > MAX_BROWSER_IDLE_MS
        || raw.human_credential_lifetime_ms > MAX_HUMAN_LIFETIME_MS
        || raw.service_credential_lifetime_ms > MAX_SERVICE_CREDENTIAL_LIFETIME_MS
        || raw.max_rotation_overlap_ms > MAX_ROTATION_OVERLAP_MS
        || raw.credentials_per_principal == 0
        || raw.credentials_per_principal > MAX_CREDENTIALS_PER_PRINCIPAL
        || raw.service_accounts_per_team == 0
        || raw.service_accounts_per_team > MAX_SERVICE_ACCOUNTS_PER_TEAM
        || raw.device_poll_lease_ms > MAX_DEVICE_POLL_LEASE_MS
        || raw.device_poll_lease_ms > raw.login_lifetime_ms
        || raw.device_slow_down_increment_ms > MAX_DEVICE_INTERVAL_MS
        || raw.max_device_poll_interval_ms > MAX_DEVICE_INTERVAL_MS
        || raw.max_device_poll_interval_ms > raw.login_lifetime_ms
        || [
            raw.login_lifetime_ms,
            raw.browser_session_lifetime_ms,
            raw.browser_session_idle_ms,
            raw.human_credential_lifetime_ms,
            raw.service_credential_lifetime_ms,
            raw.max_rotation_overlap_ms,
            raw.device_poll_lease_ms,
            raw.device_slow_down_increment_ms,
            raw.max_device_poll_interval_ms,
        ]
        .contains(&0)
    {
        return Err(ConfigError::Invalid { field: "auth" });
    }
    let result = AuthConfig {
        pending_transactions: raw.pending_transactions,
        login_lifetime: Duration::from_millis(raw.login_lifetime_ms),
        browser_session_lifetime: Duration::from_millis(raw.browser_session_lifetime_ms),
        browser_session_idle: Duration::from_millis(raw.browser_session_idle_ms),
        human_credential_lifetime: Duration::from_millis(raw.human_credential_lifetime_ms),
        service_credential_lifetime: Duration::from_millis(raw.service_credential_lifetime_ms),
        max_rotation_overlap: Duration::from_millis(raw.max_rotation_overlap_ms),
        credentials_per_principal: raw.credentials_per_principal,
        service_accounts_per_team: raw.service_accounts_per_team,
        device_poll_lease: Duration::from_millis(raw.device_poll_lease_ms),
        device_slow_down_increment: Duration::from_millis(raw.device_slow_down_increment_ms),
        max_device_poll_interval: Duration::from_millis(raw.max_device_poll_interval_ms),
    };
    if result.browser_session_idle > result.browser_session_lifetime
        || result.device_slow_down_increment > result.max_device_poll_interval
        || result.max_rotation_overlap > result.human_credential_lifetime
        || result.max_rotation_overlap > result.service_credential_lifetime
    {
        return Err(ConfigError::Invalid { field: "auth" });
    }
    Ok(result)
}

fn evidence_limits(raw: &RawEvidence) -> Result<EvidenceConfig, ConfigError> {
    require_private_file(&raw.signing_keys_file, "evidence.signing_keys_file")?;
    require_private_file(
        &raw.internal_client_ca_file,
        "evidence.internal_client_ca_file",
    )?;
    let internal_bind: std::net::SocketAddr =
        raw.internal_bind
            .parse()
            .map_err(|_error| ConfigError::Invalid {
                field: "evidence.internal_bind",
            })?;
    if !internal_bind.ip().is_loopback() {
        return Err(ConfigError::Invalid {
            field: "evidence.internal_bind",
        });
    }
    EvidenceConfig::new(
        raw.signing_keys_file.clone(),
        internal_bind,
        raw.internal_client_ca_file.clone(),
        Duration::from_millis(raw.challenge_lifetime_ms),
        raw.max_evidence_bytes,
    )
}

fn exact_https(value: &str, field: &'static str) -> Result<Url, ConfigError> {
    let url = Url::parse(value).map_err(|_error| ConfigError::Invalid { field })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(ConfigError::Invalid { field });
    }
    Ok(url)
}
fn issuer_https(value: &str, field: &'static str) -> Result<Url, ConfigError> {
    let url = Url::parse(value).map_err(|_error| ConfigError::Invalid { field })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::Invalid { field });
    }
    Ok(url)
}
fn limits(raw: &RawLimits) -> Result<Limits, ConfigError> {
    if raw.body_bytes == 0
        || raw.body_bytes > MAX_BODY_BYTES
        || raw.response_bytes > MAX_RESPONSE_BYTES
        || raw.response_bytes == 0
        || raw.connections == 0
        || raw.connections > tokio::sync::Semaphore::MAX_PERMITS
        || raw.connections > MAX_CONNECTIONS
        || raw.backlog == 0
        || raw.backlog > MAX_BACKLOG
        || raw.database_connections > MAX_DATABASE_CONNECTIONS
        || raw.fence_connections < 2
        || raw.fence_connections > MAX_CONTROL_CONNECTIONS
        || raw.reserved_connections == 0
        || raw.reserved_connections > MAX_CONTROL_CONNECTIONS
        || raw.requests_per_window > MAX_REQUESTS_PER_WINDOW
        || raw.rate_window_ms > MAX_RATE_WINDOW_MS
        || raw.connection_lifetime_ms > MAX_CONNECTION_LIFETIME_MS
        || raw.request_timeout_ms > MAX_REQUEST_TIMEOUT_MS
        || raw.database_connections == 0
        || raw.per_team == 0
        || raw.per_team > raw.connections
        || raw.per_principal == 0
        || raw.per_principal > raw.per_team
        || raw.requests_per_window == 0
        || raw.rate_window_ms == 0
        || raw.connection_lifetime_ms == 0
        || raw.request_timeout_ms == 0
        || raw.shutdown_timeout_ms == 0
        || raw.lease_renew_ms == 0
        || raw.idle_recheck_ms == 0
    {
        return Err(ConfigError::Invalid { field: "limits" });
    }
    let lease_renew = Duration::from_millis(raw.lease_renew_ms);
    let idle_recheck = Duration::from_millis(raw.idle_recheck_ms);
    if lease_renew > MAX_LEASE_RENEW
        || idle_recheck > MAX_IDLE_RECHECK
        || Duration::from_millis(raw.shutdown_timeout_ms) >= LEASE_LIFETIME
    {
        return Err(ConfigError::Invalid { field: "limits" });
    }
    Ok(Limits {
        body_bytes: raw.body_bytes,
        response_bytes: raw.response_bytes,
        connections: raw.connections,
        backlog: raw.backlog,
        database_connections: raw.database_connections,
        fence_connections: raw.fence_connections,
        reserved_connections: raw.reserved_connections,
        per_team: raw.per_team,
        per_principal: raw.per_principal,
        requests_per_window: raw.requests_per_window,
        rate_window: Duration::from_millis(raw.rate_window_ms),
        connection_lifetime: Duration::from_millis(raw.connection_lifetime_ms),
        request_timeout: Duration::from_millis(raw.request_timeout_ms),
        shutdown_timeout: Duration::from_millis(raw.shutdown_timeout_ms),
        lease_renew,
        idle_recheck,
    })
}
fn require_private_file(path: &Path, field: &'static str) -> Result<(), ConfigError> {
    open_private_file(path, field).map(|_file| ())
}
fn require_private_dir(path: &Path, field: &'static str) -> Result<(), ConfigError> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_error| ConfigError::Read { field })?;
    let metadata = directory
        .metadata()
        .map_err(|_error| ConfigError::Read { field })?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_DIRECTORY_MODE
    {
        return Err(ConfigError::Invalid { field });
    }
    Ok(())
}

fn open_private_file(path: &Path, field: &'static str) -> Result<File, ConfigError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_error| ConfigError::Read { field })?;
    let metadata = file
        .metadata()
        .map_err(|_error| ConfigError::Read { field })?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE
        || metadata.len() > MAX_PRIVATE_FILE_BYTES
    {
        return Err(ConfigError::Invalid { field });
    }
    Ok(file)
}

/// Opens, validates, and reads a private bounded file at its point of use.
pub fn read_private_text(path: &Path, field: &'static str) -> Result<String, ConfigError> {
    let file = open_private_file(path, field)?;
    let mut bytes = Vec::new();
    file.take(MAX_PRIVATE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_error| ConfigError::Read { field })?;
    if bytes.len() as u64 > MAX_PRIVATE_FILE_BYTES {
        return Err(ConfigError::Invalid { field });
    }
    String::from_utf8(bytes).map_err(|_error| ConfigError::Invalid { field })
}

/// Opens, validates, and reads one bounded private binary key at its point of use.
pub fn read_private_bytes(path: &Path, field: &'static str) -> Result<Vec<u8>, ConfigError> {
    let file = open_private_file(path, field)?;
    let mut bytes = Vec::new();
    file.take(MAX_PRIVATE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_error| ConfigError::Read { field })?;
    if bytes.len() as u64 > MAX_PRIVATE_FILE_BYTES {
        return Err(ConfigError::Invalid { field });
    }
    Ok(bytes)
}

#[cfg(test)]
pub(crate) fn fixture_limits() -> Limits {
    Limits {
        body_bytes: 8,
        response_bytes: 16,
        connections: 1,
        backlog: 16,
        database_connections: 1,
        fence_connections: 2,
        reserved_connections: 1,
        per_team: 1,
        per_principal: 1,
        requests_per_window: 2,
        rate_window: Duration::from_secs(10),
        connection_lifetime: Duration::from_secs(30),
        request_timeout: Duration::from_millis(20),
        shutdown_timeout: Duration::from_secs(1),
        lease_renew: Duration::from_secs(1),
        idle_recheck: Duration::from_millis(100),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::time::Duration;

    use super::{exact_https, issuer_https, require_private_file};

    pub(crate) fn config_fixture() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let directory = tempfile::tempdir().expect("configuration fixture");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private directory");
        let key = directory.path().join("fixture-key");
        fs::write(&key, [7_u8; 32]).expect("fixture key");
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("private file");
        let signing_keys = directory.path().join("signing-keys");
        fs::write(&signing_keys, [9_u8; 32]).expect("signing keys");
        fs::set_permissions(&signing_keys, fs::Permissions::from_mode(0o600))
            .expect("private file");
        let client_ca = directory.path().join("evidence-client-ca");
        fs::write(&client_ca, [11_u8; 32]).expect("client CA");
        fs::set_permissions(&client_ca, fs::Permissions::from_mode(0o600)).expect("private file");
        let path = directory.path().join("relay.toml");
        let config = format!(
            r#"
relay_id = "relay_{}"
bind = "127.0.0.1:0"
public_origin = "https://relay.example/"
database_url_file = "{}"
digest_key_file = "{}"
digest_key_id = "digest-1"
witness_key_file = "{}"
witness_key_id = "witness-1"
witness_dir = "{}"
[oidc]
issuer = "https://issuer.example/realms/test"
client_id = "relay-client"
[tls]
mode = "loopback_proxy"
[login_policy]
mode = "any_authenticated_subject"
[logging]
directory = "{}"
max_file_bytes = 1048576
max_files = 3
[limits]
body_bytes = 8192
response_bytes = 65536
connections = 32
backlog = 64
database_connections = 8
fence_connections = 2
reserved_connections = 2
per_team = 16
per_principal = 4
requests_per_window = 100
rate_window_ms = 1000
connection_lifetime_ms = 60000
request_timeout_ms = 2000
shutdown_timeout_ms = 3000
lease_renew_ms = 1000
idle_recheck_ms = 100
[auth]
pending_transactions = 32
login_lifetime_ms = 60000
browser_session_lifetime_ms = 3600000
browser_session_idle_ms = 600000
human_credential_lifetime_ms = 86400000
service_credential_lifetime_ms = 86400000
max_rotation_overlap_ms = 60000
credentials_per_principal = 8
service_accounts_per_team = 16
device_poll_lease_ms = 5000
device_slow_down_increment_ms = 5000
max_device_poll_interval_ms = 30000
[evidence]
signing_keys_file = "{}"
internal_bind = "127.0.0.1:0"
internal_client_ca_file = "{}"
challenge_lifetime_ms = 300000
max_evidence_bytes = 16384
"#,
            "A".repeat(43),
            key.display(),
            key.display(),
            key.display(),
            directory.path().display(),
            directory.path().join("logs").display(),
            signing_keys.display(),
            client_ca.display()
        );
        fs::write(&path, &config).expect("write config");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private config");
        (directory, path, config)
    }

    #[test]
    fn explicit_runtime_budgets_load_and_missing_or_unsafe_values_fail_fast() {
        let (_directory, path, source) = config_fixture();
        let config = super::Config::load(&path).expect("complete config");
        assert_eq!(config.limits.database_connections, 8);
        assert_eq!(config.limits.per_principal, 4);
        assert_eq!(config.digest_key_id, "digest-1");
        assert_eq!(config.evidence.challenge_lifetime, Duration::from_mins(5));
        assert_eq!(config.evidence.max_evidence_bytes, 16_384);
        assert_eq!(
            config
                .auth_limits()
                .expect("derived auth limits")
                .evidence_challenge_lifetime(),
            Duration::from_mins(5)
        );
        for (before, after) in [
            ("database_connections = 8", ""),
            ("database_connections = 8", "database_connections = 0"),
            ("per_team = 16", "per_team = 33"),
            ("per_principal = 4", "per_principal = 17"),
            ("requests_per_window = 100", "requests_per_window = 0"),
            ("shutdown_timeout_ms = 3000", "shutdown_timeout_ms = 5000"),
            ("lease_renew_ms = 1000", "lease_renew_ms = 1001"),
            ("idle_recheck_ms = 100", "idle_recheck_ms = 101"),
            (
                "digest_key_id = \"digest-1\"",
                "digest_key_id = \"unsafe key id\"",
            ),
            ("bind = \"127.0.0.1:0\"", "bind = \"0.0.0.0:0\""),
            ("max_files = 3", "max_files = 0"),
            ("service_credential_lifetime_ms = 86400000", ""),
            (
                "max_rotation_overlap_ms = 60000",
                "max_rotation_overlap_ms = 0",
            ),
            (
                "credentials_per_principal = 8",
                "credentials_per_principal = 0",
            ),
            ("service_accounts_per_team = 16", ""),
            ("[evidence]", ""),
            (
                "challenge_lifetime_ms = 300000",
                "challenge_lifetime_ms = 0",
            ),
            ("max_evidence_bytes = 16384", "max_evidence_bytes = 0"),
            (
                "internal_bind = \"127.0.0.1:0\"",
                "internal_bind = \"0.0.0.0:0\"",
            ),
        ] {
            fs::write(&path, source.replace(before, after)).expect("write invalid config");
            super::Config::load(&path).expect_err("unsafe or missing setting rejected");
        }
    }

    #[test]
    fn resource_and_replay_limits_reject_the_first_value_above_each_ceiling() {
        let (_directory, path, source) = config_fixture();
        for (field, maximum) in [
            ("body_bytes", super::MAX_BODY_BYTES as u64),
            ("response_bytes", super::MAX_RESPONSE_BYTES as u64),
            ("connections", super::MAX_CONNECTIONS as u64),
            ("backlog", u64::from(super::MAX_BACKLOG)),
            (
                "database_connections",
                u64::from(super::MAX_DATABASE_CONNECTIONS),
            ),
            (
                "fence_connections",
                u64::from(super::MAX_CONTROL_CONNECTIONS),
            ),
            (
                "reserved_connections",
                u64::from(super::MAX_CONTROL_CONNECTIONS),
            ),
            (
                "requests_per_window",
                u64::from(super::MAX_REQUESTS_PER_WINDOW),
            ),
            ("rate_window_ms", super::MAX_RATE_WINDOW_MS),
            ("connection_lifetime_ms", super::MAX_CONNECTION_LIFETIME_MS),
            ("request_timeout_ms", super::MAX_REQUEST_TIMEOUT_MS),
            ("pending_transactions", super::MAX_PENDING_LOGINS as u64),
            ("login_lifetime_ms", super::MAX_LOGIN_LIFETIME_MS),
            (
                "browser_session_lifetime_ms",
                super::MAX_BROWSER_LIFETIME_MS,
            ),
            ("browser_session_idle_ms", super::MAX_BROWSER_IDLE_MS),
            ("human_credential_lifetime_ms", super::MAX_HUMAN_LIFETIME_MS),
            (
                "service_credential_lifetime_ms",
                super::MAX_SERVICE_CREDENTIAL_LIFETIME_MS,
            ),
            ("max_rotation_overlap_ms", super::MAX_ROTATION_OVERLAP_MS),
            ("device_poll_lease_ms", super::MAX_DEVICE_POLL_LEASE_MS),
            ("max_device_poll_interval_ms", super::MAX_DEVICE_INTERVAL_MS),
            ("challenge_lifetime_ms", super::MAX_EVIDENCE_CHALLENGE_MS),
            ("max_evidence_bytes", super::MAX_EVIDENCE_BYTES as u64),
            ("max_file_bytes", super::MAX_LOG_FILE_BYTES),
            ("max_files", super::MAX_LOG_FILES as u64),
        ] {
            let prefix = format!("{field} = ");
            let invalid = source
                .lines()
                .map(|line| {
                    if line.starts_with(&prefix) {
                        format!("{prefix}{}", maximum + 1)
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(&path, invalid).expect("over-limit config");
            super::Config::load(&path).expect_err("finite deployment maximum enforced");
        }
    }

    #[test]
    fn debug_redacts_nested_paths_and_allowed_subjects() {
        let (_directory, path, _source) = config_fixture();
        let mut config = super::Config::load(&path).expect("config");
        config.tls = super::Tls::Native {
            certificate_file: "/sentinel-certificate".into(),
            private_key_file: "/sentinel-private-key".into(),
        };
        config.login_policy =
            super::LoginPolicy::AllowedSubjects(vec!["sentinel-subject".to_owned()]);
        config.evidence.signing_keys_file = "/sentinel-signing-keys".into();
        config.evidence.internal_client_ca_file = "/sentinel-evidence-ca".into();
        assert!(!format!("{config:?}").contains("sentinel"));
    }

    #[test]
    fn evidence_defaults_apply_and_missing_evidence_fails_fast() {
        let (directory, path, source) = config_fixture();
        let without_optional = source
            .lines()
            .filter(|line| {
                !line.starts_with("challenge_lifetime_ms = ")
                    && !line.starts_with("max_evidence_bytes = ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, without_optional).expect("evidence defaults config");
        let config = super::Config::load(&path).expect("evidence defaults load");
        assert_eq!(
            config.evidence.challenge_lifetime,
            Duration::from_millis(super::DEFAULT_EVIDENCE_CHALLENGE_MS)
        );
        assert_eq!(
            config.evidence.max_evidence_bytes,
            super::DEFAULT_EVIDENCE_BYTES
        );

        let without_section = source
            .lines()
            .filter(|line| {
                *line != "[evidence]"
                    && !line.starts_with("signing_keys_file = ")
                    && !line.starts_with("internal_bind = ")
                    && !line.starts_with("internal_client_ca_file = ")
                    && !line.starts_with("challenge_lifetime_ms = ")
                    && !line.starts_with("max_evidence_bytes = ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, without_section).expect("missing evidence config");
        super::Config::load(&path).expect_err("missing evidence section rejected");
        let _guard = directory;
    }

    #[test]
    fn evidence_rejects_invalid_values() {
        for (before, after) in [
            (
                "signing_keys_file = ",
                "signing_keys_file = \"/nonexistent-signing-keys\"",
            ),
            (
                "internal_bind = \"127.0.0.1:0\"",
                "internal_bind = \"not-a-socket\"",
            ),
            (
                "internal_client_ca_file = ",
                "internal_client_ca_file = \"/nonexistent-client-ca\"",
            ),
        ] {
            let (_directory, path, source) = config_fixture();
            let invalid = source
                .lines()
                .map(|line| {
                    if line.starts_with(before) {
                        after.to_owned()
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(&path, invalid).expect("invalid evidence config");
            super::Config::load(&path).expect_err("invalid evidence rejected");
        }
        super::EvidenceConfig::new(
            "".into(),
            "127.0.0.1:0".parse().expect("evidence bind"),
            "/evidence-ca".into(),
            Duration::from_mins(5),
            16_384,
        )
        .expect_err("empty signing keys rejected");
    }

    #[test]
    fn origin_requires_exact_https_root() {
        for value in [
            "http://relay.example/",
            "https://user@relay.example/",
            "https://relay.example/?q=1",
            "https://relay.example/path",
        ] {
            exact_https(value, "public_origin").expect_err("noncanonical origin");
        }
        exact_https("https://relay.example/", "public_origin").expect("valid origin");
    }

    #[test]
    fn issuer_allows_a_realm_path_but_not_credential_or_query_coordinates() {
        issuer_https("https://keycloak.example/realms/pohunek", "oidc.issuer")
            .expect("valid issuer");
        issuer_https(
            "https://user@keycloak.example/realms/pohunek",
            "oidc.issuer",
        )
        .expect_err("invalid issuer");
        issuer_https(
            "https://keycloak.example/realms/pohunek?next=x",
            "oidc.issuer",
        )
        .expect_err("invalid issuer");
    }

    #[test]
    fn private_file_rejects_a_symlink_after_opening_by_descriptor() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("target");
        fs::write(&target, "private").expect("target contents");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))
            .expect("target permissions");
        let link = directory.path().join("link");
        symlink(&target, &link).expect("symlink");

        require_private_file(&link, "test").expect_err("symlink refused");
    }

    #[test]
    fn private_file_requires_owner_only_permissions() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("public");
        fs::write(&path, "not private").expect("file contents");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("file permissions");

        require_private_file(&path, "test").expect_err("public file refused");
    }
}

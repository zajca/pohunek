//! Serves bounded relay HTTP health and authentication ingress.

// Rust guideline compliant 2026-09-08

mod limits;
pub(crate) mod transport;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use axum::{
    extract::{RawQuery, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::{
    ApiError, ApiErrorBody, CreateServiceAccountRequest, CredentialId, DeviceCredential,
    DeviceLoginStart, DevicePollResult, LoginId, PageRequest, RevokeCredentialRequest,
    RotateCredentialRequest, Secret, TeamId,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    admission::Authority,
    auth::{
        AuthError, AuthService, BrowserCallback, BrowserCallbackOutcome, BrowserCookie,
        DevicePollSecret, LoginBindingCookie, OidcCallbackCode, OidcClient, RelayBearerCredential,
    },
    config::Config,
    store::{Store, StoreError},
};

const LOGIN_COOKIE: &str = "__Host-pohunek-relay-login";
const SESSION_COOKIE: &str = "__Host-pohunek-relay-session";
const CSRF_COOKIE: &str = "__Host-pohunek-relay-csrf";
const DEVICE_SECRET_HEADER: &str = "x-pohunek-device-secret";
/// Browser login bindings use 256 bits so another origin cannot feasibly plant one.
const BROWSER_BINDING_BYTES: usize = 32;

/// Shared state whose callers are resolved by the authentication service.
#[derive(Clone, Debug)]
pub struct ServerState {
    config: Arc<Config>,
    store: Store,
    auth: AuthService,
    oidc: OidcClient,
    authority: Arc<Authority>,
    ingress_open: Arc<AtomicBool>,
}

impl ServerState {
    /// Composes the HTTP surface from validated process-owned dependencies.
    #[must_use]
    pub fn new(
        config: Config,
        store: Store,
        auth: AuthService,
        oidc: OidcClient,
        authority: Arc<Authority>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            store,
            auth,
            oidc,
            authority,
            ingress_open: Arc::new(AtomicBool::new(true)),
        }
    }
    /// Stops readiness and privileged ingress after authority loss.
    pub fn close_ingress(&self) {
        self.ingress_open.store(false, Ordering::Release);
        self.authority.close_all();
    }
}

/// Builds the bounded public relay API.
pub fn router(state: ServerState) -> Router {
    let limits = state.config.limits.clone();
    let app = Router::new()
        .route("/healthz", get(liveness))
        .route("/readyz", get(readiness))
        .route("/account/ready", get(account_ready))
        .route("/v1/auth/browser/start", post(browser_start))
        .route("/v1/auth/oidc/callback", get(browser_callback))
        .route("/v1/auth/device/start", post(device_start))
        .route("/v1/auth/device/poll", post(device_poll))
        .route("/v1/account", get(account))
        .route("/v1/account/credentials", get(list_credentials))
        .route(
            "/v1/account/credentials/{credential_id}/rotate",
            post(rotate_credential),
        )
        .route(
            "/v1/account/credentials/{credential_id}/revoke",
            post(revoke_credential),
        )
        .route(
            "/v1/auth/credentials/self/revoke",
            post(revoke_current_credential),
        )
        .route(
            "/v1/teams/{team_id}/service-accounts",
            post(create_service_account).get(list_service_accounts),
        )
        .route(
            "/v1/teams/{team_id}/service-accounts/{principal_id}/credentials/{credential_id}/revoke",
            post(revoke_service_credential),
        )
        .route(
            "/v1/teams/{team_id}/service-accounts/{principal_id}/credentials/{credential_id}/rotate",
            post(rotate_service_credential),
        )
        .with_state(state);
    limits::apply(app, &limits)
}

async fn liveness() -> StatusCode {
    StatusCode::NO_CONTENT
}
async fn readiness(State(state): State<ServerState>) -> Result<StatusCode, ApiFailure> {
    gate(&state)?;
    state
        .authority
        .validate_fence()
        .await
        .map_err(|_error| ApiFailure::unavailable())?;
    state
        .store
        .healthcheck()
        .await
        .map_err(|error| ApiFailure::store(&error))?;
    Ok(StatusCode::NO_CONTENT)
}
async fn account_ready(State(state): State<ServerState>) -> Result<Html<&'static str>, ApiFailure> {
    gate(&state)?;
    Ok(Html("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>Pohunek relay</title><body>Sign-in completed. You may return to Pohunek.</body></html>"))
}

async fn browser_start(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Response, ApiFailure> {
    gate(&state)?;
    reject_mixed(&headers)?;
    require_exact_origin(&headers, &state.config)?;
    let binding = random_cookie()?;
    let login = state
        .auth
        .begin_browser_login(&state.oidc, LoginBindingCookie::new(binding.clone()))
        .await
        .map_err(|error| ApiFailure::auth(&error))?;
    let mut response = Redirect::to(login.authorization_url.as_str()).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        protected_cookie(LOGIN_COOKIE, &binding, true)?,
    );
    Ok(no_store(response))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallbackQuery {
    state: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    iss: Option<String>,
    #[serde(default)]
    session_state: Option<String>,
}
async fn browser_callback(
    State(state): State<ServerState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Result<Response, ApiFailure> {
    if gate(&state).is_err() || reject_mixed(&headers).is_err() {
        return callback_failure_response(&AuthError::Durable);
    }
    let query = match parse_callback_query(raw_query.as_deref()) {
        Ok(query) => query,
        Err(error) => return callback_failure_response(&error),
    };
    let Ok(Some(binding)) = cookie(&headers, LOGIN_COOKIE) else {
        return callback_failure_response(&AuthError::OidcInvalid);
    };
    let outcome = match (query.code, query.error) {
        (Some(code), None) => BrowserCallbackOutcome::Code(OidcCallbackCode::new(code)),
        (None, Some(_)) => BrowserCallbackOutcome::ProviderDenied,
        _ => BrowserCallbackOutcome::Invalid,
    };
    let session = state
        .auth
        .complete_browser_login(
            &state.oidc,
            BrowserCallback::new(query.state, query.iss, query.session_state, outcome),
            LoginBindingCookie::new(binding),
        )
        .await;
    let session = match session {
        Ok(session) => session,
        Err(error) => return callback_failure_response(&error),
    };
    let target = state
        .config
        .public_origin
        .join("account/ready")
        .map_err(|_error| ApiFailure::invalid())?;
    let mut response = Redirect::temporary(target.as_str()).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        protected_cookie(SESSION_COOKIE, session.cookie(), true)?,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        protected_cookie(CSRF_COOKIE, session.csrf(), false)?,
    );
    response
        .headers_mut()
        .append(header::SET_COOKIE, cleared_cookie(LOGIN_COOKIE, true)?);
    Ok(no_store(response))
}

fn parse_callback_query(raw_query: Option<&str>) -> Result<CallbackQuery, AuthError> {
    let raw_query = raw_query.ok_or(AuthError::OidcInvalid)?;
    let mut state = None;
    let mut code = None;
    let mut error = None;
    let mut issuer = None;
    let mut session_state = None;
    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        let target = match key.as_ref() {
            "state" => &mut state,
            "code" => &mut code,
            "error" => &mut error,
            "iss" => &mut issuer,
            "session_state" => &mut session_state,
            _ => return Err(AuthError::OidcInvalid),
        };
        if target.replace(value.into_owned()).is_some() {
            return Err(AuthError::OidcInvalid);
        }
    }
    Ok(CallbackQuery {
        state: state.ok_or(AuthError::OidcInvalid)?,
        code,
        error,
        iss: issuer,
        session_state,
    })
}

fn callback_failure_response(error: &AuthError) -> Result<Response, ApiFailure> {
    let mut response = ApiFailure::auth(error).into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, cleared_cookie(LOGIN_COOKIE, true)?);
    Ok(no_store(response))
}

async fn device_start(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Response, ApiFailure> {
    gate(&state)?;
    reject_browser(&headers)?;
    let login = state
        .auth
        .begin_device_login(&state.oidc)
        .await
        .map_err(|error| ApiFailure::auth(&error))?;
    let poll_secret = Secret::new(login.poll_secret().to_owned());
    let seconds = i64::try_from(login.authorization.expires_in.as_secs())
        .map_err(|_error| ApiFailure::invalid())?;
    Ok(no_store(
        Json(DeviceLoginStart {
            login_id: LoginId::from_uuid(login.login_id),
            verification_uri: login.authorization.verification_uri.to_string(),
            verification_uri_complete: login
                .authorization
                .verification_uri_complete
                .map(|url| url.to_string()),
            user_code: login.authorization.user_code,
            expires_at: time::OffsetDateTime::now_utc() + time::Duration::seconds(seconds),
            interval_seconds: u32::try_from(login.authorization.interval.as_secs())
                .map_err(|_error| ApiFailure::invalid())?,
            poll_secret,
        })
        .into_response(),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DevicePollRequest {
    login_id: LoginId,
}
async fn device_poll(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<DevicePollRequest>,
) -> Result<Response, ApiFailure> {
    gate(&state)?;
    reject_browser(&headers)?;
    let secret = required_header(&headers, DEVICE_SECRET_HEADER)?;
    let result = match state
        .auth
        .poll_device_login(
            &state.oidc,
            request.login_id.as_uuid(),
            DevicePollSecret::new(secret),
        )
        .await
    {
        Ok(credential) => DevicePollResult::Complete {
            credential: DeviceCredential {
                credential_id: relay_protocol::CredentialId::parse(&credential.public_id)
                    .map_err(|_error| ApiFailure::invalid())?,
                secret: Secret::new(credential.secret().to_owned()),
                expires_at: credential.expires_at(),
            },
        },
        Err(AuthError::DevicePending {
            retry_after_seconds,
        }) => DevicePollResult::Pending {
            retry_after_seconds,
        },
        Err(
            AuthError::DeviceSlowDown {
                retry_after_seconds,
            }
            | AuthError::DeviceBusy {
                retry_after_seconds,
            },
        ) => DevicePollResult::SlowDown {
            retry_after_seconds,
        },
        Err(AuthError::DeviceDenied) => DevicePollResult::Denied,
        Err(AuthError::DeviceExpired) => DevicePollResult::Expired,
        Err(error) => return Err(ApiFailure::auth(&error)),
    };
    Ok(no_store(Json(result).into_response()))
}

async fn account(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Response, ApiFailure> {
    gate(&state)?;
    let actor = authenticated(&state, &headers, false).await?;
    Ok(no_store(
        Json(
            state
                .auth
                .account(actor)
                .await
                .map_err(|error| ApiFailure::auth(&error))?,
        )
        .into_response(),
    ))
}

async fn list_credentials(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Query(page): axum::extract::Query<PageRequest>,
) -> Result<Response, ApiFailure> {
    gate(&state)?;
    let actor = authenticated(&state, &headers, false).await?;
    Ok(no_store(
        Json(
            state
                .auth
                .list_credentials(actor, page)
                .await
                .map_err(|error| ApiFailure::auth(&error))?,
        )
        .into_response(),
    ))
}

async fn rotate_credential(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(credential_id): axum::extract::Path<CredentialId>,
    Json(request): Json<RotateCredentialRequest>,
) -> Result<Response, ApiFailure> {
    gate(&state)?;
    let actor = authenticated(&state, &headers, true).await?;
    Ok(no_store(
        Json(
            state
                .auth
                .rotate_credential(actor, credential_id, request)
                .await
                .map_err(|error| ApiFailure::auth(&error))?,
        )
        .into_response(),
    ))
}

async fn revoke_credential(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(credential_id): axum::extract::Path<CredentialId>,
    Json(request): Json<RevokeCredentialRequest>,
) -> Result<StatusCode, ApiFailure> {
    gate(&state)?;
    let actor = authenticated(&state, &headers, true).await?;
    state
        .auth
        .revoke_credential(actor, credential_id, request)
        .await
        .map_err(|error| ApiFailure::auth(&error))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn revoke_current_credential(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<RevokeCredentialRequest>,
) -> Result<StatusCode, ApiFailure> {
    gate(&state)?;
    let actor = authenticated(&state, &headers, true).await?;
    if actor.actor().binding() != crate::store::AuthenticationBinding::Credential {
        return Err(ApiFailure::unauthenticated());
    }
    state
        .auth
        .revoke_credential(
            actor,
            CredentialId::from_uuid(actor.actor().authentication_id()),
            request,
        )
        .await
        .map_err(|error| ApiFailure::auth(&error))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_service_account(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(team_id): axum::extract::Path<TeamId>,
    Json(request): Json<CreateServiceAccountRequest>,
) -> Result<Response, ApiFailure> {
    gate(&state)?;
    let actor = authenticated(&state, &headers, true).await?;
    Ok(no_store(
        Json(
            state
                .auth
                .create_service_account(actor, team_id, request)
                .await
                .map_err(|error| ApiFailure::auth(&error))?,
        )
        .into_response(),
    ))
}

async fn list_service_accounts(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(team_id): axum::extract::Path<TeamId>,
    axum::extract::Query(page): axum::extract::Query<PageRequest>,
) -> Result<Response, ApiFailure> {
    gate(&state)?;
    let actor = authenticated(&state, &headers, false).await?;
    Ok(no_store(
        Json(
            state
                .auth
                .list_service_accounts(actor, team_id, page)
                .await
                .map_err(|error| ApiFailure::auth(&error))?,
        )
        .into_response(),
    ))
}

async fn revoke_service_credential(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path((team_id, principal_id, credential_id)): axum::extract::Path<(
        TeamId,
        relay_protocol::PrincipalId,
        CredentialId,
    )>,
    Json(request): Json<RevokeCredentialRequest>,
) -> Result<StatusCode, ApiFailure> {
    gate(&state)?;
    let actor = authenticated(&state, &headers, true).await?;
    state
        .auth
        .revoke_service_credential(actor, team_id, principal_id, credential_id, request)
        .await
        .map_err(|error| ApiFailure::auth(&error))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn rotate_service_credential(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path((team_id, principal_id, credential_id)): axum::extract::Path<(
        TeamId,
        relay_protocol::PrincipalId,
        CredentialId,
    )>,
    Json(request): Json<RotateCredentialRequest>,
) -> Result<Response, ApiFailure> {
    gate(&state)?;
    let actor = authenticated(&state, &headers, true).await?;
    Ok(no_store(
        Json(
            state
                .auth
                .rotate_service_credential(actor, team_id, principal_id, credential_id, request)
                .await
                .map_err(|error| ApiFailure::auth(&error))?,
        )
        .into_response(),
    ))
}

async fn authenticated(
    state: &ServerState,
    headers: &HeaderMap,
    mutation: bool,
) -> Result<crate::auth::AuthenticatedActor, ApiFailure> {
    if let Some(authorization) = headers.get(header::AUTHORIZATION) {
        if headers.contains_key(header::COOKIE) || headers.contains_key(header::ORIGIN) {
            return Err(ApiFailure::invalid());
        }
        let value = authorization
            .to_str()
            .map_err(|_error| ApiFailure::invalid())?;
        let bearer = value
            .strip_prefix("Bearer ")
            .ok_or_else(ApiFailure::invalid)?;
        if bearer.is_empty() || bearer.contains(char::is_whitespace) {
            return Err(ApiFailure::invalid());
        }
        return state
            .auth
            .authenticate_bearer(RelayBearerCredential::new(bearer.to_owned()))
            .await
            .map_err(|error| ApiFailure::auth(&error));
    }
    let session = cookie(headers, SESSION_COOKIE)?.ok_or_else(ApiFailure::unauthenticated)?;
    if mutation {
        require_exact_origin(headers, &state.config)?;
        let csrf = required_header(headers, "x-pohunek-csrf")?;
        let expected = cookie(headers, CSRF_COOKIE)?.ok_or_else(ApiFailure::unauthenticated)?;
        state
            .auth
            .verify_browser_csrf(&session, &csrf)
            .await
            .map_err(|error| ApiFailure::auth(&error))?;
        if csrf != expected {
            return Err(ApiFailure::unauthenticated());
        }
    }
    state
        .auth
        .authenticate_browser(BrowserCookie::new(session))
        .await
        .map_err(|error| ApiFailure::auth(&error))
}

fn reject_mixed(headers: &HeaderMap) -> Result<(), ApiFailure> {
    if headers.contains_key(header::AUTHORIZATION) {
        Err(ApiFailure::invalid())
    } else {
        Ok(())
    }
}
fn gate(state: &ServerState) -> Result<(), ApiFailure> {
    if state.ingress_open.load(Ordering::Acquire) && !state.authority.is_closed() {
        Ok(())
    } else {
        Err(ApiFailure::unavailable())
    }
}
fn require_exact_origin(headers: &HeaderMap, config: &Config) -> Result<(), ApiFailure> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(ApiFailure::invalid)?;
    if origin == config.public_origin.as_str().trim_end_matches('/') {
        Ok(())
    } else {
        Err(ApiFailure::invalid())
    }
}
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}
fn reject_browser(headers: &HeaderMap) -> Result<(), ApiFailure> {
    if headers.contains_key(header::COOKIE)
        || headers.contains_key(header::ORIGIN)
        || headers.contains_key(header::AUTHORIZATION)
    {
        Err(ApiFailure::invalid())
    } else {
        Ok(())
    }
}
fn cookie(headers: &HeaderMap, name: &str) -> Result<Option<String>, ApiFailure> {
    let prefix = format!("{name}=");
    let mut selected = None;
    for header_value in &headers.get_all(header::COOKIE) {
        let header_value = header_value
            .to_str()
            .map_err(|_error| ApiFailure::invalid())?;
        for part in header_value.split(';').map(str::trim) {
            let Some(value) = part.strip_prefix(&prefix) else {
                continue;
            };
            if selected.replace(value.to_owned()).is_some() {
                return Err(ApiFailure::invalid());
            }
        }
    }
    Ok(selected)
}
fn required_header(headers: &HeaderMap, name: &str) -> Result<String, ApiFailure> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(ApiFailure::invalid)
}
fn random_cookie() -> Result<String, ApiFailure> {
    let mut bytes = [0_u8; BROWSER_BINDING_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|_error| ApiFailure::unavailable())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn protected_cookie(name: &str, value: &str, http_only: bool) -> Result<HeaderValue, ApiFailure> {
    let tail = if http_only {
        "; Path=/; Secure; HttpOnly; SameSite=Lax"
    } else {
        "; Path=/; Secure; SameSite=Strict"
    };
    HeaderValue::from_str(&format!("{name}={value}{tail}")).map_err(|_error| ApiFailure::invalid())
}
fn cleared_cookie(name: &str, http_only: bool) -> Result<HeaderValue, ApiFailure> {
    let tail = if http_only {
        "; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0"
    } else {
        "; Path=/; Secure; SameSite=Strict; Max-Age=0"
    };
    HeaderValue::from_str(&format!("{name}={tail}")).map_err(|_error| ApiFailure::invalid())
}

#[derive(Debug)]
struct ApiFailure {
    status: StatusCode,
    code: &'static str,
}
impl ApiFailure {
    fn invalid() -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
        }
    }
    fn unauthenticated() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "authentication_required",
        }
    }
    fn unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "unavailable",
        }
    }
    fn auth(error: &AuthError) -> Self {
        match error {
            AuthError::CredentialInvalid | AuthError::CsrfInvalid | AuthError::OriginInvalid => {
                Self::unauthenticated()
            }
            AuthError::Capacity | AuthError::Durable | AuthError::IssuerUnavailable => {
                Self::unavailable()
            }
            AuthError::IdempotencyConflict => Self {
                status: StatusCode::CONFLICT,
                code: "state_conflict",
            },
            AuthError::RotationExpired => Self {
                status: StatusCode::GONE,
                code: "rotation_expired",
            },
            AuthError::RotationRejected => Self {
                status: StatusCode::BAD_REQUEST,
                code: "rotation_rejected",
            },
            _ => Self::invalid(),
        }
    }
    fn store(error: &StoreError) -> Self {
        match error {
            StoreError::Database(_) | StoreError::Migration(_) | StoreError::AuditUnavailable => {
                Self::unavailable()
            }
            StoreError::Forbidden => Self::unauthenticated(),
            StoreError::StaleState | StoreError::Contended | StoreError::IdempotencyConflict => {
                Self {
                    status: StatusCode::CONFLICT,
                    code: "state_conflict",
                }
            }
            StoreError::InputTooLarge => Self::invalid(),
        }
    }
}

#[cfg(all(test, feature = "postgres-tests"))]
mod credential_router_tests {
    use std::{path::PathBuf, time::Duration};

    use super::*;
    use crate::{
        auth::{service::tests as auth_tests, AuthLimits, DigestKey},
        config::{AuthConfig, LogConfig, LoginPolicy, Tls},
    };
    use axum::{body::to_bytes, http::Request};
    use serde_json::json;
    use tower::ServiceExt;

    fn config() -> Config {
        let mut limits = crate::config::fixture_limits();
        limits.body_bytes = 8 * 1024;
        limits.response_bytes = 64 * 1024;
        limits.requests_per_window = 100;
        Config {
            relay_id: "test-relay".to_owned(),
            bind: "127.0.0.1:0".parse().expect("test bind"),
            public_origin: "https://relay.example/".parse().expect("test origin"),
            callback: "https://relay.example/v1/auth/oidc/callback"
                .parse()
                .expect("test callback"),
            tls: Tls::LoopbackProxy,
            database_url_file: PathBuf::new(),
            digest_key_file: PathBuf::new(),
            digest_key_id: "test".to_owned(),
            witness_key_file: PathBuf::new(),
            witness_key_id: "test".to_owned(),
            issuer: "https://issuer.example".parse().expect("test issuer"),
            client_id: "client".to_owned(),
            ca_file: None,
            witness_dir: PathBuf::new(),
            limits,
            logging: LogConfig {
                directory: PathBuf::new(),
                max_file_bytes: 1024,
                max_files: 1,
            },
            auth: AuthConfig {
                pending_transactions: 2,
                login_lifetime: Duration::from_mins(1),
                browser_session_lifetime: Duration::from_mins(5),
                browser_session_idle: Duration::from_mins(1),
                human_credential_lifetime: Duration::from_mins(10),
                service_credential_lifetime: Duration::from_mins(10),
                max_rotation_overlap: Duration::from_secs(30),
                credentials_per_principal: 8,
                service_accounts_per_team: 16,
                device_poll_lease: Duration::from_secs(5),
                device_slow_down_increment: Duration::from_secs(5),
                max_device_poll_interval: Duration::from_mins(1),
            },
            login_policy: LoginPolicy::AnyAuthenticatedSubject,
        }
    }

    fn router_auth_limits() -> AuthLimits {
        AuthLimits::new(
            Duration::from_mins(1),
            Duration::from_mins(5),
            Duration::from_mins(1),
            Duration::from_mins(10),
            Duration::from_mins(10),
            Duration::from_secs(30),
            8,
            16,
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_mins(1),
        )
        .expect("valid router authentication limits")
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "The HTTP bearer-boundary and idempotency cases are one end-to-end security sequence."
    )]
    async fn credential_routes_enforce_bearer_boundaries_and_exact_service_create_retry() {
        let (store, schema, bootstrap) = auth_tests::fixture().await;
        let (authority, _directory) = auth_tests::authority(store.clone()).await;
        let digest_key = DigestKey::new("test".to_owned(), b"ephemeral-test-key".to_vec());
        let auth = AuthService::new(
            store.clone(),
            digest_key.clone(),
            2,
            router_auth_limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            Arc::clone(&authority),
        );
        let principal_id = Uuid::now_v7();
        let credential_id = Uuid::now_v7();
        let identity_id = Uuid::now_v7();
        let team_id = Uuid::now_v7();
        let secret = "router-owner-secret";
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(principal_id)
        .execute(store.pool())
        .await
        .expect("seed principal");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','router-owner',$2,1)")
            .bind(identity_id)
            .bind(principal_id)
            .execute(store.pool())
            .await
            .expect("seed identity");
        sqlx::query("INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ($1,'team','active',1,1)")
            .bind(team_id).execute(store.pool()).await.expect("seed team");
        sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)")
            .bind(Uuid::now_v7()).bind(team_id).bind(principal_id).execute(store.pool()).await.expect("seed owner");
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,1,clock_timestamp()+interval '1 hour')")
            .bind(credential_id).bind(credential_id.to_string()).bind(digest_key.digest(secret).as_slice()).bind(principal_id).bind(identity_id).execute(store.pool()).await.expect("seed bearer");
        let state = ServerState::new(
            config(),
            store.clone(),
            auth,
            OidcClient::test_client(),
            authority,
        );
        let app = router(state);
        let bearer = format!("Bearer {credential_id}.{secret}");
        let account = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/account")
                    .header(header::AUTHORIZATION, &bearer)
                    .body(axum::body::Body::empty())
                    .expect("account request"),
            )
            .await
            .expect("router response");
        assert_eq!(account.status(), StatusCode::OK);
        let invalid_page = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/account/credentials?limit=0")
                    .header(header::AUTHORIZATION, &bearer)
                    .body(axum::body::Body::empty())
                    .expect("invalid page request"),
            )
            .await
            .expect("router response");
        assert_eq!(invalid_page.status(), StatusCode::BAD_REQUEST);
        let mixed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/account")
                    .header(header::AUTHORIZATION, &bearer)
                    .header(header::COOKIE, "__Host-pohunek-relay-session=x")
                    .body(axum::body::Body::empty())
                    .expect("mixed request"),
            )
            .await
            .expect("router response");
        assert_eq!(mixed.status(), StatusCode::BAD_REQUEST);
        let rotation_audit_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action = 'auth.credential.rotate'",
        )
        .fetch_one(store.pool())
        .await
        .expect("count rotation audits before rejected requests");
        let rejected_expiry = (time::OffsetDateTime::now_utc() + time::Duration::minutes(5))
            .replace_nanosecond(0)
            .expect("whole-second rejected expiry")
            .format(&time::format_description::well_known::Rfc3339)
            .expect("RFC3339 rejected expiry");
        let rejected_rotation = json!({
            "overlap_seconds":120,
            "idempotency":{"correlation_id":Uuid::now_v7(),"idempotency_key":Uuid::now_v7()},
            "expires_at":rejected_expiry,
        });
        let rejected = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/account/credentials/{credential_id}/rotate"))
                    .header(header::AUTHORIZATION, &bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(rejected_rotation.to_string()))
                    .expect("rejected rotation request"),
            )
            .await
            .expect("router response");
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        let rejected_json: serde_json::Value = serde_json::from_slice(
            &to_bytes(rejected.into_body(), 64 * 1024)
                .await
                .expect("rejected rotation body"),
        )
        .expect("rejected rotation JSON");
        assert_eq!(rejected_json["error"]["code"], "rotation_rejected");
        let excessive_lifetime = (time::OffsetDateTime::now_utc() + time::Duration::seconds(601))
            .replace_nanosecond(0)
            .expect("whole-second excessive lifetime")
            .format(&time::format_description::well_known::Rfc3339)
            .expect("RFC3339 excessive lifetime");
        let lifetime_rejection = json!({
            "overlap_seconds":1,
            "idempotency":{"correlation_id":Uuid::now_v7(),"idempotency_key":Uuid::now_v7()},
            "expires_at":excessive_lifetime,
        });
        let rejected_lifetime = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/account/credentials/{credential_id}/rotate"))
                    .header(header::AUTHORIZATION, &bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(lifetime_rejection.to_string()))
                    .expect("lifetime rejection request"),
            )
            .await
            .expect("router response");
        assert_eq!(rejected_lifetime.status(), StatusCode::BAD_REQUEST);
        let rejected_lifetime_json: serde_json::Value = serde_json::from_slice(
            &to_bytes(rejected_lifetime.into_body(), 64 * 1024)
                .await
                .expect("lifetime rejection body"),
        )
        .expect("lifetime rejection JSON");
        assert_eq!(rejected_lifetime_json["error"]["code"], "rotation_rejected");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM relay_credentials WHERE principal_id = $1"
            )
            .bind(principal_id)
            .fetch_one(store.pool())
            .await
            .expect("count credentials after rejected router rotation"),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events WHERE action = 'auth.credential.rotate'"
            )
            .fetch_one(store.pool())
            .await
            .expect("count audits after rejected router rotation"),
            rotation_audit_count
        );
        let expiry = (time::OffsetDateTime::now_utc() + time::Duration::minutes(10))
            .replace_nanosecond(0)
            .expect("whole-second expiry")
            .format(&time::format_description::well_known::Rfc3339)
            .expect("RFC3339 expiry");
        let request = json!({"display_name":"deploy","idempotency":{"correlation_id":Uuid::now_v7(),"idempotency_key":Uuid::now_v7()},"expires_at":expiry});
        let uri = format!("/v1/teams/{team_id}/service-accounts");
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&uri)
                    .header(header::AUTHORIZATION, &bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(request.to_string()))
                    .expect("create request"),
            )
            .await
            .expect("router response");
        assert_eq!(first.status(), StatusCode::OK);
        let first_json: serde_json::Value = serde_json::from_slice(
            &to_bytes(first.into_body(), 64 * 1024)
                .await
                .expect("create body"),
        )
        .expect("create JSON");
        assert!(first_json["issued"]["credential"]["secret"].is_string());
        let service_page = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("{uri}?limit=1"))
                    .header(header::AUTHORIZATION, &bearer)
                    .body(axum::body::Body::empty())
                    .expect("service list request"),
            )
            .await
            .expect("router response");
        assert_eq!(service_page.status(), StatusCode::OK);
        let replay = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&uri)
                    .header(header::AUTHORIZATION, &bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(request.to_string()))
                    .expect("replay request"),
            )
            .await
            .expect("router response");
        assert_eq!(replay.status(), StatusCode::OK);
        let replay_json: serde_json::Value = serde_json::from_slice(
            &to_bytes(replay.into_body(), 64 * 1024)
                .await
                .expect("replay body"),
        )
        .expect("replay JSON");
        assert!(replay_json["issued"]["credential"].is_null());
        let service_principal = first_json["account"]["principal_id"]
            .as_str()
            .expect("service principal");
        let service_credential = first_json["issued"]["record"]["credential_id"]
            .as_str()
            .expect("service credential");
        let rotate_uri =
            format!("{uri}/{service_principal}/credentials/{service_credential}/rotate");
        let rotate = json!({"overlap_seconds":1,"idempotency":{"correlation_id":Uuid::now_v7(),"idempotency_key":Uuid::now_v7()},"expires_at":expiry});
        let rotated = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(rotate_uri)
                    .header(header::AUTHORIZATION, &bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(rotate.to_string()))
                    .expect("rotate request"),
            )
            .await
            .expect("router response");
        assert_eq!(rotated.status(), StatusCode::OK);
        let rotated_json: serde_json::Value = serde_json::from_slice(
            &to_bytes(rotated.into_body(), 64 * 1024)
                .await
                .expect("rotate body"),
        )
        .expect("rotate JSON");
        let replacement = rotated_json["record"]["credential_id"]
            .as_str()
            .expect("replacement credential");
        let revoke_uri = format!("{uri}/{service_principal}/credentials/{replacement}/revoke");
        let revoke = json!({
            "idempotency": {
                "correlation_id": Uuid::now_v7(),
                "idempotency_key": Uuid::now_v7(),
            },
        });
        let revoked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(revoke_uri)
                    .header(header::AUTHORIZATION, &bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(revoke.to_string()))
                    .expect("revoke request"),
            )
            .await
            .expect("router response");
        assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
        auth_tests::cleanup(&bootstrap, &schema).await;
    }
}
impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let response = (
            self.status,
            Json(ApiErrorBody {
                error: ApiError {
                    code: self.code.to_owned(),
                    message: self.code.to_owned(),
                    request_id: Uuid::now_v7().hyphenated().to_string(),
                    recover: None,
                },
            }),
        )
            .into_response();
        no_store(response)
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        http::{header, HeaderMap, HeaderValue},
        response::IntoResponse,
        response::Redirect,
    };

    use super::{
        callback_failure_response, cookie, protected_cookie, random_cookie, CallbackQuery,
        LOGIN_COOKIE,
    };
    use crate::auth::AuthError;

    #[test]
    fn browser_start_redirect_changes_post_to_get() {
        let response = Redirect::to("https://issuer.example/authorize").into_response();
        assert_eq!(response.status(), http::StatusCode::SEE_OTHER);
    }

    #[test]
    fn callback_accepts_standard_keycloak_coordinates_only() {
        let callback: CallbackQuery = serde_json::from_str(
            r#"{"code":"opaque-code","state":"opaque-state","iss":"https://issuer.example","session_state":"opaque-session"}"#,
        )
        .expect("Keycloak callback coordinates parse");
        assert_eq!(callback.iss.as_deref(), Some("https://issuer.example"));
        assert!(callback.session_state.is_some());
        assert!(serde_json::from_str::<CallbackQuery>(
            r#"{"code":"opaque-code","state":"opaque-state","unexpected":"value"}"#
        )
        .is_err());
        let denial: CallbackQuery = serde_json::from_str(
            r#"{"error":"access_denied","state":"opaque-state","iss":"https://issuer.example"}"#,
        )
        .expect("provider denial coordinates parse for the auth audit boundary");
        assert!(denial.code.is_none());
        assert_eq!(denial.error.as_deref(), Some("access_denied"));
    }

    #[test]
    fn host_cookie_is_secure_and_cannot_be_duplicated() {
        let cookie_value = random_cookie().expect("generate browser binding");
        assert_eq!(cookie_value.len(), 43);
        let rendered = protected_cookie(LOGIN_COOKIE, &cookie_value, true)
            .expect("serialize protected login cookie");
        let rendered = rendered.to_str().expect("cookie header is ASCII");
        assert!(rendered.starts_with("__Host-pohunek-relay-login="));
        assert!(rendered.contains("; Path=/; Secure; HttpOnly"));
        assert!(!rendered.contains("Domain="));

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static(
                "__Host-pohunek-relay-login=first; __Host-pohunek-relay-login=second",
            ),
        );
        cookie(&headers, LOGIN_COOKIE).unwrap_err();

        let mut separate_headers = HeaderMap::new();
        separate_headers.append(
            header::COOKIE,
            HeaderValue::from_static("__Host-pohunek-relay-login=first"),
        );
        separate_headers.append(
            header::COOKIE,
            HeaderValue::from_static("__Host-pohunek-relay-login=second"),
        );
        cookie(&separate_headers, LOGIN_COOKIE).unwrap_err();
    }

    #[test]
    fn bound_callback_failure_clears_the_login_cookie() {
        let response =
            callback_failure_response(&AuthError::OidcInvalid).expect("serialize callback failure");
        let cookie = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .next()
            .expect("cleared login cookie")
            .to_str()
            .expect("ASCII cookie");
        assert!(cookie.starts_with("__Host-pohunek-relay-login="));
        assert!(cookie.contains("Max-Age=0"));
    }
}

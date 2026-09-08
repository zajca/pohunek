//! Performs bounded OpenID Connect exchanges for relay authentication.
//!
//! This module is deliberately the only auth component that sees provider
//! responses. It neither logs nor persists authorization codes, tokens, PKCE
//! verifiers, or device codes.

// Rust guideline compliant 2026-09-08

use std::{
    fmt::{Debug, Formatter},
    fs,
    future::Future,
    path::PathBuf,
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use openidconnect::{
    core::{
        CoreAuthDisplay, CoreAuthenticationFlow, CoreClaimName, CoreClaimType, CoreClient,
        CoreClientAuthMethod, CoreDeviceAuthorizationResponse, CoreGrantType, CoreJsonWebKey,
        CoreJweContentEncryptionAlgorithm, CoreJweKeyManagementAlgorithm, CoreResponseMode,
        CoreResponseType, CoreSubjectIdentifierType,
    },
    AdditionalProviderMetadata, AsyncHttpClient, AuthorizationCode, ClientId, CsrfToken,
    DeviceAuthorizationUrl, EndpointMaybeSet, EndpointNotSet, EndpointSet, HttpRequest,
    HttpResponse, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, ProviderMetadata,
    RedirectUrl, Scope, TokenResponse,
};
use reqwest::{redirect::Policy, Certificate, Client, ClientBuilder};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;
use zeroize::Zeroizing;

use super::AuthError;

/// Configuration that pins one relay to one OIDC relying-party registration.
#[derive(Clone)]
pub struct OidcClientConfig {
    issuer: Url,
    client_id: String,
    redirect_uri: Url,
    request_timeout: Duration,
    ca_file: Option<PathBuf>,
    max_response_bytes: usize,
}

impl OidcClientConfig {
    /// Validates a pinned issuer, registered client ID, and exact callback URI.
    pub fn new(
        issuer: Url,
        client_id: String,
        redirect_uri: Url,
        request_timeout: Duration,
        ca_file: Option<PathBuf>,
        max_response_bytes: usize,
    ) -> Result<Self, AuthError> {
        if issuer.scheme() != "https"
            || issuer.host_str().is_none()
            || issuer.query().is_some()
            || issuer.fragment().is_some()
            || client_id.is_empty()
            || redirect_uri.scheme() != "https"
            || redirect_uri.host_str().is_none()
            || request_timeout.is_zero()
            || max_response_bytes == 0
        {
            return Err(AuthError::Malformed);
        }
        Ok(Self {
            issuer,
            client_id,
            redirect_uri,
            request_timeout,
            ca_file,
            max_response_bytes,
        })
    }
}

impl Debug for OidcClientConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OidcClientConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("request_timeout", &self.request_timeout)
            .field("ca_file", &self.ca_file.as_ref().map(|_| "REDACTED_PATH"))
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

/// A discovered OIDC client with redirects disabled for every provider request.
#[derive(Clone)]
pub struct OidcClient {
    client: CoreClient<
        EndpointSet,
        EndpointSet,
        EndpointNotSet,
        EndpointNotSet,
        EndpointMaybeSet,
        EndpointMaybeSet,
    >,
    issuer: String,
    http: BoundedHttpClient,
}

/// Bounds all upstream provider response bodies before protocol parsing.
#[derive(Clone)]
struct BoundedHttpClient {
    client: Client,
    max_response_bytes: usize,
}

#[derive(Debug, Error)]
enum OidcHttpError {
    #[error("OIDC request body exceeds configured limit")]
    RequestTooLarge,
    #[error("OIDC response body exceeds configured limit")]
    ResponseTooLarge,
    #[error("OIDC transport failed")]
    Transport(#[source] reqwest::Error),
    #[error("OIDC response was malformed")]
    Response(#[source] http::Error),
}

impl<'request> AsyncHttpClient<'request> for BoundedHttpClient {
    type Error = OidcHttpError;
    type Future =
        Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + Sync + 'request>>;

    fn call(&'request self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            if request.body().len() > self.max_response_bytes {
                return Err(OidcHttpError::RequestTooLarge);
            }
            let response = self
                .client
                .execute(request.try_into().map_err(OidcHttpError::Transport)?)
                .await
                .map_err(OidcHttpError::Transport)?;
            if response
                .content_length()
                .is_some_and(|length| length > self.max_response_bytes as u64)
            {
                return Err(OidcHttpError::ResponseTooLarge);
            }
            let status = response.status();
            let version = response.version();
            let headers = response.headers().clone();
            let mut body = Vec::new();
            let mut response = response;
            while let Some(chunk) = response.chunk().await.map_err(OidcHttpError::Transport)? {
                if body.len().saturating_add(chunk.len()) > self.max_response_bytes {
                    return Err(OidcHttpError::ResponseTooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            let mut builder = http::Response::builder().status(status).version(version);
            for (name, value) in &headers {
                builder = builder.header(name, value);
            }
            builder.body(body).map_err(OidcHttpError::Response)
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DeviceEndpointMetadata {
    device_authorization_endpoint: DeviceAuthorizationUrl,
}
impl AdditionalProviderMetadata for DeviceEndpointMetadata {}
type DeviceProviderMetadata = ProviderMetadata<
    DeviceEndpointMetadata,
    CoreAuthDisplay,
    CoreClientAuthMethod,
    CoreClaimName,
    CoreClaimType,
    CoreGrantType,
    CoreJweContentEncryptionAlgorithm,
    CoreJweKeyManagementAlgorithm,
    CoreJsonWebKey,
    CoreResponseMode,
    CoreResponseType,
    CoreSubjectIdentifierType,
>;

impl Debug for OidcClient {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OidcClient")
            .field("redacted", &true)
            .finish()
    }
}

/// One browser authorization request and its process-local PKCE verifier.
pub(crate) struct BrowserAuthorization {
    pub(crate) url: Url,
    pub(crate) state: Zeroizing<String>,
    pub(crate) nonce: Zeroizing<String>,
    pub(crate) verifier: Zeroizing<String>,
}

impl Debug for BrowserAuthorization {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserAuthorization")
            .field("redacted", &true)
            .finish()
    }
}

/// Safe fields returned to a native terminal for RFC 8628 authorization.
#[derive(Clone)]
pub struct OidcDeviceAuthorization {
    /// Provider verification URL.
    pub verification_uri: Url,
    /// Optional pre-filled provider verification URL.
    pub verification_uri_complete: Option<Url>,
    /// User-entered short code.
    pub user_code: String,
    /// Provider expiry interval.
    pub expires_in: Duration,
    /// Provider polling interval.
    pub interval: Duration,
    pub(crate) device_code: Zeroizing<String>,
    pub(crate) verifier: Zeroizing<String>,
}

impl Debug for OidcDeviceAuthorization {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OidcDeviceAuthorization")
            .field("verification_uri", &self.verification_uri)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .field("redacted", &true)
            .finish()
    }
}

impl OidcDeviceAuthorization {
    pub(crate) fn device_code(&self) -> &str {
        &self.device_code
    }
}

#[derive(Deserialize)]
struct SignedTokenPayload {
    azp: Option<String>,
    nbf: Option<i64>,
}

fn accept_optional_nonce(_nonce: Option<&Nonce>) -> Result<(), String> {
    // RFC 8628 has no browser nonce transaction. PKCE and the one-use device
    // code bind this flow, so an issuer nonce is accepted when present.
    Ok(())
}

fn device_pkce() -> (PkceCodeChallenge, Zeroizing<String>) {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    (challenge, Zeroizing::new(verifier.secret().to_owned()))
}

fn device_token_body(authorization: &OidcDeviceAuthorization, client_id: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:device_code")
        .append_pair("device_code", authorization.device_code())
        .append_pair("client_id", client_id)
        .append_pair("code_verifier", authorization.verifier.as_str())
        .finish()
}

#[derive(Deserialize)]
struct DevicePollErrorResponse {
    error: String,
}

fn device_poll_response_error(body: &[u8]) -> AuthError {
    match serde_json::from_slice::<DevicePollErrorResponse>(body)
        .ok()
        .map(|response| response.error)
        .as_deref()
    {
        Some("authorization_pending") => AuthError::DevicePending {
            retry_after_seconds: 0,
        },
        Some("slow_down") => AuthError::DeviceSlowDown {
            retry_after_seconds: 0,
        },
        Some("access_denied") => AuthError::DeviceDenied,
        Some("expired_token") => AuthError::DeviceExpired,
        _ => AuthError::OidcInvalid,
    }
}

fn signed_token_payload(token: &str) -> Result<SignedTokenPayload, AuthError> {
    let mut parts = token.split('.');
    let _header = parts.next().ok_or(AuthError::OidcInvalid)?;
    let payload = parts.next().ok_or(AuthError::OidcInvalid)?;
    if parts.next().is_none() || parts.next().is_some() {
        return Err(AuthError::OidcInvalid);
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| AuthError::OidcInvalid)?;
    serde_json::from_slice(&decoded).map_err(|_| AuthError::OidcInvalid)
}

fn current_unix_seconds() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs()
        .try_into()
        .ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use zeroize::Zeroizing;

    use super::{
        accept_optional_nonce, device_pkce, device_poll_response_error, device_token_body,
        signed_token_payload, AuthError, OidcDeviceAuthorization,
    };

    fn error(kind: &str) -> AuthError {
        device_poll_response_error(format!(r#"{{"error":"{kind}"}}"#).as_bytes())
    }

    #[test]
    fn device_poll_preserves_every_rfc_terminal_and_retry_class() {
        assert!(matches!(
            error("authorization_pending"),
            AuthError::DevicePending { .. }
        ));
        assert!(matches!(
            error("slow_down"),
            AuthError::DeviceSlowDown { .. }
        ));
        assert!(matches!(error("access_denied"), AuthError::DeviceDenied));
        assert!(matches!(error("expired_token"), AuthError::DeviceExpired));
    }

    #[test]
    fn device_nonce_policy_accepts_absent_nonce_without_fabrication() {
        assert!(accept_optional_nonce(None).is_ok());
    }

    #[test]
    fn device_authorization_debug_never_exposes_exchange_secrets() {
        let authorization = OidcDeviceAuthorization {
            verification_uri: url::Url::parse("https://issuer.example/device")
                .expect("valid fixture URL"),
            verification_uri_complete: Some(
                url::Url::parse("https://issuer.example/device?user_code=sentinel-complete-code")
                    .expect("valid complete fixture URL"),
            ),
            user_code: "sentinel-user-code".to_owned(),
            expires_in: Duration::from_secs(60),
            interval: Duration::from_secs(1),
            device_code: Zeroizing::new("sentinel-device-code".to_owned()),
            verifier: Zeroizing::new("sentinel-verifier".to_owned()),
        };
        let rendered = format!("{authorization:?}");
        for secret in [
            "sentinel-user-code",
            "sentinel-complete-code",
            "sentinel-device-code",
            "sentinel-verifier",
        ] {
            assert!(!rendered.contains(secret));
        }
    }

    #[test]
    fn device_request_uses_s256_challenge_and_token_reuses_its_verifier() {
        let (challenge, verifier) = device_pkce();
        let expected = openidconnect::PkceCodeChallenge::from_code_verifier_sha256(
            &openidconnect::PkceCodeVerifier::new(verifier.to_string()),
        );
        assert_eq!(challenge.as_str(), expected.as_str());
        assert_eq!(&**challenge.method(), "S256");

        let authorization = OidcDeviceAuthorization {
            verification_uri: url::Url::parse("https://issuer.example/device")
                .expect("valid fixture URL"),
            verification_uri_complete: None,
            user_code: "user-code".to_owned(),
            expires_in: Duration::from_secs(60),
            interval: Duration::from_secs(1),
            device_code: Zeroizing::new("device-code".to_owned()),
            verifier,
        };
        let fields: std::collections::HashMap<_, _> = url::form_urlencoded::parse(
            device_token_body(&authorization, "relay-client").as_bytes(),
        )
        .into_owned()
        .collect();
        assert_eq!(
            fields.get("code_verifier"),
            Some(&authorization.verifier.to_string())
        );
        assert_eq!(
            fields.get("grant_type"),
            Some(&"urn:ietf:params:oauth:grant-type:device_code".to_owned())
        );
    }

    #[test]
    fn signed_payload_retains_azp_and_not_before_for_explicit_validation() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"azp":"other-client","nbf":9999999999}"#);
        let token = format!("header.{payload}.signature");
        let parsed = signed_token_payload(&token).expect("three-part token payload parses");
        assert_eq!(parsed.azp.as_deref(), Some("other-client"));
        assert_eq!(parsed.nbf, Some(9_999_999_999));
    }
}

/// Verified stable provider identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcIdentity {
    /// Configured issuer string after signed-token verification.
    pub issuer: String,
    /// Immutable OIDC subject.
    pub subject: String,
}

impl OidcClient {
    #[cfg(test)]
    pub(crate) fn test_client() -> Self {
        let metadata: DeviceProviderMetadata = serde_json::from_value(serde_json::json!({
            "issuer": "https://issuer.example",
            "authorization_endpoint": "https://issuer.example/authorize",
            "token_endpoint": "https://issuer.example/token",
            "jwks_uri": "https://issuer.example/jwks",
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "device_authorization_endpoint": "https://issuer.example/device"
        }))
        .expect("valid test provider metadata");
        let device_endpoint = metadata
            .additional_metadata()
            .device_authorization_endpoint
            .clone();
        Self {
            client: CoreClient::from_provider_metadata(
                metadata,
                ClientId::new("client".to_owned()),
                None,
            )
            .set_device_authorization_url(device_endpoint)
            .set_redirect_uri(
                RedirectUrl::new("https://relay.example/callback".to_owned())
                    .expect("valid test redirect URI"),
            ),
            issuer: "https://issuer.example".to_owned(),
            http: BoundedHttpClient {
                client: Client::new(),
                max_response_bytes: 1024,
            },
        }
    }

    /// Discovers and pins provider metadata with strict redirect and timeout policy.
    pub async fn discover(config: OidcClientConfig) -> Result<Self, AuthError> {
        let issuer = IssuerUrl::new(config.issuer.to_string()).map_err(|_| AuthError::Malformed)?;
        let mut builder = ClientBuilder::new()
            .redirect(Policy::none())
            .timeout(config.request_timeout);
        if let Some(path) = &config.ca_file {
            let certificate =
                Certificate::from_pem(&fs::read(path).map_err(|_| AuthError::IssuerUnavailable)?)
                    .map_err(|_| AuthError::IssuerUnavailable)?;
            builder = builder.add_root_certificate(certificate);
        }
        let client = builder.build().map_err(|_| AuthError::IssuerUnavailable)?;
        let http = BoundedHttpClient {
            client,
            max_response_bytes: config.max_response_bytes,
        };
        let metadata = DeviceProviderMetadata::discover_async(issuer, &http)
            .await
            .map_err(|_| AuthError::IssuerUnavailable)?;
        if metadata.issuer().as_str() != config.issuer.as_str() {
            return Err(AuthError::OidcInvalid);
        }
        let redirect =
            RedirectUrl::new(config.redirect_uri.to_string()).map_err(|_| AuthError::Malformed)?;
        let device_endpoint = metadata
            .additional_metadata()
            .device_authorization_endpoint
            .clone();
        Ok(Self {
            client: CoreClient::from_provider_metadata(
                metadata,
                ClientId::new(config.client_id),
                None,
            )
            .set_device_authorization_url(device_endpoint)
            .set_redirect_uri(redirect),
            issuer: config.issuer.to_string(),
            http,
        })
    }

    /// Creates a PKCE-bound authorization-code redirect.
    pub(crate) fn begin_browser(&self) -> BrowserAuthorization {
        let (challenge, verifier) = device_pkce();
        let (url, state, nonce) = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("openid".to_owned()))
            .set_pkce_challenge(challenge)
            .url();
        BrowserAuthorization {
            url,
            state: Zeroizing::new(state.secret().to_owned()),
            nonce: Zeroizing::new(nonce.secret().to_owned()),
            verifier,
        }
    }

    pub(crate) fn issuer(&self) -> &str {
        &self.issuer
    }
    pub(crate) fn client_id(&self) -> &str {
        self.client.client_id().as_str()
    }
    pub(crate) fn redirect_uri(&self) -> Option<&str> {
        self.client.redirect_uri().map(|uri| uri.as_str())
    }

    fn checked_identity(
        &self,
        raw_token: String,
        claims: &openidconnect::core::CoreIdTokenClaims,
    ) -> Result<OidcIdentity, AuthError> {
        if claims.audiences().len() != 1 || claims.audiences()[0].as_str() != self.client_id() {
            return Err(AuthError::OidcInvalid);
        }
        if claims
            .authorized_party()
            .is_some_and(|party| party.as_str() != self.client_id())
        {
            return Err(AuthError::OidcInvalid);
        }
        let payload = signed_token_payload(&raw_token)?;
        if payload.azp.is_some_and(|party| party != self.client_id()) {
            return Err(AuthError::OidcInvalid);
        }
        if payload
            .nbf
            .is_some_and(|not_before| current_unix_seconds().is_none_or(|now| not_before > now))
        {
            return Err(AuthError::OidcInvalid);
        }
        let subject = claims.subject().as_str();
        if subject.is_empty() {
            return Err(AuthError::OidcInvalid);
        }
        Ok(OidcIdentity {
            issuer: claims.issuer().as_str().to_owned(),
            subject: subject.to_owned(),
        })
    }

    /// Starts a standards-compliant RFC 8628 transaction without a loopback fallback.
    pub async fn begin_device(&self) -> Result<OidcDeviceAuthorization, AuthError> {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let response: CoreDeviceAuthorizationResponse = self
            .client
            .exchange_device_code()
            // RFC 8628 does not define PKCE parameters, but the pinned provider
            // binds its device transaction to RFC 7636 S256 explicitly.
            .add_extra_param("code_challenge", challenge.as_str())
            .add_extra_param("code_challenge_method", &**challenge.method())
            .request_async(&self.http)
            .await
            .map_err(|_| AuthError::OidcInvalid)?;
        let verification_uri =
            Url::parse(response.verification_uri().as_str()).map_err(|_| AuthError::OidcInvalid)?;
        let verification_uri_complete = response
            .verification_uri_complete()
            .map(|value| Url::parse(value.secret()).map_err(|_| AuthError::OidcInvalid))
            .transpose()?;
        Ok(OidcDeviceAuthorization {
            verification_uri,
            verification_uri_complete,
            user_code: response.user_code().secret().to_owned(),
            expires_in: response.expires_in(),
            interval: response.interval(),
            device_code: Zeroizing::new(response.device_code().secret().to_owned()),
            verifier: Zeroizing::new(verifier.secret().to_owned()),
        })
    }

    /// Exchanges the exact in-memory device response and validates its signed ID token.
    pub(crate) async fn poll_device(
        &self,
        authorization: &OidcDeviceAuthorization,
    ) -> Result<OidcIdentity, AuthError> {
        let body = device_token_body(authorization, self.client_id());
        let token_uri = self.client.token_uri().ok_or(AuthError::OidcInvalid)?;
        let response = self.http.post_form(token_uri.as_str(), body).await?;
        if !response.status().is_success() {
            return Err(device_poll_response_error(response.body()));
        }
        let token: openidconnect::core::CoreTokenResponse =
            serde_json::from_slice(response.body()).map_err(|_| AuthError::OidcInvalid)?;
        let id_token = token.id_token().ok_or(AuthError::OidcInvalid)?;
        let claims = id_token
            .claims(&self.client.id_token_verifier(), accept_optional_nonce)
            .map_err(|_| AuthError::OidcInvalid)?;
        self.checked_identity(id_token.to_string(), &claims)
    }

    /// Exchanges a one-use authorization code and verifies all standard ID-token claims.
    pub(crate) async fn exchange_browser_code(
        &self,
        code: String,
        verifier: Zeroizing<String>,
        nonce: Zeroizing<String>,
    ) -> Result<OidcIdentity, AuthError> {
        let response = self
            .client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|_| AuthError::OidcInvalid)?
            .set_pkce_verifier(PkceCodeVerifier::new(verifier.to_string()))
            .request_async(&self.http)
            .await
            .map_err(|_| AuthError::OidcInvalid)?;
        let token = response.id_token().ok_or(AuthError::OidcInvalid)?;
        let claims = token
            .claims(
                &self.client.id_token_verifier(),
                &Nonce::new(nonce.to_string()),
            )
            .map_err(|_| AuthError::OidcInvalid)?;
        self.checked_identity(token.to_string(), &claims)
    }
}

impl BoundedHttpClient {
    async fn post_form(&self, endpoint: &str, body: String) -> Result<HttpResponse, AuthError> {
        if body.len() > self.max_response_bytes {
            return Err(AuthError::Malformed);
        }
        let response = self
            .client
            .post(endpoint)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| AuthError::OidcInvalid)?;
        self.bounded_response(response)
            .await
            .map_err(|_| AuthError::OidcInvalid)
    }

    async fn bounded_response(
        &self,
        response: reqwest::Response,
    ) -> Result<HttpResponse, OidcHttpError> {
        if response
            .content_length()
            .is_some_and(|length| length > self.max_response_bytes as u64)
        {
            return Err(OidcHttpError::ResponseTooLarge);
        }
        let status = response.status();
        let version = response.version();
        let headers = response.headers().clone();
        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await.map_err(OidcHttpError::Transport)? {
            if body.len().saturating_add(chunk.len()) > self.max_response_bytes {
                return Err(OidcHttpError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        let mut builder = http::Response::builder().status(status).version(version);
        for (name, value) in &headers {
            builder = builder.header(name, value);
        }
        builder.body(body).map_err(OidcHttpError::Response)
    }
}

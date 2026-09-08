//! HTTPS-only native relay authentication without a daemon dependency.

#![forbid(unsafe_code)]

pub mod config;

use std::fmt::{Debug, Formatter};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::{
    AccountRecord, CredentialId, CredentialMutation, DeviceCredential, DeviceLoginStart,
    DevicePollResult, LoginId, RevokeCredentialRequest, RotateCredentialRequest, Secret,
};
use reqwest::{
    header::{HeaderValue, AUTHORIZATION},
    redirect::Policy,
    Certificate, RequestBuilder, StatusCode,
};
use serde::de::DeserializeOwned;
use thiserror::Error;
use zeroize::Zeroizing;

use config::{
    Limits, Origin, MAX_CREDENTIAL_LIFETIME, MAX_LOGIN_LIFETIME, MAX_POLL_INTERVAL_SECONDS,
    SECRET_BYTES,
};

/// Safe failures deliberately omit remote bodies, tokens, and HTTP error sources.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid relay client setting: {0}")]
    Configuration(&'static str),
    #[error("relay HTTPS request failed")]
    Transport,
    #[error("relay redirects are not permitted")]
    Redirect,
    #[error("relay response exceeds the configured size limit")]
    ResponseTooLarge,
    #[error("relay returned a malformed response")]
    Malformed,
    #[error("relay rejected authentication")]
    Unauthenticated,
    #[error("relay confirmed that this expired rotation request was never committed")]
    RotationExpired,
    #[error("relay confirmed that this rotation policy was rejected without committing it")]
    RotationRejected,
    #[error("relay request failed with HTTP status {0}")]
    Remote(u16),
}

/// A cloneable transport pinned to exactly one validated relay origin.
#[derive(Clone)]
pub struct Client {
    origin: Origin,
    http: reqwest::Client,
    response_bytes: usize,
}

impl Debug for Client {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("origin", &self.origin)
            .field("response_bytes", &self.response_bytes)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Creates a bounded HTTPS client; an optional CA augments normal trust roots.
    pub fn new(origin: Origin, limits: Limits, ca: Option<Certificate>) -> Result<Self, Error> {
        limits.validate()?;
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .redirect(Policy::none())
            .timeout(limits.request_timeout);
        if let Some(ca) = ca {
            builder = builder.add_root_certificate(ca);
        }
        let http = builder.build().map_err(|_error| Error::Transport)?;
        Ok(Self {
            origin,
            http,
            response_bytes: limits.response_bytes,
        })
    }

    /// Returns the origin to which this client binds every credential.
    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Starts an anonymous native OIDC device login.
    pub async fn start_device(&self) -> Result<DeviceLoginStart, Error> {
        let request = self
            .http
            .post(self.origin.endpoint("v1/auth/device/start")?);
        let start: DeviceLoginStart = self.json(request).await?;
        validate_login(&start)?;
        Ok(start)
    }

    /// Polls one possession-bound login without exposing the poll secret in a URL.
    pub async fn poll_device(
        &self,
        login_id: &LoginId,
        poll_secret: &Secret,
    ) -> Result<DevicePollResult, Error> {
        let secret = secret_header(poll_secret)?;
        let request = self
            .http
            .post(self.origin.endpoint("v1/auth/device/poll")?)
            .header("x-pohunek-device-secret", secret)
            .json(&serde_json::json!({"login_id": login_id}));
        let result: DevicePollResult = self.json(request).await?;
        match &result {
            DevicePollResult::Pending {
                retry_after_seconds,
            }
            | DevicePollResult::SlowDown {
                retry_after_seconds,
            } => {
                if *retry_after_seconds == 0 || *retry_after_seconds > MAX_POLL_INTERVAL_SECONDS {
                    return Err(Error::Malformed);
                }
            }
            DevicePollResult::Complete { credential } => validate_credential(credential)?,
            DevicePollResult::Denied | DevicePollResult::Expired | DevicePollResult::Cancelled => {}
        }
        Ok(result)
    }

    /// Reads the account proven by this exact public credential ID and secret.
    pub async fn account(&self, credential: &DeviceCredential) -> Result<AccountRecord, Error> {
        self.json(self.authenticated(credential, "v1/account", reqwest::Method::GET)?)
            .await
    }

    /// Revokes the calling credential, including compensation after keyring failure.
    ///
    /// Reuse `request` after a transport failure only while this credential can
    /// still authenticate the retry. An unauthenticated response is not proof
    /// that this request committed.
    pub async fn revoke_self(
        &self,
        credential: &DeviceCredential,
        request: &RevokeCredentialRequest,
    ) -> Result<(), Error> {
        let request = self
            .authenticated(
                credential,
                "v1/auth/credentials/self/revoke",
                reqwest::Method::POST,
            )?
            .json(request);
        let response = self.send(request).await?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(Error::Malformed);
        }
        let bytes = self.read_body(response).await?;
        if !bytes.is_empty() {
            return Err(Error::Malformed);
        }
        Ok(())
    }

    /// Revokes an owned credential when its one-time secret delivery was lost.
    ///
    /// Reuse `request` after a transport failure to obtain the original result
    /// without another durable mutation.
    pub async fn revoke_owned(
        &self,
        authentication: &DeviceCredential,
        target: CredentialId,
        request: &RevokeCredentialRequest,
    ) -> Result<(), Error> {
        let path = format!("v1/account/credentials/{target}/revoke");
        let response = self
            .send(
                self.authenticated(authentication, &path, reqwest::Method::POST)?
                    .json(request),
            )
            .await?;
        if response.status() != StatusCode::NO_CONTENT
            || !self.read_body(response).await?.is_empty()
        {
            return Err(Error::Malformed);
        }
        Ok(())
    }

    /// Rotates an owned credential with caller-selected expiry and bounded overlap.
    pub async fn rotate(
        &self,
        authentication: &DeviceCredential,
        target: CredentialId,
        request: &RotateCredentialRequest,
    ) -> Result<CredentialMutation, Error> {
        let path = format!("v1/account/credentials/{target}/rotate");
        let expires_at = request.expires_at;
        let request = self
            .authenticated(authentication, &path, reqwest::Method::POST)?
            .json(request);
        let response = request.send().await.map_err(|_error| Error::Transport)?;
        let status = response.status();
        if matches!(status, StatusCode::GONE | StatusCode::BAD_REQUEST) {
            if response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .is_none_or(|value| !value.trim().eq_ignore_ascii_case("application/json"))
            {
                return Err(Error::Malformed);
            }
            let body: relay_protocol::ApiErrorBody =
                serde_json::from_slice(&self.read_body(response).await?)
                    .map_err(|_error| Error::Malformed)?;
            return Err(match (status, body.error.code.as_str()) {
                (StatusCode::GONE, "rotation_expired") => Error::RotationExpired,
                (StatusCode::BAD_REQUEST, "rotation_rejected") => Error::RotationRejected,
                _ => Error::Remote(status.as_u16()),
            });
        }
        let response = successful(response)?;
        if response.status() != StatusCode::OK {
            return Err(Error::Malformed);
        }
        let result: CredentialMutation = serde_json::from_slice(&self.read_body(response).await?)
            .map_err(|_error| Error::Malformed)?;
        if result.record.credential_id == target || result.record.expires_at != expires_at {
            return Err(Error::Malformed);
        }
        if let Some(credential) = &result.credential {
            validate_credential(credential)?;
            if credential.credential_id != result.record.credential_id
                || credential.expires_at != result.record.expires_at
            {
                return Err(Error::Malformed);
            }
        }
        Ok(result)
    }

    fn authenticated(
        &self,
        credential: &DeviceCredential,
        path: &str,
        method: reqwest::Method,
    ) -> Result<RequestBuilder, Error> {
        validate_secret(&credential.secret)?;
        let raw = Zeroizing::new(format!(
            "Bearer {}.{}",
            credential.credential_id,
            credential.secret.expose()
        ));
        let mut header = HeaderValue::from_str(&raw).map_err(|_error| Error::Malformed)?;
        header.set_sensitive(true);
        Ok(self
            .http
            .request(method, self.origin.endpoint(path)?)
            .header(AUTHORIZATION, header))
    }

    async fn json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T, Error> {
        let response = self.send(request).await?;
        if response.status() != StatusCode::OK {
            return Err(Error::Malformed);
        }
        serde_json::from_slice(&self.read_body(response).await?).map_err(|_error| Error::Malformed)
    }

    async fn send(&self, request: RequestBuilder) -> Result<reqwest::Response, Error> {
        let response = request.send().await.map_err(|_error| Error::Transport)?;
        successful(response)
    }

    async fn read_body(
        &self,
        mut response: reqwest::Response,
    ) -> Result<Zeroizing<Vec<u8>>, Error> {
        if response
            .content_length()
            .is_some_and(|length| length > self.response_bytes as u64)
        {
            return Err(Error::ResponseTooLarge);
        }
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = response.chunk().await.map_err(|_error| Error::Transport)? {
            if bytes.len().saturating_add(chunk.len()) > self.response_bytes {
                return Err(Error::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

fn successful(response: reqwest::Response) -> Result<reqwest::Response, Error> {
    let status = response.status();
    if status.is_redirection() {
        return Err(Error::Redirect);
    }
    if status == StatusCode::UNAUTHORIZED {
        return Err(Error::Unauthenticated);
    }
    if !status.is_success() {
        return Err(Error::Remote(status.as_u16()));
    }
    Ok(response)
}

fn secret_header(secret: &Secret) -> Result<HeaderValue, Error> {
    validate_secret(secret)?;
    let mut header = HeaderValue::from_str(secret.expose()).map_err(|_error| Error::Malformed)?;
    header.set_sensitive(true);
    Ok(header)
}

/// Validates a canonical native credential secret without requiring it to be unexpired.
///
/// Expired stored credentials remain usable for an idempotent remote revoke.
///
/// # Errors
/// Returns [`Error::Malformed`] unless the secret encodes exactly 32 random bytes.
pub fn validate_secret(secret: &Secret) -> Result<(), Error> {
    // Check encoded length before allocation, including values read from keyring.
    if secret.expose().len() != SECRET_BYTES * 4 / 3 + 1 {
        return Err(Error::Malformed);
    }
    let decoded = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(secret.expose())
            .map_err(|_error| Error::Malformed)?,
    );
    let canonical = Zeroizing::new(URL_SAFE_NO_PAD.encode(decoded.as_slice()));
    if decoded.len() != SECRET_BYTES || canonical.as_str() != secret.expose() {
        return Err(Error::Malformed);
    }
    Ok(())
}

fn validate_credential(credential: &DeviceCredential) -> Result<(), Error> {
    validate_secret(&credential.secret)?;
    let remaining = credential.expires_at - time::OffsetDateTime::now_utc();
    if remaining <= time::Duration::ZERO || remaining > MAX_CREDENTIAL_LIFETIME {
        return Err(Error::Malformed);
    }
    Ok(())
}

fn validate_login(start: &DeviceLoginStart) -> Result<(), Error> {
    validate_secret(&start.poll_secret)?;
    let remaining = start.expires_at - time::OffsetDateTime::now_utc();
    if remaining <= time::Duration::ZERO
        || remaining > MAX_LOGIN_LIFETIME
        || start.interval_seconds == 0
        || start.interval_seconds > MAX_POLL_INTERVAL_SECONDS
        || time::Duration::seconds(i64::from(start.interval_seconds)) > remaining
        || start.user_code.is_empty()
        || !start.user_code.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(Error::Malformed);
    }
    for uri in
        std::iter::once(&start.verification_uri).chain(start.verification_uri_complete.iter())
    {
        if uri.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(Error::Malformed);
        }
        let url = url::Url::parse(uri).map_err(|_error| Error::Malformed)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::Malformed);
        }
    }
    Ok(())
}

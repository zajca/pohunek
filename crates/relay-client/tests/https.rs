//! Exercises the public SDK through real TLS, including hostile response framing.

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use pohunek_relay_client::{
    config::{Limits, Origin},
    Client, Error,
};
use relay_protocol::{
    AccountRecord, CredentialId, DeviceCredential, DeviceLoginStart, DevicePollResult, LoginId,
    PrincipalId, PrincipalKind, PrincipalState, RevokeCredentialRequest, Secret,
};
use std::{
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    origin: Origin,
    certificate: reqwest::Certificate,
    handle: axum_server::Handle<SocketAddr>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new(app: Router) -> Self {
        let directory = tempfile::tempdir().expect("isolated TLS fixture");
        let key = directory.path().join("tls.key");
        let certificate = directory.path().join("tls.crt");
        let output = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-nodes",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&certificate)
            .args([
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost,IP:127.0.0.1",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-days",
                "1",
            ])
            .output()
            .expect("OpenSSL fixture generator");
        assert!(output.status.success(), "TLS fixture generated");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))
            .expect("private key mode");
        let pem = std::fs::read(certificate).expect("fixture certificate");
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
            pem.clone(),
            std::fs::read(key).expect("fixture key"),
        )
        .await
        .expect("server TLS");
        let certificate = reqwest::Certificate::from_pem(&pem).expect("fixture trust anchor");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        listener
            .set_nonblocking(true)
            .expect("async fixture listener");
        let address = listener.local_addr().expect("fixture address");
        let handle = axum_server::Handle::new();
        let server = axum_server::from_tcp_rustls(listener, tls)
            .expect("TLS listener")
            .handle(handle.clone())
            .serve(app.into_make_service());
        let task = tokio::spawn(server);
        handle.listening().await.expect("fixture ready");
        Self {
            origin: Origin::parse(&format!("https://{address}")).expect("HTTPS origin"),
            certificate,
            handle,
            task,
            _directory: directory,
        }
    }

    fn client(&self, response_bytes: usize) -> Client {
        Client::new(
            self.origin.clone(),
            Limits {
                request_timeout: Duration::from_secs(2),
                response_bytes,
            },
            Some(self.certificate.clone()),
        )
        .expect("client")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.shutdown();
        self.task.abort();
    }
}

fn credential() -> DeviceCredential {
    DeviceCredential {
        credential_id: CredentialId::from_uuid(Uuid::nil()),
        secret: Secret::new(URL_SAFE_NO_PAD.encode([7; 32])),
        expires_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
    }
}

fn revoke_request() -> RevokeCredentialRequest {
    RevokeCredentialRequest {
        idempotency: relay_protocol::Idempotency {
            correlation_id: Uuid::now_v7(),
            idempotency_key: Uuid::now_v7(),
        },
    }
}

fn login() -> DeviceLoginStart {
    DeviceLoginStart {
        login_id: LoginId::from_uuid(Uuid::nil()),
        verification_uri: "https://issuer.example/device".to_owned(),
        verification_uri_complete: None,
        user_code: "ABCD-EFGH".to_owned(),
        expires_at: OffsetDateTime::now_utc() + time::Duration::minutes(5),
        interval_seconds: 1,
        poll_secret: Secret::new(URL_SAFE_NO_PAD.encode([9; 32])),
    }
}

#[derive(Clone)]
struct AuthFixture {
    polls: Arc<AtomicUsize>,
    revoked: Arc<AtomicBool>,
}

fn authenticated(headers: &HeaderMap) -> bool {
    let expected = format!(
        "Bearer {}.{}",
        credential().credential_id,
        credential().secret.expose()
    );
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        == Some(expected.as_str())
        && !headers.contains_key(header::COOKIE)
        && !headers.contains_key(header::ORIGIN)
}

async fn poll(
    State(state): State<AuthFixture>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if headers
        .get("x-pohunek-device-secret")
        .and_then(|value| value.to_str().ok())
        != Some(login().poll_secret.expose())
        || body["login_id"] != Uuid::nil().to_string()
        || headers.contains_key(header::AUTHORIZATION)
        || headers.contains_key(header::COOKIE)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let result = match state.polls.fetch_add(1, Ordering::SeqCst) {
        0 => DevicePollResult::Pending {
            retry_after_seconds: 1,
        },
        1 => DevicePollResult::SlowDown {
            retry_after_seconds: 5,
        },
        _ => DevicePollResult::Complete {
            credential: credential(),
        },
    };
    Json(result).into_response()
}

async fn account(State(state): State<AuthFixture>, headers: HeaderMap) -> Response {
    if !authenticated(&headers) || state.revoked.load(Ordering::SeqCst) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(AccountRecord {
        principal_id: PrincipalId::from_uuid(Uuid::nil()),
        kind: PrincipalKind::Human,
        state: PrincipalState::Active,
        identities: vec![],
    })
    .into_response()
}

async fn revoke(
    State(state): State<AuthFixture>,
    headers: HeaderMap,
    Json(request): Json<RevokeCredentialRequest>,
) -> StatusCode {
    if request.idempotency.correlation_id.is_nil() || request.idempotency.idempotency_key.is_nil() {
        return StatusCode::BAD_REQUEST;
    }
    if !authenticated(&headers) || state.revoked.load(Ordering::SeqCst) {
        return StatusCode::UNAUTHORIZED;
    }
    state.revoked.store(true, Ordering::SeqCst);
    StatusCode::NO_CONTENT
}

#[tokio::test]
async fn device_poll_account_and_self_revoke_use_exact_protected_headers() {
    let state = AuthFixture {
        polls: Arc::new(AtomicUsize::new(0)),
        revoked: Arc::new(AtomicBool::new(false)),
    };
    let app = Router::new()
        .route("/v1/auth/device/start", post(|| async { Json(login()) }))
        .route("/v1/auth/device/poll", post(poll))
        .route("/v1/account", get(account))
        .route("/v1/auth/credentials/self/revoke", post(revoke))
        .with_state(state.clone());
    let fixture = Fixture::new(app).await;
    let client = fixture.client(4096);
    let start = client.start_device().await.expect("start device login");
    assert_eq!(
        client
            .poll_device(&start.login_id, &start.poll_secret)
            .await
            .expect("pending"),
        DevicePollResult::Pending {
            retry_after_seconds: 1
        }
    );
    assert_eq!(
        client
            .poll_device(&start.login_id, &start.poll_secret)
            .await
            .expect("slow down"),
        DevicePollResult::SlowDown {
            retry_after_seconds: 5
        }
    );
    let DevicePollResult::Complete { credential } = client
        .poll_device(&start.login_id, &start.poll_secret)
        .await
        .expect("complete")
    else {
        panic!("expected issued credential");
    };
    assert_eq!(
        client
            .account(&credential)
            .await
            .expect("authenticated account")
            .kind,
        PrincipalKind::Human
    );
    let request = revoke_request();
    client
        .revoke_self(&credential, &request)
        .await
        .expect("compensating self revoke");
    assert!(state.revoked.load(Ordering::SeqCst));
    assert_eq!(
        client
            .revoke_self(&credential, &request)
            .await
            .expect_err("unauthenticated retry is not durable revoke proof"),
        Error::Unauthenticated
    );
    assert_eq!(
        client
            .account(&credential)
            .await
            .expect_err("revoked credential"),
        Error::Unauthenticated
    );
    assert!(!format!("{client:?}").contains(credential.secret.expose()));
}

#[tokio::test]
async fn self_revoke_rejects_a_valid_but_wrong_bearer_without_claiming_success() {
    let state = AuthFixture {
        polls: Arc::new(AtomicUsize::new(0)),
        revoked: Arc::new(AtomicBool::new(false)),
    };
    let app = Router::new()
        .route("/v1/auth/credentials/self/revoke", post(revoke))
        .with_state(state.clone());
    let fixture = Fixture::new(app).await;
    let client = fixture.client(4096);
    let mut wrong = credential();
    wrong.secret = Secret::new(URL_SAFE_NO_PAD.encode([8; 32]));

    assert_eq!(
        client
            .revoke_self(&wrong, &revoke_request())
            .await
            .expect_err("wrong valid bearer cannot prove self revoke"),
        Error::Unauthenticated
    );
    assert!(
        !state.revoked.load(Ordering::SeqCst),
        "a rejected bearer must not revoke the actual credential"
    );
}

#[tokio::test]
async fn owned_revoke_preserves_authentication_and_targets_the_undelivered_credential() {
    let app = Router::new().route(
        "/v1/account/credentials/{id}/revoke",
        post(
            |axum::extract::Path(id): axum::extract::Path<Uuid>,
             headers: HeaderMap,
             Json(request): Json<RevokeCredentialRequest>| async move {
                if !authenticated(&headers) {
                    return StatusCode::UNAUTHORIZED;
                }
                if request.idempotency.correlation_id.is_nil()
                    || request.idempotency.idempotency_key.is_nil()
                {
                    return StatusCode::BAD_REQUEST;
                }
                if id != Uuid::from_u128(42) {
                    return StatusCode::NOT_FOUND;
                }
                StatusCode::NO_CONTENT
            },
        ),
    );
    let fixture = Fixture::new(app).await;
    let client = fixture.client(4096);
    let request = revoke_request();
    client
        .revoke_owned(
            &credential(),
            CredentialId::from_uuid(Uuid::from_u128(42)),
            &request,
        )
        .await
        .expect("revoke distinct owned target");
    client
        .revoke_owned(
            &credential(),
            CredentialId::from_uuid(Uuid::from_u128(42)),
            &request,
        )
        .await
        .expect("exact retry");
    let mut invalid = credential();
    invalid.credential_id = CredentialId::from_uuid(Uuid::from_u128(99));
    assert_eq!(
        client
            .revoke_owned(
                &invalid,
                CredentialId::from_uuid(Uuid::from_u128(42)),
                &revoke_request(),
            )
            .await
            .expect_err("wrong current actor"),
        Error::Unauthenticated
    );
}

#[tokio::test]
async fn redirects_are_never_followed_and_remote_error_bodies_are_not_exposed() {
    let hits = Arc::new(AtomicUsize::new(0));
    let trap = Arc::clone(&hits);
    let app = Router::new()
        .route(
            "/v1/auth/device/start",
            post(|| async {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(header::LOCATION, "/trap?sentinel-secret")],
                    "sentinel-secret",
                )
            }),
        )
        .route(
            "/trap",
            post(move || async move {
                trap.fetch_add(1, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }),
        );
    let fixture = Fixture::new(app).await;
    let error = fixture
        .client(4096)
        .start_device()
        .await
        .expect_err("redirect refused");
    assert_eq!(error, Error::Redirect);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert!(!format!("{error:?} {error}").contains("sentinel-secret"));
}

#[tokio::test]
async fn streaming_bodies_are_bounded_without_a_content_length() {
    let app = Router::new().route(
        "/v1/auth/device/start",
        post(|| async {
            Body::from_stream(futures::stream::iter([
                Ok::<_, std::io::Error>(vec![b'x'; 64]),
                Ok(vec![b'y'; 64]),
            ]))
        }),
    );
    let fixture = Fixture::new(app).await;
    assert_eq!(
        fixture
            .client(80)
            .start_device()
            .await
            .expect_err("stream bounded"),
        Error::ResponseTooLarge
    );
}

#[tokio::test]
async fn malformed_replies_and_untrusted_tls_are_rejected() {
    let app = Router::new().route(
        "/v1/auth/device/start",
        post(|| async { Json(serde_json::json!({"secret": "sentinel-malformed"})) }),
    );
    let fixture = Fixture::new(app).await;
    assert_eq!(
        fixture
            .client(4096)
            .start_device()
            .await
            .expect_err("malformed reply"),
        Error::Malformed
    );
    let untrusted = Client::new(
        fixture.origin.clone(),
        Limits {
            request_timeout: Duration::from_secs(2),
            response_bytes: 4096,
        },
        None,
    )
    .expect("normal trust roots");
    assert_eq!(
        untrusted
            .start_device()
            .await
            .expect_err("untrusted certificate"),
        Error::Transport
    );
}

#[tokio::test]
async fn request_timeout_covers_a_stalled_response_body() {
    let app = Router::new().route(
        "/v1/auth/device/start",
        post(|| async {
            Body::from_stream(futures::stream::pending::<Result<Vec<u8>, std::io::Error>>())
        }),
    );
    let fixture = Fixture::new(app).await;
    let client = Client::new(
        fixture.origin.clone(),
        Limits {
            request_timeout: Duration::from_millis(30),
            response_bytes: 4096,
        },
        Some(fixture.certificate.clone()),
    )
    .expect("bounded client");
    assert_eq!(
        client
            .start_device()
            .await
            .expect_err("stalled response times out"),
        Error::Transport
    );
}

#[tokio::test]
async fn device_start_rejects_poll_delays_past_server_bounds_or_login_expiry() {
    for (interval_seconds, lifetime) in [(61, 300), (30, 5)] {
        let app = Router::new().route(
            "/v1/auth/device/start",
            post(move || async move {
                let mut start = login();
                start.interval_seconds = interval_seconds;
                start.expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(lifetime);
                Json(start)
            }),
        );
        let fixture = Fixture::new(app).await;
        assert_eq!(
            fixture
                .client(4096)
                .start_device()
                .await
                .expect_err("unsafe poll delay"),
            Error::Malformed
        );
    }
}

#[tokio::test]
async fn issued_credentials_cannot_have_unbounded_lifetimes() {
    let app = Router::new().route(
        "/v1/auth/device/poll",
        post(|| async {
            let mut issued = credential();
            issued.expires_at = OffsetDateTime::now_utc() + time::Duration::days(91);
            Json(DevicePollResult::Complete { credential: issued })
        }),
    );
    let fixture = Fixture::new(app).await;
    let start = login();
    assert_eq!(
        fixture
            .client(4096)
            .poll_device(&start.login_id, &start.poll_secret)
            .await
            .expect_err("oversized credential lifetime"),
        Error::Malformed
    );
}

fn rotation(expires_at: OffsetDateTime) -> relay_protocol::CredentialMutation {
    let id = CredentialId::from_uuid(Uuid::from_u128(1));
    relay_protocol::CredentialMutation {
        record: relay_protocol::CredentialRecord {
            credential_id: id,
            principal_id: PrincipalId::from_uuid(Uuid::nil()),
            kind: relay_protocol::CredentialKind::Human,
            issued_at: OffsetDateTime::now_utc(),
            expires_at,
            rotation_overlap_ends_at: None,
            last_used_at: None,
            revoked_at: None,
        },
        credential: Some(DeviceCredential {
            credential_id: id,
            secret: credential().secret,
            expires_at,
        }),
    }
}

fn rotation_request() -> relay_protocol::RotateCredentialRequest {
    relay_protocol::RotateCredentialRequest {
        overlap_seconds: 30,
        idempotency: relay_protocol::Idempotency {
            correlation_id: Uuid::nil(),
            idempotency_key: Uuid::from_u128(2),
        },
        expires_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
    }
}

#[tokio::test]
async fn only_the_exact_bounded_rotation_error_is_authoritative_expiry() {
    for (status, code, expected) in [
        (StatusCode::GONE, "rotation_expired", Error::RotationExpired),
        (StatusCode::GONE, "proxy_gone", Error::Remote(410)),
        (
            StatusCode::BAD_REQUEST,
            "rotation_rejected",
            Error::RotationRejected,
        ),
        (
            StatusCode::BAD_REQUEST,
            "proxy_rejected",
            Error::Remote(400),
        ),
        (StatusCode::GONE, "rotation_rejected", Error::Remote(410)),
    ] {
        let fixture = Fixture::new(Router::new().route(
            "/v1/account/credentials/{id}/rotate",
            post(move || async move {
                (
                    status,
                    Json(relay_protocol::ApiErrorBody {
                        error: relay_protocol::ApiError {
                            code: code.to_owned(),
                            message: "Request rejected".to_owned(),
                            request_id: "test-request".to_owned(),
                            recover: None,
                        },
                    }),
                )
            }),
        ))
        .await;
        let old = credential();
        assert_eq!(
            fixture
                .client(4096)
                .rotate(&old, old.credential_id, &rotation_request())
                .await
                .expect_err("typed expiry result"),
            expected
        );
    }
    let fixture = Fixture::new(Router::new().route(
        "/v1/account/credentials/{id}/rotate",
        post(|| async { (StatusCode::GONE, "<html>Proxy resource removed</html>") }),
    ))
    .await;
    let old = credential();
    assert_eq!(
        fixture
            .client(4096)
            .rotate(&old, old.credential_id, &rotation_request())
            .await
            .expect_err("proxy response is not a receipt decision"),
        Error::Malformed
    );
    let fixture = Fixture::new(Router::new().route(
        "/v1/account/credentials/{id}/rotate",
        post(|| async {
            (
                StatusCode::GONE,
                Json(relay_protocol::ApiErrorBody {
                    error: relay_protocol::ApiError {
                        code: "rotation_expired".to_owned(),
                        message: "x".repeat(8192),
                        request_id: "test-request".to_owned(),
                        recover: None,
                    },
                }),
            )
        }),
    ))
    .await;
    assert_eq!(
        fixture
            .client(4096)
            .rotate(&old, old.credential_id, &rotation_request())
            .await
            .expect_err("oversized expiry response"),
        Error::ResponseTooLarge
    );
}

#[tokio::test]
async fn rotation_preserves_selected_expiry_and_delivers_a_secret_only_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/account/credentials/{id}/rotate",
        post(
            move |headers: HeaderMap,
                  Json(request): Json<relay_protocol::RotateCredentialRequest>| {
                let calls = Arc::clone(&calls);
                async move {
                    if !authenticated(&headers) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let mut result = rotation(request.expires_at);
                    if calls.fetch_add(1, Ordering::SeqCst) > 0 {
                        result.credential = None;
                    }
                    Json(result).into_response()
                }
            },
        ),
    );
    let fixture = Fixture::new(app).await;
    let client = fixture.client(4096);
    let old = credential();
    let request = rotation_request();
    let issued = client
        .rotate(&old, old.credential_id, &request)
        .await
        .expect("rotation");
    assert_eq!(issued.record.expires_at, request.expires_at);
    assert!(issued.credential.is_some());
    assert!(client
        .rotate(&old, old.credential_id, &request)
        .await
        .expect("exact retry metadata")
        .credential
        .is_none());
}

#[tokio::test]
async fn rotation_rejects_reused_ids_and_inconsistent_or_ignored_expiry() {
    for invalid in 0..3 {
        let app = Router::new().route(
            "/v1/account/credentials/{id}/rotate",
            post(
                move |Json(request): Json<relay_protocol::RotateCredentialRequest>| async move {
                    let mut result = rotation(request.expires_at);
                    match invalid {
                        0 => {
                            result.record.credential_id = CredentialId::from_uuid(Uuid::nil());
                            result
                                .credential
                                .as_mut()
                                .expect("first delivery")
                                .credential_id = result.record.credential_id;
                        }
                        1 => {
                            result
                                .credential
                                .as_mut()
                                .expect("first delivery")
                                .expires_at += time::Duration::minutes(1);
                        }
                        _ => {
                            result.record.expires_at += time::Duration::minutes(1);
                            result
                                .credential
                                .as_mut()
                                .expect("first delivery")
                                .expires_at = result.record.expires_at;
                        }
                    }
                    Json(result)
                },
            ),
        );
        let fixture = Fixture::new(app).await;
        let old = credential();
        assert_eq!(
            fixture
                .client(4096)
                .rotate(&old, old.credential_id, &rotation_request())
                .await
                .expect_err("malformed rotation"),
            Error::Malformed
        );
    }
}

#[tokio::test]
async fn json_endpoints_reject_unexpected_success_statuses() {
    for status in [
        StatusCode::CREATED,
        StatusCode::ACCEPTED,
        StatusCode::PARTIAL_CONTENT,
    ] {
        let app = Router::new().route(
            "/v1/auth/device/start",
            post(move || async move { (status, Json(login())) }),
        );
        let fixture = Fixture::new(app).await;
        assert_eq!(
            fixture
                .client(4096)
                .start_device()
                .await
                .expect_err("wrong successful status"),
            Error::Malformed
        );
    }
}

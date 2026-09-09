//! Exercises the actual runtime with `PostgreSQL` and native TLS sockets.

use super::*;
use axum::{extract::State, routing::get, Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::AsyncReadExt,
    sync::{oneshot, Notify},
    task::JoinHandle,
};

struct Ingress {
    fixture: Fixture,
    config: Config,
    client: reqwest::Client,
    issuer_handle: Handle<SocketAddr>,
    issuer_task: JoinHandle<std::io::Result<()>>,
    device_entered: Arc<Notify>,
    device_requests: Arc<AtomicUsize>,
    token_entered: Arc<Notify>,
    token_release: Arc<Notify>,
    token_requests: Arc<AtomicUsize>,
    token_nonce: Arc<Mutex<Option<String>>>,
}

#[derive(Clone)]
struct IssuerState {
    entered: Arc<Notify>,
    requests: Arc<AtomicUsize>,
    token_entered: Arc<Notify>,
    token_release: Arc<Notify>,
    token_requests: Arc<AtomicUsize>,
    token_nonce: Arc<Mutex<Option<String>>>,
    signing_key: PathBuf,
    issuer: String,
}

impl Ingress {
    async fn new() -> Self {
        let fixture = Fixture::new().await;
        let directory = fixture.directory.path();
        let (certificate_file, private_key_file, certificate) = tls_files(directory);
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
            certificate.clone(),
            fs::read(&private_key_file).expect("fixture key"),
        )
        .await
        .expect("fixture TLS");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("issuer listener");
        listener
            .set_nonblocking(true)
            .expect("async issuer listener");
        let issuer_address = listener.local_addr().expect("issuer address");
        let issuer = format!("https://{issuer_address}/issuer");
        let signing_key = issuer_signing_key(directory);
        let jwks = issuer_jwks(&signing_key);
        let metadata = serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "device_authorization_endpoint": format!("{issuer}/device"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"]
        });
        let device_entered = Arc::new(Notify::new());
        let device_requests = Arc::new(AtomicUsize::new(0));
        let token_entered = Arc::new(Notify::new());
        let token_release = Arc::new(Notify::new());
        let token_requests = Arc::new(AtomicUsize::new(0));
        let token_nonce = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route(
                "/issuer/.well-known/openid-configuration",
                get(move || async { Json(metadata) }),
            )
            .route(
                "/issuer/jwks",
                get(move || {
                    let jwks = jwks.clone();
                    async move { Json(jwks) }
                }),
            )
            .route("/issuer/device", axum::routing::post(held_device))
            .route("/issuer/token", axum::routing::post(held_token))
            .with_state(IssuerState {
                entered: Arc::clone(&device_entered),
                requests: Arc::clone(&device_requests),
                token_entered: Arc::clone(&token_entered),
                token_release: Arc::clone(&token_release),
                token_requests: Arc::clone(&token_requests),
                token_nonce: Arc::clone(&token_nonce),
                signing_key,
                issuer: issuer.clone(),
            });
        let issuer_handle = Handle::new();
        let server = axum_server::from_tcp_rustls(listener, tls)
            .expect("issuer socket")
            .handle(issuer_handle.clone())
            .serve(app.into_make_service());
        let issuer_task = tokio::spawn(server);
        issuer_handle.listening().await.expect("issuer ready");
        let config = configuration(&fixture, &issuer, certificate_file, private_key_file);
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .add_root_certificate(reqwest::Certificate::from_pem(&certificate).expect("fixture CA"))
            .timeout(Duration::from_secs(3))
            .pool_max_idle_per_host(0)
            .build()
            .expect("TLS client");
        let probe = client
            .get(format!(
                "{}/.well-known/openid-configuration",
                config.issuer
            ))
            .send()
            .await
            .expect("issuer TLS probe");
        assert_eq!(probe.status(), reqwest::StatusCode::OK, "discovery route");
        Self {
            fixture,
            config,
            client,
            issuer_handle,
            issuer_task,
            device_entered,
            device_requests,
            token_entered,
            token_release,
            token_requests,
            token_nonce,
        }
    }

    async fn start(&self) -> Running {
        let (stop, stopped) = oneshot::channel();
        let (ready, address) = oneshot::channel();
        let config = self.config.clone();
        let task = tokio::spawn(run_notifying(
            config,
            async { stopped.await.map_err(|_error| RuntimeError::Signal) },
            Some(ready),
        ));
        let address = match timeout(Duration::from_secs(5), address)
            .await
            .expect("bounded startup")
        {
            Ok(address) => address,
            Err(error) => panic!("runtime startup failed after {error}: {:?}", task.await),
        };
        Running {
            address,
            stop,
            task,
        }
    }

    async fn cleanup(self) {
        self.issuer_handle.shutdown();
        self.issuer_task
            .await
            .expect("issuer task")
            .expect("issuer stop");
        self.fixture.cleanup().await;
    }
}

fn private_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("fixture private file");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private mode");
}

async fn held_device(State(state): State<IssuerState>) -> Json<serde_json::Value> {
    state.requests.fetch_add(1, Ordering::Relaxed);
    state.entered.notify_one();
    std::future::pending().await
}

async fn held_token(State(state): State<IssuerState>) -> Json<serde_json::Value> {
    state.token_requests.fetch_add(1, Ordering::Relaxed);
    state.token_entered.notify_one();
    state.token_release.notified().await;
    let nonce = state
        .token_nonce
        .lock()
        .expect("token nonce lock")
        .clone()
        .expect("callback nonce");
    Json(serde_json::json!({
        "access_token": "fixture-access-token",
        "token_type": "Bearer",
        "id_token": signed_id_token(&state.signing_key, &state.issuer, &nonce),
    }))
}

struct Running {
    address: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), RuntimeError>>,
}

impl Running {
    fn url(&self, path: &str) -> String {
        format!("https://{}{path}", self.address)
    }

    async fn shutdown(self) -> Result<(), RuntimeError> {
        self.stop.send(()).expect("runtime still running");
        timeout(Duration::from_secs(2), self.task)
            .await
            .expect("bounded runtime stop")
            .expect("runtime task")
    }
}

#[tokio::test]
async fn native_tls_runtime_serves_readiness_and_cleanly_restarts() {
    let ingress = Ingress::new().await;
    for _ in 0..2 {
        let running = ingress.start().await;
        let response = ingress
            .client
            .get(running.url("/readyz"))
            .send()
            .await
            .expect("native TLS readiness");
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
        assert_eq!(response.headers()["cache-control"], "no-store");
        running.shutdown().await.expect("clean native TLS shutdown");
        assert!(
            !ingress
                .fixture
                .witness
                .latest()
                .expect("verified witness")
                .expect("checkpoint")
                .active_run
        );
    }
    ingress.cleanup().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "The security regression keeps one complete cross-flow HTTP sequence together."
)]
async fn browser_and_device_flows_share_capacity_and_control_bad_callbacks() {
    let mut ingress = Ingress::new().await;
    ingress.config.auth.pending_transactions = 1;
    let running = ingress.start().await;
    let browser = ingress
        .client
        .post(running.url("/v1/auth/browser/start"))
        .header("origin", "https://localhost:9443")
        .send()
        .await
        .expect("start browser login");
    assert_eq!(browser.status(), reqwest::StatusCode::SEE_OTHER);
    let authorization = url::Url::parse(
        browser
            .headers()
            .get("location")
            .expect("browser authorization redirect")
            .to_str()
            .expect("ASCII redirect"),
    )
    .expect("valid authorization URL");
    let state = authorization
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then_some(value.into_owned()))
        .expect("opaque browser state");
    let login_cookie = browser
        .headers()
        .get("set-cookie")
        .expect("browser binding cookie")
        .to_str()
        .expect("ASCII cookie")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned();

    let rejected = ingress
        .client
        .post(running.url("/v1/auth/device/start"))
        .send()
        .await
        .expect("capacity rejection response");
    assert_eq!(rejected.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        ingress.device_requests.load(Ordering::Relaxed),
        0,
        "a capacity rejection cannot call the issuer"
    );

    let malformed = ingress
        .client
        .get(running.url(&format!(
            "/v1/auth/oidc/callback?state={state}&state=duplicate"
        )))
        .header("cookie", &login_cookie)
        .send()
        .await
        .expect("controlled duplicate callback response");
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(malformed.headers()["cache-control"], "no-store");
    assert_eq!(malformed.headers()["pragma"], "no-cache");
    let cleared = malformed
        .headers()
        .get("set-cookie")
        .expect("duplicate callback clears browser binding")
        .to_str()
        .expect("ASCII cleared cookie");
    assert!(cleared.starts_with("__Host-pohunek-relay-login="));
    assert!(cleared.contains("Secure; HttpOnly; SameSite=Lax; Max-Age=0"));

    let missing_cookie = ingress
        .client
        .get(running.url(&format!("/v1/auth/oidc/callback?state={state}")))
        .send()
        .await
        .expect("controlled missing-cookie callback response");
    assert_eq!(missing_cookie.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(missing_cookie.headers()["cache-control"], "no-store");
    assert!(missing_cookie
        .headers()
        .get("set-cookie")
        .expect("missing-cookie response clears the binding")
        .to_str()
        .expect("ASCII cleared cookie")
        .contains("Max-Age=0"));

    let mixed = ingress
        .client
        .get(running.url(&format!("/v1/auth/oidc/callback?state={state}")))
        .header("cookie", &login_cookie)
        .header("authorization", "Bearer malformed-callback-coordinate")
        .send()
        .await
        .expect("controlled mixed-auth callback response");
    assert_eq!(mixed.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(mixed.headers()["cache-control"], "no-store");
    assert!(mixed
        .headers()
        .get("set-cookie")
        .expect("mixed callback clears browser binding")
        .to_str()
        .expect("ASCII cleared cookie")
        .contains("Max-Age=0"));

    let still_rejected = ingress
        .client
        .post(running.url("/v1/auth/device/start"))
        .send()
        .await
        .expect("capacity remains occupied after unbound malformed callback");
    assert_eq!(
        still_rejected.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(ingress.device_requests.load(Ordering::Relaxed), 0);

    let terminal = ingress
        .client
        .get(running.url(&format!(
            "/v1/auth/oidc/callback?error=access_denied&state={state}&error_description=provider+detail&error_uri=https%3A%2F%2Fissuer.example%2Ferror"
        )))
        .header("cookie", &login_cookie)
        .send()
        .await
        .expect("bound provider denial response");
    assert_eq!(terminal.status(), reqwest::StatusCode::BAD_REQUEST);

    let client = ingress.client.clone();
    let device_url = running.url("/v1/auth/device/start");
    let device = tokio::spawn(async move { client.post(device_url).send().await });
    timeout(Duration::from_secs(1), ingress.device_entered.notified())
        .await
        .expect("device issuer request after terminal browser callback");
    assert_eq!(ingress.device_requests.load(Ordering::Relaxed), 1);
    device.abort();
    let _ = device.await;
    tokio::task::yield_now().await;

    let replacement = ingress
        .client
        .post(running.url("/v1/auth/browser/start"))
        .header("origin", "https://localhost:9443")
        .send()
        .await
        .expect("cancelled device reservation releases browser admission");
    assert_eq!(replacement.status(), reqwest::StatusCode::SEE_OTHER);
    running.shutdown().await.expect("clean runtime shutdown");
    ingress.cleanup().await;
}

#[tokio::test]
async fn successful_browser_callback_holds_shared_capacity_until_session_issue_finishes() {
    let mut ingress = Ingress::new().await;
    ingress.config.auth.pending_transactions = 1;
    let running = ingress.start().await;
    let browser = ingress
        .client
        .post(running.url("/v1/auth/browser/start"))
        .header("origin", "https://localhost:9443")
        .send()
        .await
        .expect("start browser login");
    assert_eq!(browser.status(), reqwest::StatusCode::SEE_OTHER);
    let authorization = url::Url::parse(
        browser
            .headers()
            .get("location")
            .expect("browser authorization redirect")
            .to_str()
            .expect("ASCII redirect"),
    )
    .expect("valid authorization URL");
    let mut query = authorization.query_pairs();
    let state = query
        .clone()
        .find_map(|(key, value)| (key == "state").then_some(value.into_owned()))
        .expect("opaque browser state");
    let nonce = query
        .find_map(|(key, value)| (key == "nonce").then_some(value.into_owned()))
        .expect("opaque browser nonce");
    *ingress.token_nonce.lock().expect("token nonce lock") = Some(nonce);
    let login_cookie = browser
        .headers()
        .get("set-cookie")
        .expect("browser binding cookie")
        .to_str()
        .expect("ASCII cookie")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned();
    let client = ingress.client.clone();
    let callback_url = running.url(&format!(
        "/v1/auth/oidc/callback?code=fixture-code&state={state}&session_state=provider-session"
    ));
    let callback = tokio::spawn(async move {
        client
            .get(callback_url)
            .header("cookie", login_cookie)
            .send()
            .await
    });
    timeout(Duration::from_secs(1), ingress.token_entered.notified())
        .await
        .expect("browser callback entered token exchange");
    let rejected = ingress
        .client
        .post(running.url("/v1/auth/device/start"))
        .send()
        .await
        .expect("capacity rejection response");
    assert_eq!(rejected.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        ingress.device_requests.load(Ordering::Relaxed),
        0,
        "an over-capacity flow cannot call its issuer endpoint"
    );
    ingress.token_release.notify_one();
    let callback = timeout(Duration::from_secs(2), callback)
        .await
        .expect("completed browser callback")
        .expect("callback task")
        .expect("callback response");
    assert_eq!(callback.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(ingress.token_requests.load(Ordering::Relaxed), 1);
    let replacement = ingress
        .client
        .post(running.url("/v1/auth/browser/start"))
        .header("origin", "https://localhost:9443")
        .send()
        .await
        .expect("capacity reuse after successful browser callback");
    assert_eq!(replacement.status(), reqwest::StatusCode::SEE_OTHER);
    running.shutdown().await.expect("clean runtime shutdown");
    ingress.cleanup().await;
}

#[tokio::test]
async fn forced_shutdown_cancels_stalled_tls_and_preserves_dirty_evidence() {
    let ingress = Ingress::new().await;
    let running = ingress.start().await;
    let mut stalled = tokio::net::TcpStream::connect(running.address)
        .await
        .expect("stalled TLS socket");
    assert!(
        timeout(Duration::from_millis(30), stalled.read_u8())
            .await
            .is_err(),
        "handshake is held open"
    );
    assert!(matches!(
        running.shutdown().await,
        Err(RuntimeError::Shutdown)
    ));
    let result = timeout(Duration::from_millis(100), stalled.read_u8())
        .await
        .expect("pre-TLS socket cancelled");
    result.expect_err("stalled socket cannot survive failure");
    assert!(
        ingress
            .fixture
            .witness
            .latest()
            .expect("verified witness")
            .expect("checkpoint")
            .active_run
    );
    ingress.cleanup().await;
}

#[tokio::test]
async fn fence_loss_cancels_inflight_request_and_pre_tls_socket() {
    let ingress = Ingress::new().await;
    let running = ingress.start().await;
    let mut stalled = tokio::net::TcpStream::connect(running.address)
        .await
        .expect("pre-TLS socket");
    let client = ingress.client.clone();
    let url = running.url("/v1/auth/device/start");
    let request = tokio::spawn(async move { client.post(url).send().await });
    timeout(Duration::from_secs(1), ingress.device_entered.notified())
        .await
        .expect("real handler entered issuer");
    let changed = std::time::Instant::now();
    sqlx::query("UPDATE relay_lease SET fence_token = $1")
        .bind(Uuid::now_v7())
        .execute(ingress.fixture.store.pool())
        .await
        .expect("replace active fence");
    let result = timeout(Duration::from_millis(250), running.task)
        .await
        .expect("bounded fence-loss stop")
        .expect("runtime task");
    assert!(matches!(result, Err(RuntimeError::Authority(_))));
    assert!(changed.elapsed() < Duration::from_millis(250));
    timeout(Duration::from_millis(100), stalled.read_u8())
        .await
        .expect("pre-TLS failure bounded")
        .expect_err("pre-TLS socket closed");
    timeout(Duration::from_millis(100), request)
        .await
        .expect("inflight request cancelled")
        .expect("request task")
        .expect_err("no response after fence failure");
    assert!(
        tokio::net::TcpStream::connect(running.address)
            .await
            .is_err(),
        "listener stopped"
    );
    assert!(
        ingress
            .fixture
            .witness
            .latest()
            .expect("verified witness")
            .expect("checkpoint")
            .active_run
    );
    ingress.cleanup().await;
}

#[tokio::test]
async fn total_socket_lifetime_bounds_stalled_tls_before_request_timeout() {
    let mut ingress = Ingress::new().await;
    ingress.config.limits.connection_lifetime = Duration::from_millis(30);
    let running = ingress.start().await;
    let mut stalled = tokio::net::TcpStream::connect(running.address)
        .await
        .expect("TLS socket");
    timeout(Duration::from_millis(200), stalled.read_u8())
        .await
        .expect("total lifetime includes TLS")
        .expect_err("stalled TLS expired before the two-second request timeout");
    running
        .shutdown()
        .await
        .expect("expired socket fully drained");
    ingress.cleanup().await;
}

#[tokio::test]
async fn native_tls_capacity_rejects_a_connection_flood_and_reopens_after_expiry() {
    let mut ingress = Ingress::new().await;
    ingress.config.limits.connections = 1;
    ingress.config.limits.per_team = 1;
    ingress.config.limits.per_principal = 1;
    ingress.config.limits.connection_lifetime = Duration::from_millis(500);
    let running = ingress.start().await;
    let mut held = tokio::net::TcpStream::connect(running.address)
        .await
        .expect("held TLS slot");
    for _ in 0..16 {
        let mut rejected = tokio::net::TcpStream::connect(running.address)
            .await
            .expect("excess socket");
        timeout(Duration::from_millis(100), rejected.read_u8())
            .await
            .expect("excess socket promptly rejected")
            .expect_err("no extra TLS task admitted");
    }
    timeout(Duration::from_secs(1), held.read_u8())
        .await
        .expect("held TLS lifetime expires")
        .expect_err("TLS slot released");
    let response = ingress
        .client
        .get(running.url("/readyz"))
        .send()
        .await
        .expect("capacity reused by native TLS");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    running.shutdown().await.expect("clean stop after flood");
    ingress.cleanup().await;
}

fn tls_files(directory: &Path) -> (PathBuf, PathBuf, Vec<u8>) {
    let certificate_file = directory.join("tls.crt");
    let private_key_file = directory.join("tls.key");
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
        .arg(&private_key_file)
        .arg("-out")
        .arg(&certificate_file)
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
    assert!(output.status.success(), "generate ephemeral TLS fixture");
    for path in [&certificate_file, &private_key_file] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private TLS files");
    }
    let certificate = fs::read(&certificate_file).expect("fixture certificate");

    (certificate_file, private_key_file, certificate)
}

fn issuer_signing_key(directory: &Path) -> PathBuf {
    let key = directory.join("issuer-signing.key");
    let output = Command::new("openssl")
        .args([
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
        ])
        .arg("-out")
        .arg(&key)
        .output()
        .expect("OpenSSL issuer key generator");
    assert!(output.status.success(), "generate ephemeral issuer key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("private issuer key");
    key
}

fn issuer_jwks(key: &Path) -> serde_json::Value {
    let public_key = key.with_extension("pub");
    let output = Command::new("openssl")
        .arg("rsa")
        .arg("-in")
        .arg(key)
        .arg("-pubout")
        .arg("-out")
        .arg(&public_key)
        .output()
        .expect("OpenSSL issuer public key");
    assert!(output.status.success(), "derive issuer public key");
    let output = Command::new("openssl")
        .arg("rsa")
        .arg("-pubin")
        .arg("-in")
        .arg(&public_key)
        .args(["-text", "-noout"])
        .output()
        .expect("OpenSSL issuer public key inspection");
    assert!(output.status.success(), "inspect issuer public key");
    let text = String::from_utf8(output.stdout).expect("ASCII issuer public key");
    let modulus = text
        .split("Modulus:")
        .nth(1)
        .and_then(|value| value.split("Exponent:").next())
        .expect("RSA modulus");
    let modulus: String = modulus.chars().filter(char::is_ascii_hexdigit).collect();
    let mut modulus = hex::decode(modulus).expect("hexadecimal RSA modulus");
    if modulus.first() == Some(&0) {
        modulus.remove(0);
    }
    serde_json::json!({
        "keys": [{
            "kty": "RSA",
            "kid": "fixture",
            "alg": "RS256",
            "use": "sig",
            "n": URL_SAFE_NO_PAD.encode(modulus),
            "e": "AQAB",
        }]
    })
}

fn signed_id_token(key: &Path, issuer: &str, nonce: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","kid":"fixture","typ":"JWT"}"#);
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::json!({
            "iss": issuer,
            "sub": "fixture-subject",
            "aud": "fixture",
            "exp": now + 60,
            "iat": now,
            "nonce": nonce,
        })
        .to_string(),
    );
    let input = format!("{header}.{payload}");
    let directory = key.parent().expect("issuer key parent");
    let input_file = directory.join("issuer-token-input");
    let signature_file = directory.join("issuer-token-signature");
    private_file(&input_file, input.as_bytes());
    let output = Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(key)
        .arg("-out")
        .arg(&signature_file)
        .arg(&input_file)
        .output()
        .expect("OpenSSL ID token signer");
    assert!(output.status.success(), "sign fixture ID token");
    let signature = fs::read(&signature_file).expect("ID token signature");
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

fn configuration(
    fixture: &Fixture,
    issuer: &str,
    certificate_file: PathBuf,
    private_key_file: PathBuf,
) -> Config {
    let directory = fixture.directory.path();
    let database_url_file = directory.join("database-url");
    let url = std::env::var("POHUNEK_RELAY_TEST_DATABASE_URL").expect("explicit fixture URL");
    private_file(
        &database_url_file,
        format!("{url}?options[search_path]={}", fixture.schema).as_bytes(),
    );
    let digest_key_file = directory.join("digest-key");
    private_file(&digest_key_file, &[2; 32]);
    let witness_key_file = directory.join("witness-key");
    private_file(&witness_key_file, &[3; 32]);
    let mut limits = crate::config::fixture_limits();
    limits.body_bytes = 4096;
    limits.response_bytes = 16 * 1024;
    limits.connections = 4;
    limits.per_team = 4;
    limits.per_principal = 2;
    limits.requests_per_window = 100;
    limits.request_timeout = Duration::from_secs(2);
    limits.shutdown_timeout = Duration::from_millis(150);
    limits.lease_renew = Duration::from_millis(500);
    Config {
        relay_id: fixture.relay_id.clone(),
        bind: "127.0.0.1:0".parse().expect("relay bind"),
        public_origin: "https://localhost:9443/".parse().expect("public origin"),
        callback: "https://localhost:9443/v1/auth/oidc/callback"
            .parse()
            .expect("callback"),
        tls: Tls::Native {
            certificate_file: certificate_file.clone(),
            private_key_file,
        },
        database_url_file,
        digest_key_file,
        digest_key_id: "fixture".to_owned(),
        witness_key_file,
        witness_key_id: "test".to_owned(),
        issuer: issuer.parse().expect("issuer URL"),
        client_id: "fixture".to_owned(),
        ca_file: Some(certificate_file),
        witness_dir: directory.to_path_buf(),
        limits,
        logging: crate::config::LogConfig {
            directory: directory.join("logs"),
            max_file_bytes: 4096,
            max_files: 2,
        },
        auth: crate::config::AuthConfig {
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
            max_device_poll_interval: Duration::from_secs(10),
        },
        login_policy: crate::config::LoginPolicy::AnyAuthenticatedSubject,
    }
}

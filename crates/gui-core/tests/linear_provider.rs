//! Linear provider client tests.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pohunek_gui_core::providers::linear::{
    GraphqlTransport, GraphqlTransportError, LinearClient, LinearConfig, LinearError, LinearQuery,
    TokenError, TokenFuture, TokenSource, TransportFuture,
};
use serde_json::{json, Value};

const TOKEN_KEY: &str = "linear-token-ref";
const SECRET_TOKEN: &str = "lin_api_secret_fixture";

#[derive(Debug, Clone)]
struct FakeTokenSource {
    calls: Arc<Mutex<Vec<String>>>,
    result: Arc<Mutex<Result<String, TokenError>>>,
}

impl FakeTokenSource {
    fn new(token: impl Into<String>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            result: Arc::new(Mutex::new(Ok(token.into()))),
        }
    }

    fn failing(message: impl Into<String>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            result: Arc::new(Mutex::new(Err(TokenError::new(message)))),
        }
    }

    fn set_token(&self, token: impl Into<String>) {
        *self.result.lock().expect("token result lock") = Ok(token.into());
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("token calls lock").clone()
    }
}

impl TokenSource for FakeTokenSource {
    fn token<'a>(&'a self, token_key: &'a str) -> TokenFuture<'a> {
        let calls = Arc::clone(&self.calls);
        let result = Arc::clone(&self.result);
        let token_key = token_key.to_owned();
        Box::pin(async move {
            calls.lock().expect("token calls lock").push(token_key);
            result.lock().expect("token result lock").clone()
        })
    }
}

#[derive(Debug, Clone)]
struct HangingTokenSource;

impl TokenSource for HangingTokenSource {
    fn token<'a>(&'a self, _token_key: &'a str) -> TokenFuture<'a> {
        Box::pin(std::future::pending())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedCall {
    endpoint: String,
    token: String,
    body: Value,
}

#[derive(Debug, Clone)]
struct FakeTransport {
    calls: Arc<Mutex<Vec<RecordedCall>>>,
    response: Arc<Mutex<Result<Value, GraphqlTransportError>>>,
}

impl FakeTransport {
    fn new(response: Value) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(Mutex::new(Ok(response))),
        }
    }

    fn failing(message: impl Into<String>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(Mutex::new(Err(GraphqlTransportError::new(message)))),
        }
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().expect("transport calls lock").clone()
    }
}

impl GraphqlTransport for FakeTransport {
    fn post_graphql<'a>(
        &'a self,
        endpoint: &'a str,
        token: &'a str,
        body: Value,
    ) -> TransportFuture<'a> {
        let calls = Arc::clone(&self.calls);
        let response = Arc::clone(&self.response);
        let endpoint = endpoint.to_owned();
        let token = token.to_owned();
        Box::pin(async move {
            calls
                .lock()
                .expect("transport calls lock")
                .push(RecordedCall {
                    endpoint,
                    token,
                    body,
                });
            response.lock().expect("transport response lock").clone()
        })
    }
}

fn config(token_key: impl Into<String>) -> LinearConfig {
    LinearConfig {
        token_key: token_key.into(),
        endpoint: "https://linear.example/graphql".to_owned(),
        token_lookup_timeout: Duration::from_secs(1),
    }
}

fn issues_response() -> Value {
    json!({
        "data": {
            "issues": {
                "nodes": [
                    {
                        "id": "opaque-linear-id",
                        "identifier": "LIN-123",
                        "title": "Fix launcher",
                        "description": "Issue body",
                        "branchName": "lin-123-fix-launcher",
                        "url": "https://linear.example/LIN-123"
                    }
                ]
            }
        }
    })
}

fn assert_send<T: Send>(_: T) {}

#[tokio::test]
async fn list_issues_reads_token_at_call_time_and_builds_query() {
    let tokens = FakeTokenSource::new(SECRET_TOKEN);
    let transport = FakeTransport::new(issues_response());
    let client = LinearClient::new(config(TOKEN_KEY), tokens.clone(), transport.clone());
    assert_send(client.list_issues(LinearQuery::default()));

    let issues = client
        .list_issues(LinearQuery {
            filter: Some(json!({ "state": { "type": { "in": ["started"] } } })),
            search: Some("launcher".to_owned()),
            limit: 25,
        })
        .await
        .expect("assigned issues");

    assert_eq!(tokens.calls(), vec![TOKEN_KEY]);
    assert_eq!(issues.len(), 1);
    let issue = &issues[0];
    assert_eq!(issue.id, "opaque-linear-id");
    assert_eq!(issue.identifier, "LIN-123");
    assert_eq!(issue.title, "Fix launcher");
    assert_eq!(issue.body, "Issue body");
    assert_eq!(issue.branch, "lin-123-fix-launcher");
    assert_eq!(issue.url, "https://linear.example/LIN-123");
    assert_eq!(issue.prompt_item_id(), "LIN-123");
    assert_eq!(
        issue.to_prompt_json(),
        json!({
            "id": "opaque-linear-id",
            "identifier": "LIN-123",
            "title": "Fix launcher",
            "description": "Issue body",
            "body": "Issue body",
            "branchName": "lin-123-fix-launcher",
            "branch": "lin-123-fix-launcher",
            "url": "https://linear.example/LIN-123"
        })
    );

    tokens.set_token("rotated_linear_token_fixture");
    client
        .list_issues(LinearQuery::default())
        .await
        .expect("assigned issues after token rotation");

    let calls = transport.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].endpoint, "https://linear.example/graphql");
    assert_eq!(calls[0].token, SECRET_TOKEN);
    assert_eq!(calls[1].token, "rotated_linear_token_fixture");

    let request = &calls[0].body;
    let query = request
        .get("query")
        .and_then(Value::as_str)
        .expect("GraphQL query string");
    assert!(query.contains("issues"));
    assert!(query.contains("id"));
    assert!(query.contains("identifier"));
    assert!(query.contains("title"));
    assert!(query.contains("description"));
    assert!(query.contains("branchName"));
    assert!(query.contains("url"));
    assert!(query.contains("IssueFilter"));

    assert_eq!(request["variables"]["first"], 25);
    // The raw filter and the search term are combined with a logical AND.
    assert_eq!(
        request["variables"]["filter"]["and"][0]["state"]["type"]["in"][0],
        "started"
    );
    assert_eq!(
        request["variables"]["filter"]["and"][1]["searchableContent"]["containsIgnoreCase"],
        "launcher"
    );
    assert!(!request.to_string().contains(SECRET_TOKEN));

    let debug = format!("{client:?}");
    assert!(debug.contains("LinearClient"));
    assert!(!debug.contains(SECRET_TOKEN));
    assert!(!debug.contains("rotated_linear_token_fixture"));
}

#[tokio::test]
async fn missing_token_key_fails_before_token_or_transport_lookup() {
    let tokens = FakeTokenSource::new(SECRET_TOKEN);
    let transport = FakeTransport::new(issues_response());
    let client = LinearClient::new(config("  "), tokens.clone(), transport.clone());

    let err = client
        .list_issues(LinearQuery::default())
        .await
        .expect_err("missing token key");

    assert!(matches!(err, LinearError::MissingTokenKey));
    assert!(tokens.calls().is_empty());
    assert!(transport.calls().is_empty());
    assert!(!format!("{err:?}").contains(SECRET_TOKEN));
}

#[tokio::test]
async fn missing_endpoint_fails_before_token_or_transport_lookup() {
    let tokens = FakeTokenSource::new(SECRET_TOKEN);
    let transport = FakeTransport::new(issues_response());
    let client = LinearClient::new(
        LinearConfig {
            token_key: TOKEN_KEY.to_owned(),
            endpoint: "  ".to_owned(),
            token_lookup_timeout: Duration::from_secs(1),
        },
        tokens.clone(),
        transport.clone(),
    );

    let err = client
        .list_issues(LinearQuery::default())
        .await
        .expect_err("missing endpoint");

    assert!(matches!(err, LinearError::MissingEndpoint));
    assert!(tokens.calls().is_empty());
    assert!(transport.calls().is_empty());
    assert!(!format!("{err:?}").contains(SECRET_TOKEN));
}

#[tokio::test]
async fn invalid_limit_fails_before_token_or_transport_lookup() {
    let tokens = FakeTokenSource::new(SECRET_TOKEN);
    let transport = FakeTransport::new(issues_response());
    let client = LinearClient::new(config(TOKEN_KEY), tokens.clone(), transport.clone());

    let err = client
        .list_issues(LinearQuery {
            limit: 0,
            ..LinearQuery::default()
        })
        .await
        .expect_err("invalid issue limit");

    assert!(matches!(
        err,
        LinearError::InvalidLimit { limit: 0, max: 100 }
    ));
    assert!(tokens.calls().is_empty());
    assert!(transport.calls().is_empty());
    assert!(!format!("{err:?}").contains(SECRET_TOKEN));
}

#[tokio::test]
async fn token_lookup_timeout_is_enforced() {
    let transport = FakeTransport::new(issues_response());
    let client = LinearClient::new(
        LinearConfig {
            token_key: TOKEN_KEY.to_owned(),
            endpoint: "https://linear.example/graphql".to_owned(),
            token_lookup_timeout: Duration::from_millis(1),
        },
        HangingTokenSource,
        transport,
    );

    let err = client
        .list_issues(LinearQuery::default())
        .await
        .expect_err("token lookup timeout");

    assert!(matches!(
        err,
        LinearError::TokenLookupTimedOut {
            ref token_key,
            timeout_ms: 1,
            ..
        } if token_key == TOKEN_KEY
    ));
    let message = err.to_string();
    assert!(message.contains(TOKEN_KEY));
    assert!(!message.contains(SECRET_TOKEN));
}

#[tokio::test]
async fn zero_token_lookup_timeout_is_rejected() {
    let tokens = FakeTokenSource::new(SECRET_TOKEN);
    let transport = FakeTransport::new(issues_response());
    let client = LinearClient::new(
        LinearConfig {
            token_key: TOKEN_KEY.to_owned(),
            endpoint: "https://linear.example/graphql".to_owned(),
            token_lookup_timeout: Duration::ZERO,
        },
        tokens.clone(),
        transport.clone(),
    );

    let err = client
        .list_issues(LinearQuery::default())
        .await
        .expect_err("invalid token timeout");

    assert!(matches!(err, LinearError::InvalidTokenLookupTimeout));
    assert!(tokens.calls().is_empty());
    assert!(transport.calls().is_empty());
}

#[tokio::test]
async fn token_lookup_failure_is_typed_and_skips_transport() {
    let tokens = FakeTokenSource::failing("keyring unavailable");
    let transport = FakeTransport::new(issues_response());
    let client = LinearClient::new(config(TOKEN_KEY), tokens.clone(), transport.clone());

    let err = client
        .list_issues(LinearQuery::default())
        .await
        .expect_err("token lookup failure");

    assert!(matches!(
        err,
        LinearError::TokenLookup {
            ref token_key,
            ..
        } if token_key == TOKEN_KEY
    ));
    assert_eq!(tokens.calls(), vec![TOKEN_KEY]);
    assert!(transport.calls().is_empty());
    assert!(!format!("{err:?}").contains(SECRET_TOKEN));
}

#[tokio::test]
async fn transport_failure_is_typed_without_token_leak() {
    let transport = FakeTransport::failing("network unavailable");
    let client = LinearClient::new(
        config(TOKEN_KEY),
        FakeTokenSource::new(SECRET_TOKEN),
        transport,
    );

    let err = client
        .list_issues(LinearQuery::default())
        .await
        .expect_err("transport failure");

    assert!(matches!(err, LinearError::Transport { .. }));
    assert!(!err.to_string().contains(SECRET_TOKEN));
    assert!(!format!("{err:?}").contains(SECRET_TOKEN));
}

#[tokio::test]
async fn graphql_response_errors_are_typed_without_token_leak() {
    let transport = FakeTransport::new(json!({
        "errors": [
            { "message": "Cannot query field assignedIssues" }
        ]
    }));
    let client = LinearClient::new(
        config(TOKEN_KEY),
        FakeTokenSource::new(SECRET_TOKEN),
        transport,
    );

    let err = client
        .list_issues(LinearQuery::default())
        .await
        .expect_err("GraphQL errors");

    assert!(matches!(
        err,
        LinearError::GraphqlErrors { ref messages }
            if messages == &vec!["Cannot query field assignedIssues".to_owned()]
    ));
    assert!(!err.to_string().contains(SECRET_TOKEN));
    assert!(!format!("{err:?}").contains(SECRET_TOKEN));
}

#[tokio::test]
async fn invalid_response_shape_is_typed() {
    let transport = FakeTransport::new(json!({
        "data": {
            "issues": {}
        }
    }));
    let client = LinearClient::new(
        config(TOKEN_KEY),
        FakeTokenSource::new(SECRET_TOKEN),
        transport,
    );

    let err = client
        .list_issues(LinearQuery::default())
        .await
        .expect_err("invalid response shape");

    assert!(matches!(
        err,
        LinearError::InvalidResponse { path }
            if path == "data.issues.nodes"
    ));
}

#[tokio::test]
async fn missing_required_issue_field_is_typed() {
    let transport = FakeTransport::new(json!({
        "data": {
            "issues": {
                "nodes": [
                    {
                        "id": "opaque-linear-id",
                        "identifier": "LIN-123",
                        "description": "Issue body",
                        "branchName": "lin-123-fix-launcher",
                        "url": "https://linear.example/LIN-123"
                    }
                ]
            }
        }
    }));
    let client = LinearClient::new(
        config(TOKEN_KEY),
        FakeTokenSource::new(SECRET_TOKEN),
        transport,
    );

    let err = client
        .list_issues(LinearQuery::default())
        .await
        .expect_err("missing title");

    assert!(matches!(
        err,
        LinearError::MissingIssueField {
            index: 0,
            field: "title"
        }
    ));
}

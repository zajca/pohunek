//! Linear provider client for the native GUI.
//!
//! The client keeps secrets outside persistent state by reading the configured
//! token reference through [`TokenSource`] immediately before each GraphQL call.

// Rust guideline compliant 2026-06-26

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;

const DEFAULT_ISSUE_LIMIT: usize = 50;
const MAX_ISSUE_LIMIT: usize = 100;
const DEFAULT_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const ASSIGNED_ISSUES_NODES_PATH: &str = "data.viewer.assignedIssues.nodes";
const ASSIGNED_ISSUES_QUERY: &str = r"
query PohunekAssignedIssues($first: Int!, $filter: IssueFilter) {
  viewer {
    assignedIssues(first: $first, filter: $filter) {
      nodes {
        id
        identifier
        title
        description
        branchName
        url
      }
    }
  }
}
";

/// Future returned by a token source.
pub type TokenFuture<'a> = Pin<Box<dyn Future<Output = Result<String, TokenError>> + Send + 'a>>;

/// Future returned by a GraphQL transport.
pub type TransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, GraphqlTransportError>> + Send + 'a>>;

/// Configuration for Linear GraphQL calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearConfig {
    /// Keyring entry name that stores the Linear API token.
    pub token_key: String,
    /// Linear GraphQL endpoint URL.
    pub endpoint: String,
    /// Maximum time to wait for the keyring token lookup.
    pub token_lookup_timeout: Duration,
}

/// Query parameters for the assigned-to-me Linear issue view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearQuery {
    /// Optional workflow state name filter.
    pub state: Option<String>,
    /// Optional fulltext search term.
    pub search: Option<String>,
    /// Maximum number of issues to request.
    pub limit: usize,
}

impl Default for LinearQuery {
    fn default() -> Self {
        Self {
            state: None,
            search: None,
            limit: DEFAULT_ISSUE_LIMIT,
        }
    }
}

/// Linear issue fields used by prompt rendering and launch flows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearIssue {
    /// Linear's opaque issue id.
    pub id: String,
    /// Human-readable issue identifier, such as `ENG-123`.
    pub identifier: String,
    /// Issue title.
    pub title: String,
    /// Issue description body.
    pub body: String,
    /// Branch name suggested by Linear.
    pub branch: String,
    /// Browser URL for the issue.
    pub url: String,
}

impl LinearIssue {
    /// Returns the id used by shared prompt rendering.
    #[must_use]
    pub fn prompt_item_id(&self) -> &str {
        &self.identifier
    }

    /// Converts this issue to the shared prompt renderer JSON shape.
    #[must_use]
    pub fn to_prompt_json(&self) -> Value {
        json!({
            "id": self.id,
            "identifier": self.identifier,
            "title": self.title,
            "description": self.body,
            "body": self.body,
            "branchName": self.branch,
            "branch": self.branch,
            "url": self.url,
        })
    }
}

/// Source for Linear API tokens.
pub trait TokenSource: Send + Sync {
    /// Reads a token for `token_key`.
    ///
    /// Implementations must not include token values in errors or debug output.
    ///
    /// # Errors
    ///
    /// Returns an error when the token reference cannot be resolved.
    fn token<'a>(&'a self, token_key: &'a str) -> TokenFuture<'a>;
}

/// Transport used to post Linear GraphQL requests.
pub trait GraphqlTransport: Send + Sync {
    /// Posts a GraphQL request body to `endpoint` with the supplied token.
    ///
    /// Implementations must keep the token out of errors and debug output.
    fn post_graphql<'a>(
        &'a self,
        endpoint: &'a str,
        token: &'a str,
        body: Value,
    ) -> TransportFuture<'a>;
}

/// Token lookup error with redacted details.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct TokenError {
    message: String,
}

impl TokenError {
    /// Creates a token lookup error.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// GraphQL transport error with redacted details.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct GraphqlTransportError {
    message: String,
}

impl GraphqlTransportError {
    /// Creates a transport error.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Errors raised by the Linear provider client.
#[derive(Debug, Error)]
pub enum LinearError {
    /// The Linear config omitted the token key.
    #[error("missing Linear token key")]
    MissingTokenKey,
    /// The Linear config omitted the GraphQL endpoint.
    #[error("missing Linear GraphQL endpoint")]
    MissingEndpoint,
    /// The query limit is outside the supported Linear issue page size.
    #[error("invalid Linear issue limit {limit}; expected 1..={max}")]
    InvalidLimit {
        /// Requested issue limit.
        limit: usize,
        /// Maximum supported issue limit.
        max: usize,
    },
    /// The token lookup timeout is missing or invalid.
    #[error("invalid Linear token lookup timeout; expected a positive duration")]
    InvalidTokenLookupTimeout,
    /// The token reference lookup exceeded the configured timeout.
    #[error("timed out looking up Linear token `{token_key}` after {timeout_ms} ms")]
    TokenLookupTimedOut {
        /// Keyring token reference, not a token value.
        token_key: String,
        /// Configured timeout in milliseconds.
        timeout_ms: u128,
        /// Underlying Tokio timeout error.
        source: tokio::time::error::Elapsed,
    },
    /// The token reference could not be resolved.
    #[error("failed to look up Linear token `{token_key}`: {source}")]
    TokenLookup {
        /// Keyring token reference, not a token value.
        token_key: String,
        /// Underlying token lookup failure.
        source: TokenError,
    },
    /// The GraphQL transport failed.
    #[error("Linear GraphQL transport failed: {source}")]
    Transport {
        /// Underlying transport failure.
        source: GraphqlTransportError,
    },
    /// Linear returned GraphQL errors.
    #[error("Linear GraphQL response returned error(s): {messages:?}")]
    GraphqlErrors {
        /// Error messages returned by the GraphQL response.
        messages: Vec<String>,
    },
    /// Linear returned JSON with an unexpected shape.
    #[error("Linear GraphQL response has invalid shape at `{path}`")]
    InvalidResponse {
        /// Dot path that did not match the expected response shape.
        path: &'static str,
    },
    /// A returned issue missed a required field.
    #[error("Linear issue #{index} missing required field `{field}`")]
    MissingIssueField {
        /// Issue index in the response.
        index: usize,
        /// Missing field name.
        field: &'static str,
    },
}

/// Linear provider client.
pub struct LinearClient<T, H> {
    config: LinearConfig,
    token_source: T,
    transport: H,
}

impl<T, H> std::fmt::Debug for LinearClient<T, H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearClient")
            .field("config", &self.config)
            .field("token_source", &"<redacted>")
            .field("transport", &"<transport>")
            .finish()
    }
}

impl<T, H> Clone for LinearClient<T, H>
where
    T: Clone,
    H: Clone,
{
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            token_source: self.token_source.clone(),
            transport: self.transport.clone(),
        }
    }
}

impl<T, H> LinearClient<T, H> {
    /// Creates a Linear client from explicit dependencies.
    #[must_use]
    pub fn new(config: LinearConfig, token_source: T, transport: H) -> Self {
        Self {
            config,
            token_source,
            transport,
        }
    }

    /// Returns this client's configuration.
    #[must_use]
    pub fn config(&self) -> &LinearConfig {
        &self.config
    }
}

impl<T, H> LinearClient<T, H>
where
    T: TokenSource,
    H: GraphqlTransport,
{
    /// Fetches assigned-to-me Linear issues.
    ///
    /// The Linear token is read through [`TokenSource`] for every call, so token
    /// rotation does not require rebuilding the client.
    ///
    /// # Errors
    ///
    /// Returns a typed [`LinearError`] for token, transport, GraphQL, and
    /// response-shape failures.
    pub async fn assigned_issues(
        &self,
        query: LinearQuery,
    ) -> Result<Vec<LinearIssue>, LinearError> {
        if self.config.token_key.trim().is_empty() {
            return Err(LinearError::MissingTokenKey);
        }
        if self.config.endpoint.trim().is_empty() {
            return Err(LinearError::MissingEndpoint);
        }
        if query.limit == 0 || query.limit > MAX_ISSUE_LIMIT {
            return Err(LinearError::InvalidLimit {
                limit: query.limit,
                max: MAX_ISSUE_LIMIT,
            });
        }
        if self.config.token_lookup_timeout.is_zero() {
            return Err(LinearError::InvalidTokenLookupTimeout);
        }

        let token = tokio::time::timeout(
            self.config.token_lookup_timeout,
            self.token_source.token(&self.config.token_key),
        )
        .await
        .map_err(|source| LinearError::TokenLookupTimedOut {
            token_key: self.config.token_key.clone(),
            timeout_ms: self.config.token_lookup_timeout.as_millis(),
            source,
        })?
        .map_err(|source| LinearError::TokenLookup {
            token_key: self.config.token_key.clone(),
            source,
        })?;
        let body = assigned_issues_body(&query);
        let response = self
            .transport
            .post_graphql(&self.config.endpoint, &token, body)
            .await
            .map_err(|source| LinearError::Transport { source })?;

        parse_assigned_issues(&response)
    }
}

/// Keyring-backed Linear token source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyringTokenSource {
    service: String,
}

impl KeyringTokenSource {
    /// Creates a keyring token source for a service namespace.
    #[must_use]
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// Returns the keyring service namespace.
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }
}

impl TokenSource for KeyringTokenSource {
    fn token<'a>(&'a self, token_key: &'a str) -> TokenFuture<'a> {
        let service = self.service.clone();
        let token_key = token_key.to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let entry = keyring::Entry::new(&service, &token_key).map_err(|source| {
                    TokenError::new(format!("failed to open keyring entry: {source}"))
                })?;
                entry.get_password().map_err(|source| {
                    TokenError::new(format!("failed to read keyring entry: {source}"))
                })
            })
            .await
            .map_err(|source| TokenError::new(format!("keyring lookup task failed: {source}")))?
        })
    }
}

/// HTTP timing options for Linear GraphQL calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpGraphqlTransportOptions {
    /// Maximum time to establish the HTTP connection.
    pub connect_timeout: Duration,
    /// Maximum time for the full HTTP request.
    pub request_timeout: Duration,
}

impl Default for HttpGraphqlTransportOptions {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_HTTP_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_HTTP_REQUEST_TIMEOUT,
        }
    }
}

/// Reqwest-backed Linear GraphQL transport.
#[derive(Debug, Clone)]
pub struct HttpGraphqlTransport {
    client: reqwest::Client,
}

impl HttpGraphqlTransport {
    /// Creates a transport with default HTTP timing options.
    ///
    /// # Errors
    ///
    /// Returns an error if reqwest cannot build the HTTP client.
    pub fn try_new() -> Result<Self, GraphqlTransportError> {
        Self::with_options(HttpGraphqlTransportOptions::default())
    }

    /// Creates a transport with explicit HTTP timing options.
    ///
    /// # Errors
    ///
    /// Returns an error if reqwest cannot build the HTTP client.
    pub fn with_options(
        options: HttpGraphqlTransportOptions,
    ) -> Result<Self, GraphqlTransportError> {
        reqwest::Client::builder()
            .connect_timeout(options.connect_timeout)
            .timeout(options.request_timeout)
            .build()
            .map(Self::with_client)
            .map_err(|source| {
                GraphqlTransportError::new(format!("failed to build HTTP client: {source}"))
            })
    }

    /// Creates a transport from a preconfigured HTTP client.
    #[must_use]
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl GraphqlTransport for HttpGraphqlTransport {
    fn post_graphql<'a>(
        &'a self,
        endpoint: &'a str,
        token: &'a str,
        body: Value,
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            let authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|source| {
                    GraphqlTransportError::new(format!(
                        "failed to build Linear GraphQL authorization header: {source}"
                    ))
                })?;
            let response = self
                .client
                .post(endpoint)
                .header(reqwest::header::AUTHORIZATION, authorization)
                .json(&body)
                .send()
                .await
                .map_err(|source| {
                    GraphqlTransportError::new(format!("request failed: {source}"))
                })?;

            let status = response.status();
            if !status.is_success() {
                return Err(GraphqlTransportError::new(format!(
                    "Linear GraphQL HTTP request failed with status {status}"
                )));
            }

            response.json::<Value>().await.map_err(|source| {
                GraphqlTransportError::new(format!(
                    "failed to decode Linear GraphQL JSON response: {source}"
                ))
            })
        })
    }
}

fn assigned_issues_body(query: &LinearQuery) -> Value {
    let mut filter = Map::new();
    if let Some(state) = query.state.as_deref().and_then(non_empty) {
        filter.insert("state".to_owned(), json!({ "name": { "eq": state } }));
    }
    if let Some(search) = query.search.as_deref().and_then(non_empty) {
        filter.insert(
            "searchableContent".to_owned(),
            json!({ "containsIgnoreCase": search }),
        );
    }

    json!({
        "query": ASSIGNED_ISSUES_QUERY,
        "variables": {
            "first": query.limit,
            "filter": if filter.is_empty() {
                Value::Null
            } else {
                Value::Object(filter)
            },
        },
    })
}

fn parse_assigned_issues(response: &Value) -> Result<Vec<LinearIssue>, LinearError> {
    if let Some(errors) = graphql_error_messages(response) {
        return Err(LinearError::GraphqlErrors { messages: errors });
    }

    let nodes = response
        .get("data")
        .and_then(|value| value.get("viewer"))
        .and_then(|value| value.get("assignedIssues"))
        .and_then(|value| value.get("nodes"))
        .and_then(Value::as_array)
        .ok_or(LinearError::InvalidResponse {
            path: ASSIGNED_ISSUES_NODES_PATH,
        })?;

    nodes
        .iter()
        .enumerate()
        .map(|(index, value)| issue_from_value(index, value))
        .collect()
}

fn issue_from_value(index: usize, value: &Value) -> Result<LinearIssue, LinearError> {
    Ok(LinearIssue {
        id: required_str(index, value, "id")?,
        identifier: required_str(index, value, "identifier")?,
        title: required_str(index, value, "title")?,
        body: optional_str(value, &["description", "body"]),
        branch: required_any_str(index, value, &["branchName", "branch"])?,
        url: required_str(index, value, "url")?,
    })
}

fn graphql_error_messages(response: &Value) -> Option<Vec<String>> {
    let errors = response.get("errors")?.as_array()?;
    if errors.is_empty() {
        return None;
    }

    Some(
        errors
            .iter()
            .map(|error| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Linear returned an unparseable GraphQL error")
                    .to_owned()
            })
            .collect(),
    )
}

fn required_str(index: usize, value: &Value, field: &'static str) -> Result<String, LinearError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or(LinearError::MissingIssueField { index, field })
}

fn required_any_str(
    index: usize,
    value: &Value,
    fields: &'static [&'static str],
) -> Result<String, LinearError> {
    fields
        .iter()
        .find_map(|field| {
            value
                .get(*field)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .ok_or(LinearError::MissingIssueField {
            index,
            field: joined_field_name(fields),
        })
}

fn optional_str(value: &Value, fields: &[&str]) -> String {
    fields
        .iter()
        .find_map(|field| {
            value
                .get(*field)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_default()
}

fn joined_field_name(fields: &'static [&'static str]) -> &'static str {
    match fields {
        ["branchName", "branch"] => "branchName/branch",
        _ => "unknown",
    }
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

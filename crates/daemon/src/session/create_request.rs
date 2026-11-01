//! Idempotency keys of `session.new`.
//!
//! A `session.new` that carries a `client_request_id` is fingerprinted, and the
//! key is claimed for the whole registry before any host-dependent work, so two
//! requests that share a key cannot both create. The key and fingerprint then
//! live on the created session's record ([`CreateRequest`]) and die with it.
//! A repeated key answers from the registry's own sessions: the same
//! fingerprint replays the session, another one conflicts, and a create that
//! has not settled is reported as in progress.

// Rust guideline compliant 2026-11-01

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use protocol::{BranchSource, OnDiverged, ProtocolError, SessionId, SessionInfo, SessionNewParams};
use serde::Serialize;
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tracing::warn;

use super::supervision::CREATE_COMPENSATION_PENDING;
use super::{runtime_error, SessionEntry, SessionRegistry};
use crate::store::{CreateRequest, TransactionKind};

/// Version prefix of a request fingerprint. A change to what the fingerprint
/// covers is a new version, because the fingerprints already stored must keep
/// comparing equal to the same request.
const FINGERPRINT_VERSION: &str = "v1";

/// The request fields a fingerprint covers, in their fixed serialization order.
///
/// It leaves out the key itself, the terminal size (a caller resized between
/// two attempts sends the same request) and every `secret_env` value (a digest
/// of a credential in the store would break "values are never persisted"); only
/// the sorted slot names are covered. Every field is absent when unset, so a
/// field added to [`SessionNewParams`] later must stay absent when unset or
/// bump [`FINGERPRINT_VERSION`]. Values are raw: the daemon trims a name only
/// after the fingerprint is taken.
#[derive(Serialize)]
struct FingerprintView<'a> {
    agent: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_branch: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_source: Option<BranchSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_diverged: Option<OnDiverged>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extended_input_ready_wait: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_ack_timeout_ms: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    secret_env_names: Vec<&'a str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    metadata: &'a BTreeMap<String, String>,
}

/// The fingerprint of `params`, or `None` for a request without a key: the
/// version and the SHA-256 hex digest of the serialized [`FingerprintView`].
///
/// # Errors
///
/// Returns `bad_request` when a field cannot be serialized (a path that is not
/// UTF-8, which no JSON request carries).
pub(super) fn request_fingerprint(
    params: &SessionNewParams,
) -> Result<Option<String>, ProtocolError> {
    if params.client_request_id.is_none() {
        return Ok(None);
    }
    // Exhaustive on purpose: a new `SessionNewParams` field fails to compile
    // here until it joins the fingerprint or is named as excluded.
    let SessionNewParams {
        agent,
        name,
        cwd,
        cols: _cols,
        rows: _rows,
        project,
        repo,
        branch,
        base_branch,
        branch_source,
        on_diverged,
        input,
        extended_input_ready_wait,
        input_ack_timeout_ms,
        secret_env,
        metadata,
        client_request_id: _client_request_id,
    } = params;
    let mut secret_env_names: Vec<&str> = secret_env.names().collect();
    secret_env_names.sort_unstable();
    let view = FingerprintView {
        agent,
        name: name.as_deref(),
        cwd: cwd.as_deref(),
        project: project.as_deref(),
        repo: repo.as_deref(),
        branch: branch.as_deref(),
        base_branch: base_branch.as_deref(),
        branch_source: *branch_source,
        on_diverged: *on_diverged,
        input: input.as_deref(),
        extended_input_ready_wait: *extended_input_ready_wait,
        input_ack_timeout_ms: *input_ack_timeout_ms,
        secret_env_names,
        metadata,
    };
    let bytes = serde_json::to_vec(&view).map_err(|_error| {
        ProtocolError::bad_request("session.new: the parameters cannot be fingerprinted")
    })?;
    Ok(Some(format!(
        "{FINGERPRINT_VERSION}:{:x}",
        Sha256::digest(bytes)
    )))
}

/// The keys whose create is running, with the session each one allocated.
pub(super) type CreateClaims = std::sync::Mutex<HashMap<String, ClaimedCreate>>;

/// One claimed key.
#[derive(Debug)]
pub(super) struct ClaimedCreate {
    session_id: SessionId,
    fingerprint: String,
}

/// A claimed key; dropping it releases the claim.
///
/// The create task owns it until the create has either committed its record or
/// removed what it made, so a request that finds no claim finds the session or
/// nothing.
#[derive(Debug)]
pub(super) struct CreateClaim {
    registry: SessionRegistry,
    key: String,
}

impl Drop for CreateClaim {
    fn drop(&mut self) {
        self.registry
            .inner
            .create_claims
            .lock()
            .expect("create claims are never poisoned")
            .remove(&self.key);
    }
}

/// What a keyed `session.new` does next.
#[derive(Debug)]
pub(super) enum KeyedCreate {
    /// The request carries no key.
    Unkeyed,
    /// Create the session under the claim.
    Fresh {
        claim: CreateClaim,
        request: Box<CreateRequest>,
    },
    /// The key names a committed session with the same fingerprint.
    Replay(Box<SessionInfo>),
}

impl SessionRegistry {
    /// Claims `key` for a create of `id`, or answers it from the session that
    /// already holds it; a request without a key claims nothing.
    ///
    /// The claims and the registry's sessions are read under one hold of the
    /// sessions lock, and a create commits its entry before it drops its
    /// claim, so a key is claimed, held by an entry, or free.
    ///
    /// # Errors
    ///
    /// Returns `session_request_in_progress` while the create has not settled
    /// and `session_request_conflict` when the key names a session created
    /// with another fingerprint.
    pub(super) async fn claim_create_request(
        &self,
        key: Option<&str>,
        fingerprint: Option<String>,
        id: &SessionId,
    ) -> Result<KeyedCreate, ProtocolError> {
        let Some((key, fingerprint)) = key.zip(fingerprint) else {
            return Ok(KeyedCreate::Unkeyed);
        };
        let sessions = self.inner.sessions.lock().await;
        let decision = self.decide_create_request(&sessions, key, fingerprint, id)?;
        drop(sessions);
        let session = match decision {
            KeyedCreate::Replay(session) => session,
            KeyedCreate::Fresh { claim, request } => {
                // A record that holds the key without a live entry (a
                // compensation whose record could not be read back, a removal
                // that has not deleted its record yet) still owns it.
                if let Some(error) = self.durable_holder_error(&request).await? {
                    drop(claim);
                    return Err(error);
                }
                return Ok(KeyedCreate::Fresh { claim, request });
            }
            KeyedCreate::Unkeyed => return Ok(KeyedCreate::Unkeyed),
        };
        // A record that still carries its create transaction (an initial input
        // not delivered, a create transaction that reconciliation left pending, a compensation that
        // has not finished) is not a settled session.
        let unsettled = self
            .load_durable_session_record(&session.id)
            .await?
            .and_then(|record| record.transaction)
            .is_some_and(|transaction| transaction.kind == TransactionKind::Create);
        if unsettled {
            return Err(ProtocolError::session_request_in_progress(&session.id));
        }
        let mut info = [*session];
        self.enrich_project_labels(&mut info).await;
        let [info] = info;
        Ok(KeyedCreate::Replay(Box::new(info)))
    }

    /// The error for the durable record that already holds the key of
    /// `request`, if one does: `session_request_conflict` when it was created
    /// with another fingerprint or the key is duplicated, otherwise
    /// `session_request_in_progress`, because a record without a live entry has
    /// not settled.
    async fn durable_holder_error(
        &self,
        request: &CreateRequest,
    ) -> Result<Option<ProtocolError>, ProtocolError> {
        let Some(store) = self.inner.store.clone() else {
            return Ok(None);
        };
        let records = tokio::task::spawn_blocking(move || store.load_sessions())
            .await
            .map_err(|_join_error| {
                runtime_error(
                    "session_store_failed",
                    "session record read task panicked while checking a client_request_id",
                )
            })?
            .map_err(|error| {
                runtime_error(
                    "session_store_failed",
                    format!("failed to read the session records for a client_request_id: {error}"),
                )
            })?;
        let mut holders: Vec<_> = records
            .iter()
            .filter(|record| {
                record
                    .create_request
                    .as_deref()
                    .is_some_and(|held| held.id == request.id)
            })
            .collect();
        holders.sort_by_key(|record| created_order(&record.info.created_at));
        let Some(oldest) = holders.first() else {
            return Ok(None);
        };
        if holders.len() > 1 {
            warn!(
                name: "session.request.duplicate_key",
                records = holders.len(),
                "several session records hold one client_request_id; the key answers for the oldest"
            );
        }
        let session_id = SessionId(oldest.session_id.clone());
        let same = holders.len() == 1
            && oldest
                .create_request
                .as_deref()
                .is_some_and(|held| held.fingerprint == request.fingerprint);
        Ok(Some(if same {
            ProtocolError::session_request_in_progress(&session_id)
        } else {
            ProtocolError::session_request_conflict(&session_id)
        }))
    }

    /// The synchronous part of [`Self::claim_create_request`], run under the
    /// sessions lock so the claims and the sessions are read together.
    fn decide_create_request(
        &self,
        sessions: &HashMap<SessionId, SessionEntry>,
        key: &str,
        fingerprint: String,
        id: &SessionId,
    ) -> Result<KeyedCreate, ProtocolError> {
        let mut claims = self
            .inner
            .create_claims
            .lock()
            .expect("create claims are never poisoned");
        if let Some(claimed) = claims.get(key) {
            return Err(if claimed.fingerprint == fingerprint {
                ProtocolError::session_request_in_progress(&claimed.session_id)
            } else {
                ProtocolError::session_request_conflict(&claimed.session_id)
            });
        }
        let mut holders: Vec<&SessionEntry> = sessions
            .values()
            .filter(|entry| {
                entry
                    .create_request
                    .as_ref()
                    .is_some_and(|request| request.id == key)
            })
            .collect();
        holders.sort_by_key(|entry| created_order(&entry.info.created_at));
        if let Some(oldest) = holders.first() {
            if holders.len() > 1 {
                warn!(
                    name: "session.request.duplicate_key",
                    sessions = holders.len(),
                    "several sessions hold one client_request_id; the key answers for the oldest"
                );
                return Err(ProtocolError::session_request_conflict(&oldest.info.id));
            }
            let held = oldest
                .create_request
                .as_ref()
                .expect("a holder carries the key");
            if held.fingerprint != fingerprint {
                return Err(ProtocolError::session_request_conflict(&oldest.info.id));
            }
            if oldest.create_unsettled() {
                return Err(ProtocolError::session_request_in_progress(&oldest.info.id));
            }
            return Ok(KeyedCreate::Replay(Box::new(oldest.info.clone())));
        }
        claims.insert(
            key.to_owned(),
            ClaimedCreate {
                session_id: id.clone(),
                fingerprint: fingerprint.clone(),
            },
        );
        Ok(KeyedCreate::Fresh {
            claim: CreateClaim {
                registry: self.clone(),
                key: key.to_owned(),
            },
            request: Box::new(CreateRequest {
                id: key.to_owned(),
                fingerprint,
            }),
        })
    }
}

/// Sort key that orders sessions by creation time; the RFC 3339 text alone does
/// not order two instants of one second, because it trims trailing zeros of the
/// fraction.
fn created_order(created_at: &str) -> (Option<OffsetDateTime>, &str) {
    (OffsetDateTime::parse(created_at, &Rfc3339).ok(), created_at)
}

impl SessionEntry {
    /// Whether the entry shows an unsettled create: its initial input is still
    /// undelivered, or its compensation could not finish. The durable record
    /// can show more (a create transaction that reconciliation left pending).
    pub(super) fn create_unsettled(&self) -> bool {
        self.initial_input_owner.is_some()
            || self.info.runtime.as_ref().is_some_and(|runtime| {
                runtime.loss_reason.as_deref() == Some(CREATE_COMPENSATION_PENDING)
            })
    }
}

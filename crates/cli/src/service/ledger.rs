//! The accepted-risk ledger, `<state>/pohunek/service-accepted-risks.json`.
//!
//! An upgrade that completes replaces the ledger with the scoped at-risk
//! verdicts of its final adoption gate. A later gate treats a verdict as
//! previously accepted only when the ledger, still bound to the installed
//! version, namespace and metadata store, holds an entry that matches the
//! verdict's session, list, verdict, code and daemon-computed acceptance scope
//! exactly. Everything else is unaccepted, so a missing, stale or unreadable
//! ledger asks for consent again and never grants it.

// Rust guideline compliant 2026-10-19

use std::path::{Path, PathBuf};

use pohunek_service_config::preflight::{SessionVerdict, Verdict, ACCEPTANCE_SCOPE_V1};
use serde::{Deserialize, Serialize};

use super::error::Error;

/// File name of the ledger inside the application state directory.
///
/// It is not one of the files a record clear removes: it outlives the
/// transaction that wrote it.
pub const FILE_NAME: &str = "service-accepted-risks.json";

/// Version of the ledger schema; a ledger of another version is ignored with
/// a warning.
pub const SCHEMA_VERSION: u32 = 1;

/// The list of the preflight report a verdict belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum List {
    /// A session that may own a live runtime.
    Live,
    /// A session whose runtime has ended.
    Stopped,
}

impl List {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Stopped => "stopped",
        }
    }
}

/// One accepted at-risk verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Logical session id.
    pub session_id: String,
    /// The report list the verdict was in.
    pub list: List,
    /// What would happen to the session.
    pub verdict: Verdict,
    /// Stable machine reason.
    pub code: String,
    /// The daemon-computed acceptance scope.
    pub scope: String,
}

impl Entry {
    /// The entry of `verdict` in `list`, when it carries a supported scope.
    ///
    /// A scope of another scheme or with a malformed digest cannot be
    /// compared with confidence, so it yields no entry and the acceptance
    /// stays one-time.
    #[must_use]
    pub fn of(list: List, verdict: &SessionVerdict) -> Option<Self> {
        let scope = supported_scope(verdict)?;
        Some(Self {
            session_id: verdict.session_id.clone(),
            list,
            verdict: verdict.verdict,
            code: verdict.code.clone(),
            scope: scope.to_owned(),
        })
    }

    fn matches(&self, list: List, verdict: &SessionVerdict) -> bool {
        self.list == list
            && self.session_id == verdict.session_id
            && self.verdict == verdict.verdict
            && self.code == verdict.code
            && supported_scope(verdict) == Some(self.scope.as_str())
    }
}

/// The scope of `verdict` when it is a `v1` scope: the scheme, a colon and a
/// lowercase hex SHA-256.
fn supported_scope(verdict: &SessionVerdict) -> Option<&str> {
    let scope = verdict.acceptance_scope.as_deref()?;
    let digest = scope.strip_prefix(ACCEPTANCE_SCOPE_V1)?.strip_prefix(':')?;
    (digest.len() == SCOPE_DIGEST_HEX_LEN
        && digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
    .then_some(scope)
}

/// Hex characters of a SHA-256 digest.
const SCOPE_DIGEST_HEX_LEN: usize = 64;

/// A verdict accepted by the flag that no scope can bind, kept for reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OneTime {
    /// Logical session id.
    pub session_id: String,
    /// Stable machine reason.
    pub code: String,
}

/// The at-risk verdicts the last passing gate of a transaction accepted.
///
/// The transaction record journals it, and the ledger is written from it when
/// the transaction completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedSet {
    /// The metadata store the judged report named.
    pub store_path: PathBuf,
    /// When the gate passed, RFC 3339; a resumed write repeats it.
    pub recorded_at: String,
    /// Scoped verdicts, whether the flag accepted them or the previous ledger
    /// already covered them.
    pub entries: Vec<Entry>,
    /// Flag-accepted verdicts without a scope.
    pub one_time: Vec<OneTime>,
}

/// The ledger file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ledger {
    /// Always [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The service namespace the ledger belongs to.
    pub namespace: String,
    /// The metadata store the judged report named.
    pub store_path: PathBuf,
    /// The version the upgrade that wrote the ledger installed.
    pub applied_version: String,
    /// The version that upgrade replaced.
    pub previous_version: Option<String>,
    /// When the gate passed, RFC 3339.
    pub applied_at: String,
    /// The acceptance-scope scheme of the entries.
    pub scope_scheme: String,
    /// The accepted verdicts.
    pub entries: Vec<Entry>,
}

impl Ledger {
    /// The ledger an upgrade to `applied_version` writes from `set`.
    #[must_use]
    pub fn of(
        namespace: &str,
        applied_version: &str,
        previous_version: Option<&str>,
        set: &AcceptedSet,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            namespace: namespace.to_owned(),
            store_path: set.store_path.clone(),
            applied_version: applied_version.to_owned(),
            previous_version: previous_version.map(ToOwned::to_owned),
            applied_at: set.recorded_at.clone(),
            scope_scheme: ACCEPTANCE_SCOPE_V1.to_owned(),
            entries: set.entries.clone(),
        }
    }
}

/// What a gate consults to recognize verdicts accepted by an earlier upgrade.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Acceptance {
    ledger: Option<Ledger>,
    warnings: Vec<String>,
}

impl Acceptance {
    /// Accepts nothing in advance; the rollback gate consults no ledger, because
    /// the previous release's daemon emits no scope.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Binds `ledger`, as read from `path`, to the installed `version` and
    /// `namespace`. A ledger that does not belong to them, or that this build
    /// cannot read, is ignored with a warning.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AcceptedRisks`] for a ledger file that is not valid.
    pub fn bind(
        read: Option<&[u8]>,
        path: &Path,
        namespace: &str,
        version: &str,
    ) -> Result<Self, Error> {
        let Some(bytes) = read else {
            return Ok(Self::none());
        };
        let invalid = |detail: String| Error::AcceptedRisks {
            path: path.to_path_buf(),
            detail,
        };
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|error| invalid(error.to_string()))?;
        let schema = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64);
        if schema != Some(u64::from(SCHEMA_VERSION)) {
            return Ok(Self::ignored(format!(
                "the accepted-risk ledger {} has schema_version {}, which this release does not read; \
                 it is ignored and the at-risk sessions need --accept-runtime-loss again",
                path.display(),
                value.get("schema_version").unwrap_or(&serde_json::Value::Null)
            )));
        }
        let ledger: Ledger =
            serde_json::from_value(value).map_err(|error| invalid(error.to_string()))?;
        let mismatch = if ledger.scope_scheme != ACCEPTANCE_SCOPE_V1 {
            Some(format!("scope scheme {}", ledger.scope_scheme))
        } else if ledger.namespace != namespace {
            Some(format!("namespace {}", ledger.namespace))
        } else if ledger.applied_version != version {
            Some(format!(
                "the upgrade to {}, while {version} is installed",
                ledger.applied_version
            ))
        } else {
            None
        };
        match mismatch {
            Some(reason) => Ok(Self::ignored(format!(
                "the accepted-risk ledger {} is for {reason}; it is ignored and the at-risk \
                 sessions need --accept-runtime-loss again",
                path.display()
            ))),
            None => Ok(Self {
                ledger: Some(ledger),
                warnings: Vec::new(),
            }),
        }
    }

    fn ignored(warning: String) -> Self {
        Self {
            ledger: None,
            warnings: vec![warning],
        }
    }

    /// The version the covering upgrade installed, when a ledger applies.
    #[must_use]
    pub fn applied_version(&self) -> Option<&str> {
        self.ledger
            .as_ref()
            .map(|ledger| ledger.applied_version.as_str())
    }

    /// Why the ledger was ignored, for the operator.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Whether the ledger holds `verdict` of `list`, judged against `store`.
    ///
    /// A ledger for another metadata store covers nothing; the caller reports
    /// that through [`Self::store_mismatch`].
    #[must_use]
    pub fn covers(&self, store: &Path, list: List, verdict: &SessionVerdict) -> bool {
        self.ledger.as_ref().is_some_and(|ledger| {
            ledger.store_path == store
                && ledger
                    .entries
                    .iter()
                    .any(|entry| entry.matches(list, verdict))
        })
    }

    /// The warning for a ledger that applies to the installed version but names
    /// another metadata store than the judged report.
    #[must_use]
    pub fn store_mismatch(&self, store: &Path) -> Option<String> {
        let ledger = self.ledger.as_ref()?;
        (ledger.store_path != store).then(|| {
            format!(
                "the accepted-risk ledger is for the metadata store {}, not {}; it is ignored and \
                 the at-risk sessions need --accept-runtime-loss again",
                ledger.store_path.display(),
                store.display()
            )
        })
    }
}

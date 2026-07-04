//! CLI-only host fan-out helpers.
//!
//! Cross-host notification aggregation is intentionally client-side. This module
//! owns the CLI's target expansion and per-host result shape so command modules
//! can fan out without importing GUI runtime types or inventing local variants.

use std::collections::BTreeMap;
use std::future::Future;

use futures::{stream, StreamExt as _};
use protocol::{method, HostClass, HostDiscoverParams, HostRecord, ProtocolError};
use serde::Serialize;

use crate::client::Client;
use crate::commands::request_with_params;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::LOCAL_HOST;

/// Maximum simultaneous daemon operations for finite fan-out commands.
///
/// Four hosts keeps a single CLI invocation responsive without stampeding the
/// operator's mesh with connect attempts. Each connection already has its own
/// SDK timeout, so this bound mainly limits aggregate pressure.
const FAN_OUT_CONCURRENCY: usize = 4;

/// Host fan-out mode selected by a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FanOutMode {
    /// Execute against exactly one transport target.
    One {
        /// Host name selected by the command's normal target resolution.
        host: String,
    },
    /// Execute against `local` plus reachable hosts discovered locally.
    AllHosts,
}

/// One daemon target selected for a fan-out operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HostTarget {
    /// Stable host identifier used for ordering and rendered output.
    pub(crate) host_id: String,
    /// Transport target passed to the SDK client.
    pub(crate) transport_target: String,
}

impl HostTarget {
    /// Create a host target from display id and transport target.
    #[must_use]
    pub(crate) fn new(host_id: impl Into<String>, transport_target: impl Into<String>) -> Self {
        Self {
            host_id: host_id.into(),
            transport_target: transport_target.into(),
        }
    }
}

/// Stable per-host result shape for fan-out output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HostResult<T> {
    /// Stable host identifier used for ordering and rendered output.
    pub(crate) host_id: String,
    /// Transport target passed to the SDK client.
    pub(crate) transport_target: String,
    /// Whether this host operation succeeded.
    pub(crate) ok: bool,
    /// Host-local success payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) value: Option<T>,
    /// Structured error for this host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<ProtocolError>,
}

impl<T> HostResult<T> {
    /// Build a successful host result.
    #[must_use]
    pub(crate) fn success(target: HostTarget, value: T) -> Self {
        Self {
            host_id: target.host_id,
            transport_target: target.transport_target,
            ok: true,
            value: Some(value),
            error: None,
        }
    }

    /// Build a failed host result.
    #[must_use]
    pub(crate) fn failure(target: HostTarget, error: ProtocolError) -> Self {
        Self {
            host_id: target.host_id,
            transport_target: target.transport_target,
            ok: false,
            value: None,
            error: Some(error),
        }
    }
}

/// Resolve fan-out targets for a command.
///
/// # Errors
///
/// Returns [`CliError`] when `--all-hosts` discovery cannot be loaded from the
/// local daemon or the daemon returns an unexpected payload.
pub(crate) async fn resolve_targets(
    paths: &Paths,
    mode: FanOutMode,
) -> Result<Vec<HostTarget>, CliError> {
    match mode {
        FanOutMode::One { .. } => Ok(targets_for_records(mode, &[])),
        FanOutMode::AllHosts => {
            let records = fetch_records(paths).await?;
            Ok(targets_for_records(FanOutMode::AllHosts, &records))
        }
    }
}

/// Build the single target used when fan-out is not selected.
#[must_use]
pub(crate) fn single_target(host: &str) -> HostTarget {
    targets_for_records(
        FanOutMode::One {
            host: host.to_owned(),
        },
        &[],
    )
    .into_iter()
    .next()
    .expect("one-target fan-out mode always returns one target")
}

/// Run one fallible operation per host with bounded concurrency.
pub(crate) async fn fan_out<T, Fut, Op>(targets: Vec<HostTarget>, op: Op) -> Vec<HostResult<T>>
where
    Op: Fn(HostTarget) -> Fut + Clone,
    Fut: Future<Output = Result<T, ProtocolError>>,
{
    let mut results = stream::iter(targets.into_iter().map(|target| {
        let op = op.clone();
        async move {
            let result = op(target.clone()).await;
            match result {
                Ok(value) => HostResult::success(target, value),
                Err(error) => HostResult::failure(target, error),
            }
        }
    }))
    .buffer_unordered(FAN_OUT_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    results.sort_by(|left, right| {
        left.host_id
            .cmp(&right.host_id)
            .then_with(|| left.transport_target.cmp(&right.transport_target))
    });
    results
}

fn targets_for_records(mode: FanOutMode, records: &[HostRecord]) -> Vec<HostTarget> {
    let mut targets = BTreeMap::new();
    match mode {
        FanOutMode::One { host } => {
            let target = normalize_host(&host);
            targets.insert(target.clone(), HostTarget::new(target.clone(), target));
        }
        FanOutMode::AllHosts => {
            targets.insert(
                LOCAL_HOST.to_owned(),
                HostTarget::new(LOCAL_HOST, LOCAL_HOST),
            );
            for record in records {
                if let Some(target) = reachable_target(record) {
                    targets.entry(target.host_id.clone()).or_insert(target);
                }
            }
        }
    }
    targets.into_values().collect()
}

async fn fetch_records(paths: &Paths) -> Result<Vec<HostRecord>, CliError> {
    let mut client = Client::connect(LOCAL_HOST, paths).await?;
    let request = request_with_params(method::HOST_DISCOVER, &HostDiscoverParams { force: false })?;
    let result = client.request(&request).await?;
    Ok(serde_json::from_value(result)?)
}

fn reachable_target(record: &HostRecord) -> Option<HostTarget> {
    match &record.class {
        HostClass::ReachableDaemon { .. } => record
            .name
            .as_deref()
            .map(|name| HostTarget::new(name, name)),
        HostClass::VersionMismatch { .. } | HostClass::Unreachable | HostClass::Candidate => None,
    }
}

fn normalize_host(host: &str) -> String {
    if host.is_empty() {
        LOCAL_HOST.to_owned()
    } else {
        host.to_owned()
    }
}

/// Convert a CLI error into a stable per-host protocol error.
#[must_use]
#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err adapter receives owned errors from Result and returns a detached protocol shape"
)]
pub(crate) fn error_details(error: CliError) -> ProtocolError {
    error.to_protocol_error()
}

#[cfg(test)]
mod tests {
    use protocol::ErrorClass;

    use super::*;

    fn record(name: &str, class: HostClass) -> HostRecord {
        HostRecord {
            name: Some(name.to_owned()),
            fqdn: Some(format!("{name}.example.net")),
            netbird_ip: Some("100.92.30.40".to_owned()),
            class,
        }
    }

    #[test]
    fn local_only_execution_resolves_one_target() {
        let targets = targets_for_records(
            FanOutMode::One {
                host: "local".to_owned(),
            },
            &[],
        );

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].host_id, "local");
        assert_eq!(targets[0].transport_target, "local");
    }

    #[test]
    fn all_hosts_resolves_local_and_reachable_discovered_hosts_in_host_id_order() {
        let records = vec![
            record("host-c", HostClass::Unreachable),
            record(
                "host-b",
                HostClass::ReachableDaemon {
                    daemon_version: "0.1.0".to_owned(),
                },
            ),
            record(
                "host-a",
                HostClass::ReachableDaemon {
                    daemon_version: "0.1.0".to_owned(),
                },
            ),
        ];

        let targets = targets_for_records(FanOutMode::AllHosts, &records);

        let ids: Vec<_> = targets
            .iter()
            .map(|target| target.host_id.as_str())
            .collect();
        assert_eq!(ids, ["host-a", "host-b", "local"]);
        assert!(targets
            .iter()
            .all(|target| target.host_id == target.transport_target));
    }

    #[tokio::test]
    async fn fan_out_preserves_success_when_another_host_fails() {
        let targets = vec![
            HostTarget::new("host-b", "host-b"),
            HostTarget::new("host-a", "host-a"),
        ];

        let results = fan_out(targets, |target| async move {
            if target.host_id == "host-a" {
                Err(ProtocolError::new(
                    ErrorClass::Daemon,
                    "host_failed",
                    "host-a failed",
                    Some("try again".to_owned()),
                ))
            } else {
                Ok(format!("ok:{}", target.host_id))
            }
        })
        .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].host_id, "host-a");
        assert!(!results[0].ok);
        assert_eq!(
            results[0].error.as_ref().expect("error").code,
            "host_failed"
        );
        assert_eq!(results[1].host_id, "host-b");
        assert!(results[1].ok);
        assert_eq!(results[1].value.as_deref(), Some("ok:host-b"));
    }

    #[test]
    fn host_result_shape_carries_target_and_error_details() {
        let target = HostTarget::new("host-b", "host-b");
        let result = HostResult::<String>::failure(
            target,
            ProtocolError::new(
                ErrorClass::Configuration,
                "bad_filter",
                "bad filter",
                Some("change the filter".to_owned()),
            ),
        );

        assert_eq!(result.host_id, "host-b");
        assert_eq!(result.transport_target, "host-b");
        assert!(!result.ok);
        assert!(result.value.is_none());
        let error = result.error.expect("error");
        assert_eq!(error.class, ErrorClass::Configuration);
        assert_eq!(error.code, "bad_filter");
        assert_eq!(error.recover.as_deref(), Some("change the filter"));
    }
}

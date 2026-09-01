//! `pohunek notifications` — durable agent notification inbox commands.
//!
//! Notifications are stored by each host daemon and aggregated client-side when
//! the operator asks for all hosts. This command module keeps rendering and
//! CLI-only filtering local to the CLI while consuming the typed SDK helpers.

use std::fmt::Write as _;
use std::str::FromStr;

use protocol::{
    event, method, Event, NotificationCreatedEvent, NotificationDeleteParams,
    NotificationDeletedEvent, NotificationId, NotificationKind, NotificationListParams,
    NotificationListResult, NotificationPolicy, NotificationPolicyParams, NotificationPolicyResult,
    NotificationRecord, NotificationRetentionParams, NotificationRetentionResult,
    NotificationSeverity, NotificationStatus, NotificationUpdateParams, NotificationUpdatedEvent,
    Request,
};
use serde::Serialize;

use crate::client::Client;
use crate::commands::host_fanout::{
    error_details, fan_out, resolve_targets, single_target, FanOutMode, HostResult, HostTarget,
};
use crate::error::CliError;
use crate::paths::Paths;

/// Parsed notification target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotificationTarget {
    /// `None` for the command's effective host, or an explicit target host.
    pub(crate) host: Option<String>,
    /// Host-local notification identifier.
    pub(crate) id: NotificationId,
}

/// Error parsing a notification target.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum NotificationTargetParseError {
    /// The input was empty.
    #[error("empty notification target")]
    Empty,
    /// The notification id portion was empty.
    #[error("missing notification id in target '{0}'")]
    MissingId(String),
    /// The host portion was empty.
    #[error("missing host in target '{0}'")]
    MissingHost(String),
    /// More than one `/` separator.
    #[error("invalid notification target '{0}': expected at most one '/' separating host and notification id")]
    TooManySeparators(String),
}

impl FromStr for NotificationTarget {
    type Err = NotificationTargetParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_notification_target(value)
    }
}

/// Filters accepted by `notifications list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListFilters {
    /// Alias for `--status unread`.
    pub(crate) unread: bool,
    /// Lifecycle status filter.
    pub(crate) status: Option<NotificationStatus>,
    /// Notification kind filter.
    pub(crate) kind: Option<NotificationKind>,
    /// Severity filter.
    pub(crate) severity: Option<NotificationSeverity>,
    /// Producer provider filter.
    pub(crate) provider: Option<String>,
    /// Agent kind filter; applied client-side because the daemon list API does
    /// not expose an agent filter.
    pub(crate) agent: Option<protocol::AgentKind>,
    /// Session id filter.
    pub(crate) session: Option<String>,
    /// Maximum number of records to return.
    pub(crate) limit: Option<u32>,
    /// Pagination cursor.
    pub(crate) cursor: Option<String>,
}

/// Output mode for `notifications watch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchOutputMode {
    /// Human-readable stream lines.
    Human,
    /// Machine-readable JSON event lines.
    Json,
}

/// Provider policy namespace selected by `policy set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyProvider {
    /// Default policy used when no provider override applies.
    Default,
    /// Codex provider override.
    Codex,
    /// Claude provider override.
    Claude,
    /// Hermes provider override.
    Hermes,
}

/// Arguments accepted by `retention prune`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetentionArgs {
    /// Report matches without deleting them.
    pub(crate) dry_run: bool,
    /// Delete matched records.
    pub(crate) apply: bool,
    /// Restrict to a lifecycle status.
    pub(crate) status: Option<NotificationStatus>,
    /// Prune records created before this timestamp.
    pub(crate) before: Option<String>,
    /// Maximum number of records to prune.
    pub(crate) limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostedNotification {
    host_id: String,
    record: NotificationRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostErrorRow {
    host_id: String,
    error: protocol::ProtocolError,
}

#[derive(Debug, Serialize)]
struct AllHostsJson<'a, T> {
    hosts: &'a [HostResult<T>],
}

#[derive(Debug, Serialize)]
struct WatchRecordJson<'a> {
    host_id: &'a str,
    event: &'a str,
    record: &'a NotificationRecord,
}

#[derive(Debug, Serialize)]
struct WatchDeletedJson<'a> {
    host_id: &'a str,
    event: &'a str,
    notification_id: &'a NotificationId,
}

/// Parse a notification target in bare `id` or `host/id` form.
pub(crate) fn parse_notification_target(
    value: &str,
) -> Result<NotificationTarget, NotificationTargetParseError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(NotificationTargetParseError::Empty);
    }

    let mut parts = value.splitn(3, '/');
    let first = parts.next().unwrap_or_default();
    match (parts.next(), parts.next()) {
        (None, _) => Ok(NotificationTarget {
            host: None,
            id: NotificationId(first.to_owned()),
        }),
        (Some(id), None) => {
            if first.is_empty() {
                return Err(NotificationTargetParseError::MissingHost(value.to_owned()));
            }
            if id.is_empty() {
                return Err(NotificationTargetParseError::MissingId(value.to_owned()));
            }
            Ok(NotificationTarget {
                host: Some(first.to_owned()),
                id: NotificationId(id.to_owned()),
            })
        }
        (Some(_), Some(_)) => Err(NotificationTargetParseError::TooManySeparators(
            value.to_owned(),
        )),
    }
}

/// Parse a notification status wire value.
pub(crate) fn parse_notification_status(value: &str) -> Result<NotificationStatus, String> {
    match value {
        "unread" => Ok(NotificationStatus::Unread),
        "read" => Ok(NotificationStatus::Read),
        "acknowledged" => Ok(NotificationStatus::Acknowledged),
        "archived" => Ok(NotificationStatus::Archived),
        "deleted" => Ok(NotificationStatus::Deleted),
        other => Err(format!("invalid notification status '{other}'")),
    }
}

/// Parse a notification kind wire value.
pub(crate) fn parse_notification_kind(value: &str) -> Result<NotificationKind, String> {
    match value {
        "agent_blocked" => Ok(NotificationKind::AgentBlocked),
        "approval_required" => Ok(NotificationKind::ApprovalRequired),
        "turn_completed" => Ok(NotificationKind::TurnCompleted),
        "session_finished" => Ok(NotificationKind::SessionFinished),
        "error" => Ok(NotificationKind::Error),
        "system" => Ok(NotificationKind::System),
        other => Err(format!("invalid notification kind '{other}'")),
    }
}

/// Parse a notification severity wire value.
pub(crate) fn parse_notification_severity(value: &str) -> Result<NotificationSeverity, String> {
    match value {
        "info" => Ok(NotificationSeverity::Info),
        "success" => Ok(NotificationSeverity::Success),
        "warning" => Ok(NotificationSeverity::Warning),
        "error" => Ok(NotificationSeverity::Error),
        "action_required" => Ok(NotificationSeverity::ActionRequired),
        other => Err(format!("invalid notification severity '{other}'")),
    }
}

/// Parse an agent kind wire value.
pub(crate) fn parse_agent_kind(value: &str) -> Result<protocol::AgentKind, String> {
    match value {
        "shell" => Ok(protocol::AgentKind::Shell),
        "codex" => Ok(protocol::AgentKind::Codex),
        "claude" => Ok(protocol::AgentKind::Claude),
        "hermes" => Ok(protocol::AgentKind::Hermes),
        other => Err(format!("invalid agent kind '{other}'")),
    }
}

/// Parse a policy provider selector.
pub(crate) fn parse_policy_provider(value: &str) -> Result<PolicyProvider, String> {
    match value {
        "default" => Ok(PolicyProvider::Default),
        "codex" => Ok(PolicyProvider::Codex),
        "claude" => Ok(PolicyProvider::Claude),
        "hermes" => Ok(PolicyProvider::Hermes),
        other => Err(format!("invalid notification policy provider '{other}'")),
    }
}

/// Run `notifications list`.
///
/// # Errors
///
/// Returns [`CliError`] when host discovery fails, any selected daemon cannot be
/// queried in single-host mode, or a response cannot be decoded.
pub(crate) async fn run_list(
    host: &str,
    paths: &Paths,
    filters: ListFilters,
    all_hosts: bool,
    json: bool,
) -> Result<(), CliError> {
    let params = build_list_params(filters.clone());
    if all_hosts {
        let targets = resolve_targets(paths, FanOutMode::AllHosts).await?;
        let agent = filters.agent;
        let results = fan_out(targets, |target| {
            let params = params.clone();
            let agent = agent.clone();
            async move { list_on_target(paths, target, params, agent).await }
        })
        .await;

        if json {
            print!("{}", render_all_hosts_list_json(&results)?);
        } else {
            print!("{}", render_all_hosts_list_human(&results));
        }
        return Ok(());
    }

    let target = single_target(host);
    let mut result = list_on_target(paths, target.clone(), params, filters.agent)
        .await
        .map_err(CliError::Protocol)?;
    sort_notifications(&mut result.notifications);
    if json {
        print!("{}", render_single_host_list_json(&result)?);
    } else {
        let rows = result
            .notifications
            .into_iter()
            .map(|record| HostedNotification {
                host_id: target.host_id.clone(),
                record,
            })
            .collect::<Vec<_>>();
        print!("{}", render_list_human(&rows, &[]));
    }
    Ok(())
}

/// Run `notifications watch`.
///
/// # Errors
///
/// Returns [`CliError`] when host discovery fails or a selected subscription
/// cannot be established in single-host mode.
pub(crate) async fn run_watch(
    host: &str,
    paths: &Paths,
    all_hosts: bool,
    json: bool,
) -> Result<(), CliError> {
    let mode = if json {
        WatchOutputMode::Json
    } else {
        WatchOutputMode::Human
    };
    let targets = if all_hosts {
        resolve_targets(paths, FanOutMode::AllHosts).await?
    } else {
        vec![single_target(host)]
    };

    if targets.len() == 1 {
        watch_one(paths, targets[0].clone(), mode)
            .await
            .map_err(CliError::Protocol)?;
        return Ok(());
    }

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    for target in targets {
        let paths = paths.clone();
        let sender = sender.clone();
        tokio::spawn(async move {
            let result = watch_one(&paths, target.clone(), mode).await;
            if let Err(error) = result {
                let _ = sender.send(render_watch_host_error(&target, &error, mode));
            }
        });
    }
    drop(sender);

    while let Some(line) = receiver.recv().await {
        print!("{line}");
    }
    Ok(())
}

/// Mark a notification read.
pub(crate) async fn run_read(
    host: &str,
    paths: &Paths,
    target: &NotificationTarget,
    json: bool,
) -> Result<(), CliError> {
    run_update(host, paths, target, NotificationStatus::Read, json).await
}

/// Acknowledge a notification.
pub(crate) async fn run_ack(
    host: &str,
    paths: &Paths,
    target: &NotificationTarget,
    json: bool,
) -> Result<(), CliError> {
    run_update(host, paths, target, NotificationStatus::Acknowledged, json).await
}

/// Archive a notification.
pub(crate) async fn run_archive(
    host: &str,
    paths: &Paths,
    target: &NotificationTarget,
    json: bool,
) -> Result<(), CliError> {
    run_update(host, paths, target, NotificationStatus::Archived, json).await
}

/// Delete a notification.
///
/// # Errors
///
/// Returns [`CliError`] when the target daemon rejects the delete or returns an
/// unexpected payload.
pub(crate) async fn run_delete(
    host: &str,
    paths: &Paths,
    target: &NotificationTarget,
    json: bool,
) -> Result<(), CliError> {
    let host = effective_notification_host(host, target);
    let client = Client::connect(&host, paths).await?;
    let mut sdk = client.into_sdk();
    let result = sdk
        .delete_notification(NotificationDeleteParams {
            id: target.id.clone(),
        })
        .await?;

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        println!(
            "notification {} delete on {host}: {}",
            result.id.0, result.deleted
        );
    }
    Ok(())
}

/// Run `notifications policy get`.
pub(crate) async fn run_policy_get(
    host: &str,
    paths: &Paths,
    all_hosts: bool,
    json: bool,
) -> Result<(), CliError> {
    if all_hosts {
        let targets = resolve_targets(paths, FanOutMode::AllHosts).await?;
        let results = fan_out(targets, |target| async move {
            policy_get_on_target(paths, target).await
        })
        .await;
        if json {
            print!("{}", render_all_hosts_json(&results)?);
        } else {
            print!("{}", render_policy_results_human(&results));
        }
        return Ok(());
    }

    let target = single_target(host);
    let result = policy_get_on_target(paths, target)
        .await
        .map_err(CliError::Protocol)?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_policy_human(host, &result.policy));
    }
    Ok(())
}

/// Run `notifications policy set`.
pub(crate) async fn run_policy_set(
    host: &str,
    paths: &Paths,
    provider: PolicyProvider,
    kind: NotificationKind,
    enabled: bool,
    all_hosts: bool,
    json: bool,
) -> Result<(), CliError> {
    if all_hosts {
        let targets = resolve_targets(paths, FanOutMode::AllHosts).await?;
        let results = fan_out(targets, |target| async move {
            policy_set_on_target(paths, target, provider, kind, enabled).await
        })
        .await;
        if json {
            print!("{}", render_all_hosts_json(&results)?);
        } else {
            print!("{}", render_policy_results_human(&results));
        }
        return Ok(());
    }

    let target = single_target(host);
    let result = policy_set_on_target(paths, target, provider, kind, enabled)
        .await
        .map_err(CliError::Protocol)?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_policy_human(host, &result.policy));
    }
    Ok(())
}

/// Run `notifications retention prune`.
pub(crate) async fn run_retention_prune(
    host: &str,
    paths: &Paths,
    args: RetentionArgs,
    all_hosts: bool,
    json: bool,
) -> Result<(), CliError> {
    let params = build_retention_params(args);
    if all_hosts {
        let targets = resolve_targets(paths, FanOutMode::AllHosts).await?;
        let results = fan_out(targets, |target| {
            let params = params.clone();
            async move { retention_prune_on_target(paths, target, params).await }
        })
        .await;
        if json {
            print!("{}", render_all_hosts_json(&results)?);
        } else {
            print!("{}", render_retention_results_human(&results));
        }
        return Ok(());
    }

    let target = single_target(host);
    let result = retention_prune_on_target(paths, target, params)
        .await
        .map_err(CliError::Protocol)?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_retention_human(host, &result));
    }
    Ok(())
}

fn build_list_params(filters: ListFilters) -> NotificationListParams {
    NotificationListParams {
        status: if filters.unread {
            Some(NotificationStatus::Unread)
        } else {
            filters.status
        },
        kind: filters.kind,
        severity: filters.severity,
        provider: filters.provider,
        session_id: filters.session.map(protocol::SessionId),
        created_after: None,
        created_before: None,
        limit: filters.limit,
        cursor: filters.cursor,
    }
}

fn build_retention_params(args: RetentionArgs) -> NotificationRetentionParams {
    let dry_run = match (args.dry_run, args.apply) {
        (true, false) => true,
        (false, true) => false,
        _ => args.dry_run,
    };
    NotificationRetentionParams {
        dry_run,
        status: args.status,
        before: args.before,
        limit: args.limit,
    }
}

fn effective_notification_host(global: &str, target: &NotificationTarget) -> String {
    target.host.as_deref().unwrap_or(global).to_owned()
}

async fn connect_target(paths: &Paths, target: &HostTarget) -> Result<Client, CliError> {
    Client::connect(&target.transport_target, paths).await
}

async fn list_on_target(
    paths: &Paths,
    target: HostTarget,
    params: NotificationListParams,
    agent: Option<protocol::AgentKind>,
) -> Result<NotificationListResult, protocol::ProtocolError> {
    let client = connect_target(paths, &target)
        .await
        .map_err(error_details)?;
    let mut sdk = client.into_sdk();
    let mut result = sdk
        .list_notifications(params)
        .await
        .map_err(CliError::from)
        .map_err(error_details)?;
    result.notifications.retain(|record| {
        agent
            .as_ref()
            .is_none_or(|wanted| record.agent_kind.as_ref() == Some(wanted))
    });
    sort_notifications(&mut result.notifications);
    Ok(result)
}

async fn policy_get_on_target(
    paths: &Paths,
    target: HostTarget,
) -> Result<NotificationPolicyResult, protocol::ProtocolError> {
    let client = connect_target(paths, &target)
        .await
        .map_err(error_details)?;
    let mut sdk = client.into_sdk();
    sdk.get_notification_policy()
        .await
        .map_err(CliError::from)
        .map_err(error_details)
}

async fn policy_set_on_target(
    paths: &Paths,
    target: HostTarget,
    provider: PolicyProvider,
    kind: NotificationKind,
    enabled: bool,
) -> Result<NotificationPolicyResult, protocol::ProtocolError> {
    let mut client = connect_target(paths, &target)
        .await
        .map_err(error_details)?
        .into_sdk();
    let mut policy = client
        .get_notification_policy()
        .await
        .map_err(CliError::from)
        .map_err(error_details)?
        .policy;
    apply_policy_toggle(&mut policy, provider, kind, enabled);
    client
        .set_notification_policy(NotificationPolicyParams { policy })
        .await
        .map_err(CliError::from)
        .map_err(error_details)
}

async fn retention_prune_on_target(
    paths: &Paths,
    target: HostTarget,
    params: NotificationRetentionParams,
) -> Result<NotificationRetentionResult, protocol::ProtocolError> {
    let client = connect_target(paths, &target)
        .await
        .map_err(error_details)?;
    let mut sdk = client.into_sdk();
    sdk.prune_notifications(params)
        .await
        .map_err(CliError::from)
        .map_err(error_details)
}

async fn watch_one(
    paths: &Paths,
    target: HostTarget,
    mode: WatchOutputMode,
) -> Result<(), protocol::ProtocolError> {
    let client = connect_target(paths, &target)
        .await
        .map_err(error_details)?;
    let request = Request::new(
        crate::commands::request_id(method::SUBSCRIBE),
        method::SUBSCRIBE,
        serde_json::Value::Null,
    )
    .map_err(pohunek_client::ClientError::from)
    .map_err(CliError::from)
    .map_err(error_details)?;
    let mut subscription = client
        .into_sdk()
        .subscribe(&request)
        .await
        .map_err(CliError::from)
        .map_err(error_details)?;
    while let Some(event) = subscription
        .next_event()
        .await
        .map_err(CliError::from)
        .map_err(error_details)?
    {
        let rendered = render_watch_event(&target.host_id, &event, mode).map_err(error_details)?;
        if !rendered.is_empty() {
            print!("{rendered}");
        }
    }
    Ok(())
}

async fn run_update(
    host: &str,
    paths: &Paths,
    target: &NotificationTarget,
    status: NotificationStatus,
    json: bool,
) -> Result<(), CliError> {
    let host = effective_notification_host(host, target);
    let client = Client::connect(&host, paths).await?;
    let mut sdk = client.into_sdk();
    let result = sdk
        .update_notification(NotificationUpdateParams {
            id: target.id.clone(),
            status,
        })
        .await?;

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        println!(
            "notification {} marked {} on {host}",
            result.record.id.0,
            result.record.status.as_str()
        );
    }
    Ok(())
}

fn apply_policy_toggle(
    policy: &mut NotificationPolicy,
    provider: PolicyProvider,
    kind: NotificationKind,
    enabled: bool,
) {
    match provider {
        PolicyProvider::Default => set_kind_enabled(&mut policy.enabled, kind, enabled),
        PolicyProvider::Codex => {
            let kinds = policy
                .providers
                .entry("codex".to_owned())
                .or_insert_with(|| policy.enabled.clone());
            set_kind_enabled(kinds, kind, enabled);
        }
        PolicyProvider::Claude => {
            let kinds = policy
                .providers
                .entry("claude".to_owned())
                .or_insert_with(|| policy.enabled.clone());
            set_kind_enabled(kinds, kind, enabled);
        }
        PolicyProvider::Hermes => {
            let kinds = policy
                .providers
                .entry("hermes".to_owned())
                .or_insert_with(|| policy.enabled.clone());
            set_kind_enabled(kinds, kind, enabled);
        }
    }
}

fn set_kind_enabled(
    policy: &mut protocol::NotificationKindPolicy,
    kind: NotificationKind,
    enabled: bool,
) {
    match kind {
        NotificationKind::AgentBlocked => policy.agent_blocked = enabled,
        NotificationKind::ApprovalRequired => policy.approval_required = enabled,
        NotificationKind::TurnCompleted => policy.turn_completed = enabled,
        NotificationKind::SessionFinished => policy.session_finished = enabled,
        NotificationKind::Error => policy.error = enabled,
        NotificationKind::System => policy.system = enabled,
    }
}

#[cfg(test)]
fn filter_rows_by_agent(
    rows: &[HostedNotification],
    agent: Option<&protocol::AgentKind>,
) -> Vec<HostedNotification> {
    rows.iter()
        .filter(|row| agent.is_none_or(|wanted| row.record.agent_kind.as_ref() == Some(wanted)))
        .cloned()
        .collect()
}

fn render_single_host_list_json(result: &NotificationListResult) -> Result<String, CliError> {
    crate::commands::render_json(result)
}

fn render_all_hosts_list_json(
    results: &[HostResult<NotificationListResult>],
) -> Result<String, CliError> {
    render_all_hosts_json(results)
}

fn render_all_hosts_json<T: Serialize>(results: &[HostResult<T>]) -> Result<String, CliError> {
    crate::commands::render_json(&AllHostsJson { hosts: results })
}

fn render_all_hosts_list_human(results: &[HostResult<NotificationListResult>]) -> String {
    let rows = results
        .iter()
        .filter_map(|result| {
            result.value.as_ref().map(|value| {
                value
                    .notifications
                    .iter()
                    .cloned()
                    .map(|record| HostedNotification {
                        host_id: result.host_id.clone(),
                        record,
                    })
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect::<Vec<_>>();
    let errors = results
        .iter()
        .filter_map(|result| {
            result.error.as_ref().map(|error| HostErrorRow {
                host_id: result.host_id.clone(),
                error: error.clone(),
            })
        })
        .collect::<Vec<_>>();
    render_list_human(&rows, &errors)
}

fn render_list_human(rows: &[HostedNotification], errors: &[HostErrorRow]) -> String {
    let host_width = rows
        .iter()
        .map(|row| row.host_id.len())
        .chain(errors.iter().map(|row| row.host_id.len()))
        .max()
        .unwrap_or(0)
        .max("HOST".len());
    let status_width = rows
        .iter()
        .map(|row| row.record.status.as_str().len())
        .max()
        .unwrap_or(0)
        .max("STATUS".len());
    let severity_width = rows
        .iter()
        .map(|row| row.record.severity.as_str().len())
        .max()
        .unwrap_or(0)
        .max("SEVERITY".len());
    let age_width = rows
        .iter()
        .map(|row| row.record.created_at.len())
        .max()
        .unwrap_or(0)
        .max("AGE".len());
    let session_width = rows
        .iter()
        .map(|row| session_label(&row.record).len())
        .max()
        .unwrap_or(0)
        .max("SESSION".len());
    let kind_width = rows
        .iter()
        .map(|row| row.record.kind.as_str().len())
        .max()
        .unwrap_or(0)
        .max("KIND".len());

    let mut output = String::new();
    let _ = writeln!(
        output,
        "{:<host_width$}  {:<status_width$}  {:<severity_width$}  {:<age_width$}  {:<session_width$}  {:<kind_width$}  TITLE",
        "HOST", "STATUS", "SEVERITY", "AGE", "SESSION", "KIND",
    );
    for row in rows {
        let _ = writeln!(
            output,
            "{:<host_width$}  {:<status_width$}  {:<severity_width$}  {:<age_width$}  {:<session_width$}  {:<kind_width$}  {}",
            row.host_id,
            row.record.status.as_str(),
            row.record.severity.as_str(),
            age_label(&row.record),
            session_label(&row.record),
            row.record.kind.as_str(),
            row.record.title,
        );
    }
    for row in errors {
        let title = format!("{}: {}", row.error.code, row.error.msg);
        let _ = writeln!(
            output,
            "{:<host_width$}  {:<status_width$}  {:<severity_width$}  {:<age_width$}  {:<session_width$}  {:<kind_width$}  {title}",
            row.host_id,
            "error",
            "-",
            "-",
            "-",
            "-",
        );
    }
    output
}

fn render_watch_event(
    host_id: &str,
    event: &Event,
    mode: WatchOutputMode,
) -> Result<String, CliError> {
    match event.event() {
        event::NOTIFICATION_CREATED => {
            let payload: NotificationCreatedEvent =
                serde_json::from_value(event.payload().clone())?;
            render_watch_record(host_id, event::NOTIFICATION_CREATED, &payload.record, mode)
        }
        event::NOTIFICATION_UPDATED => {
            let payload: NotificationUpdatedEvent =
                serde_json::from_value(event.payload().clone())?;
            render_watch_record(host_id, event::NOTIFICATION_UPDATED, &payload.record, mode)
        }
        event::NOTIFICATION_DELETED => {
            let payload: NotificationDeletedEvent =
                serde_json::from_value(event.payload().clone())?;
            match mode {
                WatchOutputMode::Human => Ok(format!(
                    "{host_id} notification_deleted {} deleted\n",
                    payload.notification_id.0
                )),
                WatchOutputMode::Json => Ok(format!(
                    "{}\n",
                    serde_json::to_string(&WatchDeletedJson {
                        host_id,
                        event: event::NOTIFICATION_DELETED,
                        notification_id: &payload.notification_id,
                    })?
                )),
            }
        }
        _ => Ok(String::new()),
    }
}

fn render_watch_record(
    host_id: &str,
    event: &str,
    record: &NotificationRecord,
    mode: WatchOutputMode,
) -> Result<String, CliError> {
    match mode {
        WatchOutputMode::Human => Ok(format!(
            "{host_id} {event} {} {} {} {}\n",
            record.status.as_str(),
            record.severity.as_str(),
            record.id.0,
            record.title
        )),
        WatchOutputMode::Json => Ok(format!(
            "{}\n",
            serde_json::to_string(&WatchRecordJson {
                host_id,
                event,
                record,
            })?
        )),
    }
}

fn render_watch_host_error(
    target: &HostTarget,
    error: &protocol::ProtocolError,
    mode: WatchOutputMode,
) -> String {
    match mode {
        WatchOutputMode::Human => {
            format!("{} error {}: {}\n", target.host_id, error.code, error.msg)
        }
        WatchOutputMode::Json => {
            let result = HostResult::<()>::failure(target.clone(), error.clone());
            serde_json::to_string(&result).map_or_else(
                |_err| {
                    format!(
                        r#"{{"host_id":"{}","transport_target":"{}","ok":false}}"#,
                        target.host_id, target.transport_target
                    )
                },
                |doc| format!("{doc}\n"),
            )
        }
    }
}

fn render_policy_results_human(results: &[HostResult<NotificationPolicyResult>]) -> String {
    let mut output = String::new();
    for result in results {
        if let Some(value) = &result.value {
            output.push_str(&render_policy_human(&result.host_id, &value.policy));
        }
        if let Some(error) = &result.error {
            let _ = writeln!(
                output,
                "host {} error {}: {}",
                result.host_id, error.code, error.msg
            );
        }
    }
    output
}

fn render_policy_human(host: &str, policy: &NotificationPolicy) -> String {
    let mut output = format!("host {host} notification policy\n");
    let _ = writeln!(
        output,
        "  attention_dedupe_window_secs: {}",
        policy.attention_dedupe_window_secs
    );
    let _ = writeln!(
        output,
        "  attention_debounce_secs: {}",
        policy.attention_debounce_secs
    );
    let retention = &policy.retention;
    let _ = writeln!(
        output,
        "  retention: sweep={}s info={}s warning={}s resolved_attention={}s resolved_error={}s archived={}s compaction_min_actions={}",
        retention.sweep_interval_secs,
        retention.info_ttl_secs,
        retention.warning_ttl_secs,
        retention.resolved_attention_ttl_secs,
        retention.resolved_error_ttl_secs,
        retention.archived_ttl_secs,
        retention.compaction_min_actions,
    );
    render_kind_policy(&mut output, "default", &policy.enabled);
    for (provider, provider_policy) in &policy.providers {
        render_kind_policy(&mut output, provider, provider_policy);
    }
    output
}

fn render_kind_policy(output: &mut String, label: &str, policy: &protocol::NotificationKindPolicy) {
    let _ = writeln!(
        output,
        "  {label}: agent_blocked={} approval_required={} turn_completed={} session_finished={} error={} system={}",
        policy.agent_blocked,
        policy.approval_required,
        policy.turn_completed,
        policy.session_finished,
        policy.error,
        policy.system,
    );
}

fn render_retention_results_human(results: &[HostResult<NotificationRetentionResult>]) -> String {
    let mut output = String::new();
    for result in results {
        if let Some(value) = &result.value {
            output.push_str(&render_retention_human(&result.host_id, value));
        }
        if let Some(error) = &result.error {
            let _ = writeln!(
                output,
                "host {} error {}: {}",
                result.host_id, error.code, error.msg
            );
        }
    }
    output
}

fn render_retention_human(host: &str, result: &NotificationRetentionResult) -> String {
    let action = if result.dry_run { "matched" } else { "pruned" };
    let mut output = format!(
        "host {host} retention {action} {} notifications\n",
        result.pruned.len()
    );
    for id in &result.pruned {
        let _ = writeln!(output, "  {}", id.0);
    }
    output
}

fn session_label(record: &NotificationRecord) -> &str {
    record
        .session_id
        .as_ref()
        .map_or("-", |session_id| session_id.0.as_str())
}

fn age_label(record: &NotificationRecord) -> &str {
    if record.created_at.is_empty() {
        "-"
    } else {
        &record.created_at
    }
}

fn sort_notifications(records: &mut [NotificationRecord]) {
    records.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .reverse()
            .then_with(|| left.id.0.cmp(&right.id.0))
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use protocol::{NotificationKindPolicy, NotificationSource};
    use serde_json::json;

    use super::*;

    fn record(id: &str, status: NotificationStatus) -> protocol::NotificationRecord {
        protocol::NotificationRecord {
            id: NotificationId(id.to_owned()),
            source: NotificationSource {
                provider: "codex".to_owned(),
                provider_event: "PermissionRequest".to_owned(),
                host_local_source_id: format!("hook:{id}"),
            },
            kind: NotificationKind::ApprovalRequired,
            severity: NotificationSeverity::ActionRequired,
            status,
            title: format!("Title {id}"),
            body: "Body".to_owned(),
            metadata: BTreeMap::new(),
            created_at: "2026-07-03T10:00:00Z".to_owned(),
            session_id: Some(protocol::SessionId("s-1".to_owned())),
            agent_kind: Some(protocol::AgentKind::Codex),
            source_id: Some(format!("codex:{id}")),
            dedupe_key: Some("attention:s-1".to_owned()),
            project_id: None,
            read_at: None,
            acked_at: None,
            archived_at: None,
            deleted_at: None,
            superseded_by: None,
        }
    }

    fn policy() -> NotificationPolicy {
        let enabled = NotificationKindPolicy {
            agent_blocked: true,
            approval_required: true,
            turn_completed: false,
            session_finished: false,
            error: true,
            system: false,
        };
        NotificationPolicy {
            attention_dedupe_window_secs: 120,
            attention_debounce_secs: 5,
            enabled: enabled.clone(),
            providers: std::collections::BTreeMap::from([
                ("claude".to_owned(), enabled.clone()),
                ("codex".to_owned(), enabled),
            ]),
            retention: protocol::NotificationRetentionPolicy::default(),
        }
    }

    #[test]
    fn parses_bare_notification_target_as_implicit_host() {
        let target = parse_notification_target("n-1").expect("parse");

        assert_eq!(target.host.as_deref(), None);
        assert_eq!(target.id, NotificationId("n-1".to_owned()));
    }

    #[test]
    fn parses_host_qualified_notification_target() {
        let target = parse_notification_target("host-b/n-1").expect("parse");

        assert_eq!(target.host.as_deref(), Some("host-b"));
        assert_eq!(target.id, NotificationId("n-1".to_owned()));
    }

    #[test]
    fn builds_list_params_from_unread_and_filters() {
        let params = build_list_params(ListFilters {
            unread: true,
            status: None,
            kind: Some(NotificationKind::ApprovalRequired),
            severity: Some(NotificationSeverity::ActionRequired),
            provider: Some("codex".to_owned()),
            agent: Some(protocol::AgentKind::Codex),
            session: Some("s-1".to_owned()),
            limit: Some(25),
            cursor: Some("next".to_owned()),
        });

        assert_eq!(params.status, Some(NotificationStatus::Unread));
        assert_eq!(params.kind, Some(NotificationKind::ApprovalRequired));
        assert_eq!(params.severity, Some(NotificationSeverity::ActionRequired));
        assert_eq!(params.provider.as_deref(), Some("codex"));
        assert_eq!(
            params.session_id,
            Some(protocol::SessionId("s-1".to_owned()))
        );
        assert_eq!(params.limit, Some(25));
        assert_eq!(params.cursor.as_deref(), Some("next"));
    }

    #[test]
    fn human_list_render_includes_required_columns_and_host_values() {
        let rows = vec![HostedNotification {
            host_id: "host-b".to_owned(),
            record: record("n-1", NotificationStatus::Unread),
        }];

        let output = render_list_human(&rows, &[]);

        let header = output.lines().next().expect("header");
        for column in [
            "HOST", "STATUS", "SEVERITY", "AGE", "SESSION", "KIND", "TITLE",
        ] {
            assert!(header.contains(column), "header missing {column}: {header}");
        }
        assert!(output.contains("host-b"));
        assert!(output.contains("unread"));
        assert!(output.contains("action_required"));
        assert!(output.contains("s-1"));
        assert!(output.contains("approval_required"));
        assert!(output.contains("Title n-1"));
    }

    #[test]
    fn json_list_render_includes_host_id_for_all_hosts() {
        let results = vec![HostResult::success(
            HostTarget::new("host-b", "host-b"),
            NotificationListResult {
                notifications: vec![record("n-1", NotificationStatus::Unread)],
                next_cursor: Some("cursor-2".to_owned()),
            },
        )];

        let output = render_all_hosts_list_json(&results).expect("render");
        let doc: serde_json::Value = serde_json::from_str(&output).expect("json");

        assert_eq!(doc["ok"]["hosts"][0]["host_id"], "host-b");
        assert_eq!(doc["ok"]["hosts"][0]["ok"], true);
        assert_eq!(
            doc["ok"]["hosts"][0]["value"]["notifications"][0]["id"],
            "n-1"
        );
        assert_eq!(doc["ok"]["hosts"][0]["value"]["next_cursor"], "cursor-2");
    }

    #[test]
    fn watch_render_handles_notification_events() {
        let created = Event::new(
            protocol::PROTOCOL_VERSION,
            event::NOTIFICATION_CREATED,
            serde_json::to_value(NotificationCreatedEvent {
                record: record("n-1", NotificationStatus::Unread),
            })
            .expect("payload"),
        )
        .expect("created event");
        let updated = Event::new(
            protocol::PROTOCOL_VERSION,
            event::NOTIFICATION_UPDATED,
            serde_json::to_value(NotificationUpdatedEvent {
                record: record("n-1", NotificationStatus::Read),
            })
            .expect("payload"),
        )
        .expect("updated event");
        let deleted = Event::new(
            protocol::PROTOCOL_VERSION,
            event::NOTIFICATION_DELETED,
            serde_json::to_value(NotificationDeletedEvent {
                notification_id: NotificationId("n-1".to_owned()),
            })
            .expect("payload"),
        )
        .expect("deleted event");

        assert!(
            render_watch_event("host-b", &created, WatchOutputMode::Human)
                .expect("created")
                .contains("notification_created")
        );
        assert!(
            render_watch_event("host-b", &updated, WatchOutputMode::Human)
                .expect("updated")
                .contains("notification_updated")
        );
        let deleted_output =
            render_watch_event("host-b", &deleted, WatchOutputMode::Json).expect("deleted");
        let doc: serde_json::Value = serde_json::from_str(&deleted_output).expect("json");
        assert_eq!(doc["host_id"], "host-b");
        assert_eq!(doc["event"], "notification_deleted");
        assert_eq!(doc["notification_id"], "n-1");
    }

    #[test]
    fn policy_get_json_render_includes_attention_debounce_secs() {
        let result = NotificationPolicyResult { policy: policy() };

        let output = crate::commands::render_json(&result).expect("render");
        let doc: serde_json::Value = serde_json::from_str(&output).expect("json");

        assert_eq!(doc["ok"]["policy"]["attention_debounce_secs"], 5);
        assert_eq!(doc["ok"]["policy"]["attention_dedupe_window_secs"], 120);
        assert_eq!(doc["ok"]["policy"]["retention"]["info_ttl_secs"], 259_200);
    }

    #[test]
    fn policy_human_render_includes_attention_debounce_secs() {
        let output = render_policy_human("host-b", &policy());

        assert!(output.contains("attention_debounce_secs: 5"));
        assert!(output.contains("retention: sweep=21600s info=259200s"));
    }

    #[test]
    fn policy_toggle_enables_and_disables_turn_completed() {
        let mut policy = policy();

        apply_policy_toggle(
            &mut policy,
            PolicyProvider::Codex,
            NotificationKind::TurnCompleted,
            true,
        );
        assert!(policy.providers["codex"].turn_completed);

        apply_policy_toggle(
            &mut policy,
            PolicyProvider::Codex,
            NotificationKind::TurnCompleted,
            false,
        );
        assert!(!policy.providers["codex"].turn_completed);
    }

    #[test]
    fn hermes_is_a_selectable_agent_and_policy_provider() {
        assert_eq!(parse_agent_kind("hermes"), Ok(protocol::AgentKind::Hermes));
        assert_eq!(parse_policy_provider("hermes"), Ok(PolicyProvider::Hermes));

        let mut policy = policy();
        apply_policy_toggle(
            &mut policy,
            PolicyProvider::Hermes,
            NotificationKind::TurnCompleted,
            true,
        );

        assert!(policy.providers["hermes"].turn_completed);
    }

    #[test]
    fn retention_params_distinguish_dry_run_and_apply() {
        let dry_run = build_retention_params(RetentionArgs {
            dry_run: true,
            apply: false,
            status: Some(NotificationStatus::Archived),
            before: Some("2026-07-03T10:00:00Z".to_owned()),
            limit: Some(5),
        });
        let apply = build_retention_params(RetentionArgs {
            dry_run: false,
            apply: true,
            status: None,
            before: None,
            limit: None,
        });

        assert_eq!(
            dry_run,
            NotificationRetentionParams {
                dry_run: true,
                status: Some(NotificationStatus::Archived),
                before: Some("2026-07-03T10:00:00Z".to_owned()),
                limit: Some(5),
            }
        );
        assert_eq!(
            apply,
            NotificationRetentionParams {
                dry_run: false,
                status: None,
                before: None,
                limit: None,
            }
        );
    }

    #[test]
    fn agent_filter_is_applied_client_side() {
        let rows = vec![
            HostedNotification {
                host_id: "host-b".to_owned(),
                record: record("n-1", NotificationStatus::Unread),
            },
            HostedNotification {
                host_id: "host-b".to_owned(),
                record: protocol::NotificationRecord {
                    agent_kind: Some(protocol::AgentKind::Claude),
                    ..record("n-2", NotificationStatus::Unread)
                },
            },
        ];

        let filtered = filter_rows_by_agent(&rows, Some(&protocol::AgentKind::Codex));

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].record.id, NotificationId("n-1".to_owned()));
    }

    #[test]
    fn single_host_json_render_keeps_daemon_shape() {
        let result = NotificationListResult {
            notifications: vec![record("n-1", NotificationStatus::Unread)],
            next_cursor: None,
        };

        let output = render_single_host_list_json(&result).expect("render");
        let doc: serde_json::Value = serde_json::from_str(&output).expect("json");

        assert_eq!(
            doc["ok"],
            json!({
                "notifications": [{
                    "id": "n-1",
                    "source": {
                        "provider": "codex",
                        "provider_event": "PermissionRequest",
                        "host_local_source_id": "hook:n-1"
                    },
                    "kind": "approval_required",
                    "severity": "action_required",
                    "status": "unread",
                    "title": "Title n-1",
                    "body": "Body",
                    "created_at": "2026-07-03T10:00:00Z",
                    "session_id": "s-1",
                    "agent_kind": "codex",
                    "source_id": "codex:n-1",
                    "dedupe_key": "attention:s-1"
                }]
            })
        );
    }
}

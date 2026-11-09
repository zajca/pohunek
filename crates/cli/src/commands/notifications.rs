//! `pohunek notifications` — the attention inbox commands (`list` and `watch`).
//!
//! Notifications are stored by each host daemon and aggregated client-side when
//! the operator asks for all hosts. This command module keeps rendering and
//! CLI-only filtering local to the CLI while consuming the typed SDK helpers.

use std::fmt::Write as _;

use protocol::{
    event, method, Event, NotificationCreatedEvent, NotificationDeletedEvent, NotificationId,
    NotificationKind, NotificationListParams, NotificationListResult, NotificationRecord,
    NotificationSeverity, NotificationStatus, NotificationUpdatedEvent, Request,
};
use serde::Serialize;

use crate::client::Client;
use crate::commands::host_fanout::{
    error_details, fan_out, resolve_targets, single_target, FanOutMode, HostResult, HostTarget,
};
use crate::error::CliError;
use crate::paths::Paths;

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
    pub(crate) agent: Option<protocol::RuntimeRef>,
    /// Session id filter.
    pub(crate) session: Option<String>,
    /// Maximum number of records to return.
    pub(crate) limit: Option<u32>,
    /// Pagination cursor.
    pub(crate) cursor: Option<String>,
}

/// Filters accepted by `notifications watch`.
///
/// The daemon subscription is global, so these filters run client-side on each
/// streamed event, with the value semantics of the matching `notifications
/// list` flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WatchFilters {
    /// Alias for `--status unread`.
    pub(crate) unread: bool,
    /// Lifecycle status the record carries in the event.
    pub(crate) status: Option<NotificationStatus>,
    /// Notification kind filter.
    pub(crate) kind: Option<NotificationKind>,
    /// Linked session id filter; a record without a session never matches.
    pub(crate) session: Option<String>,
}

impl WatchFilters {
    /// Returns whether no filter is set, so every event passes unchanged.
    fn is_empty(&self) -> bool {
        self.effective_status().is_none() && self.kind.is_none() && self.session.is_none()
    }

    /// Returns the status filter with `--unread` resolved.
    fn effective_status(&self) -> Option<NotificationStatus> {
        if self.unread {
            Some(NotificationStatus::Unread)
        } else {
            self.status
        }
    }

    /// Returns whether `record` satisfies every set filter.
    fn matches(&self, record: &NotificationRecord) -> bool {
        self.effective_status()
            .is_none_or(|status| record.status == status)
            && self.kind.is_none_or(|kind| record.kind == kind)
            && self.session.as_deref().is_none_or(|session| {
                record
                    .session_id
                    .as_ref()
                    .is_some_and(|linked| linked.0 == session)
            })
    }
}

/// Output mode for `notifications watch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchOutputMode {
    /// Human-readable stream lines.
    Human,
    /// Machine-readable JSON event lines.
    Json,
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

/// Parse a `notifications watch --status` value.
///
/// Accepts every status `notifications list` accepts except `deleted`: the
/// daemon announces a deletion only as a `notification_deleted` event that
/// carries the id alone, so no streamed record can ever match it.
pub(crate) fn parse_watch_status(value: &str) -> Result<NotificationStatus, String> {
    match parse_notification_status(value)? {
        NotificationStatus::Deleted => Err(
            "notifications watch cannot filter on status 'deleted': deletions stream as \
             notification_deleted events, which carry only the id; watch without filters \
             to see them"
                .to_owned(),
        ),
        status => Ok(status),
    }
}

/// Parse a notification kind wire value.
///
/// Only the attention kinds exist as notifications; a turn end is waited for with
/// `pohunek session wait`.
pub(crate) fn parse_notification_kind(value: &str) -> Result<NotificationKind, String> {
    match value {
        "agent_blocked" => Ok(NotificationKind::AgentBlocked),
        "approval_required" => Ok(NotificationKind::ApprovalRequired),
        other => Err(format!(
            "invalid notification kind '{other}': notifications hold agent_blocked and \
             approval_required; wait for turn ends with `pohunek session wait`"
        )),
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

/// Parse an agent kind: any grammar-valid runtime id, installed or not, since
/// the filter only compares against the kinds notification records carry.
pub(crate) fn parse_agent_kind(value: &str) -> Result<protocol::RuntimeRef, String> {
    protocol::RuntimeId::parse(value)
        .map(protocol::RuntimeRef::from)
        .map_err(|_error| format!("invalid agent kind '{value}'"))
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
/// Every host subscription applies `filters` to its own event stream.
///
/// # Errors
///
/// Returns [`CliError`] when host discovery fails or a selected subscription
/// cannot be established in single-host mode.
pub(crate) async fn run_watch(
    host: &str,
    paths: &Paths,
    filters: WatchFilters,
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
        watch_one(paths, targets[0].clone(), mode, &filters)
            .await
            .map_err(CliError::Protocol)?;
        return Ok(());
    }

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    for target in targets {
        let paths = paths.clone();
        let sender = sender.clone();
        let filters = filters.clone();
        tokio::spawn(async move {
            let result = watch_one(&paths, target.clone(), mode, &filters).await;
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

async fn connect_target(paths: &Paths, target: &HostTarget) -> Result<Client, CliError> {
    Client::connect(&target.transport_target, paths).await
}

async fn list_on_target(
    paths: &Paths,
    target: HostTarget,
    params: NotificationListParams,
    agent: Option<protocol::RuntimeRef>,
) -> Result<NotificationListResult, protocol::ProtocolError> {
    let listed = if params.limit.is_some() {
        list_one_page(paths, &target, params).await
    } else {
        list_every_page(paths, &target, params).await
    };
    let mut result = listed.map_err(error_details)?;
    result.notifications.retain(|record| {
        agent
            .as_ref()
            .is_none_or(|wanted| record.agent_kind.as_ref() == Some(wanted))
    });
    sort_notifications(&mut result.notifications);
    Ok(result)
}

/// Lists the one page `params` asks for.
async fn list_one_page(
    paths: &Paths,
    target: &HostTarget,
    params: NotificationListParams,
) -> Result<NotificationListResult, CliError> {
    let mut sdk = connect_target(paths, target).await?.into_sdk();
    Ok(sdk.list_notifications(params).await?)
}

/// Records one `notification.list` page asks for when the command has no
/// `--limit`. A page of typical records fits one control line with room to
/// spare; a page that does not fit is asked again at half the size, down to a
/// single record.
const LIST_PAGE_SIZE: u32 = 200;

/// Lists every matching record by following `next_cursor` from `params.cursor`,
/// so the whole history never has to fit one response.
///
/// A daemon reports a page too large for one control line with
/// `notification_list_too_large`. A daemon of an earlier release sends the
/// oversized line instead, which leaves the connection unusable: the client
/// refuses it as a framing error from the local daemon and as an unavailable
/// daemon from a remote host. The listing then starts again from the first
/// page at half the size on a new connection. A cursor never crosses
/// connections, because a remote host name resolved again may select another
/// daemon, so every record listed comes from one connection.
async fn list_every_page(
    paths: &Paths,
    target: &HostTarget,
    params: NotificationListParams,
) -> Result<NotificationListResult, CliError> {
    let mut sdk = connect_target(paths, target).await?.into_sdk();
    let mut page_params = params.clone();
    let mut notifications = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut page_size = LIST_PAGE_SIZE;
    loop {
        page_params.limit = Some(page_size);
        let page = match sdk.list_notifications(page_params.clone()).await {
            Ok(page) => page,
            Err(error) if page_size > 1 && is_list_too_large(&error) => {
                page_size /= 2;
                continue;
            }
            Err(
                pohunek_client::ClientError::Framing(_)
                | pohunek_client::ClientError::RemoteDaemonUnavailable { .. },
            ) if page_size > 1 => {
                sdk = connect_target(paths, target).await?.into_sdk();
                page_params = params.clone();
                notifications.clear();
                visited.clear();
                page_size /= 2;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        notifications.extend(page.notifications);
        let Some(next) = page.next_cursor else {
            break;
        };
        // A cursor the walk already followed would never end it.
        if !visited.insert(next.clone()) {
            return Err(CliError::Protocol(protocol::ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "notification_list_cursor_repeated",
                "the daemon repeated a notification.list pagination cursor",
                Some("upgrade the daemon and client together".to_owned()),
            )));
        }
        page_params.cursor = Some(next);
    }
    Ok(NotificationListResult {
        notifications,
        next_cursor: None,
    })
}

/// Whether `error` is the daemon's answer to a page that did not fit one
/// control line.
fn is_list_too_large(error: &pohunek_client::ClientError) -> bool {
    match error {
        pohunek_client::ClientError::Protocol(source)
        | pohunek_client::ClientError::RemoteProtocol { source, .. } => {
            source.code == protocol::ProtocolError::NOTIFICATION_LIST_TOO_LARGE
        }
        _ => false,
    }
}

async fn watch_one(
    paths: &Paths,
    target: HostTarget,
    mode: WatchOutputMode,
    filters: &WatchFilters,
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
        if !watch_event_selected(&event, filters).map_err(error_details)? {
            continue;
        }
        let rendered = render_watch_event(&target.host_id, &event, mode).map_err(error_details)?;
        if !rendered.is_empty() {
            print!("{rendered}");
        }
    }
    Ok(())
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

/// Returns whether `event` passes the `notifications watch` filters.
///
/// Without filters every event passes undecoded. With any filter, a create or
/// update passes when its record matches, and a delete never passes because its
/// payload carries only the notification id.
///
/// # Errors
///
/// Returns [`CliError`] when a filtered create or update payload cannot be
/// decoded.
fn watch_event_selected(event: &Event, filters: &WatchFilters) -> Result<bool, CliError> {
    if filters.is_empty() {
        return Ok(true);
    }
    match event.event() {
        event::NOTIFICATION_CREATED => {
            let payload: NotificationCreatedEvent =
                protocol::tolerant::from_value(event.payload().clone())?;
            Ok(filters.matches(&payload.record))
        }
        event::NOTIFICATION_UPDATED => {
            let payload: NotificationUpdatedEvent =
                protocol::tolerant::from_value(event.payload().clone())?;
            Ok(filters.matches(&payload.record))
        }
        _ => Ok(false),
    }
}

fn render_watch_event(
    host_id: &str,
    event: &Event,
    mode: WatchOutputMode,
) -> Result<String, CliError> {
    match event.event() {
        event::NOTIFICATION_CREATED => {
            let payload: NotificationCreatedEvent =
                protocol::tolerant::from_value(event.payload().clone())?;
            render_watch_record(host_id, event::NOTIFICATION_CREATED, &payload.record, mode)
        }
        event::NOTIFICATION_UPDATED => {
            let payload: NotificationUpdatedEvent =
                protocol::tolerant::from_value(event.payload().clone())?;
            render_watch_record(host_id, event::NOTIFICATION_UPDATED, &payload.record, mode)
        }
        event::NOTIFICATION_DELETED => {
            let payload: NotificationDeletedEvent =
                protocol::tolerant::from_value(event.payload().clone())?;
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

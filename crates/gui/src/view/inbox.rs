//! Inbox pane and notification detail: filters, list rows, actions, and age/date formatting.

use std::collections::BTreeSet;

use iced::widget::{button, column, row, text};
use iced::{Center, Element, Fill, Theme};
use pohunek_gui_core::{HostId, NotificationFilter, Selection};
use protocol::{
    NotificationId, NotificationKind, NotificationRecord, NotificationSeverity, NotificationStatus,
};

use crate::message::{Message, NotificationAction};
use crate::PohunekApp;

use super::{card, list_button, muted_style, push_meta, section_title, STATUS_DOT};

// Calendar conversion offset from the civil-date algorithm's day zero to Unix
// epoch; changing it would make notification age labels wrong for every row.
const UNIX_EPOCH_DAY_OFFSET: i64 = 719_468;

// Gregorian 400-year era length used by the civil date conversion below.
const DAYS_PER_ERA: i64 = 146_097;

// Calendar years in one Gregorian era.
const YEARS_PER_ERA: i64 = 400;

// March-based month arithmetic used by the civil date conversion.
const MARCH_BASED_MONTH_OFFSET: i64 = 9;

// Month numerator from Howard Hinnant's days-from-civil algorithm.
const MONTH_DAY_NUMERATOR: i64 = 153;

// Notification age labels are intentionally coarse; these named thresholds keep
// row text short and prevent timestamp math from scattering UI constants.
const SECONDS_PER_MINUTE: u64 = 60;

const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;

pub(crate) const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;

const SECONDS_PER_WEEK: u64 = 7 * SECONDS_PER_DAY;

pub(crate) fn inbox_pane(app: &PohunekApp) -> Element<'_, Message> {
    let unread = app.workspace.unread_notification_count();
    let rows = app.workspace.notifications(&app.notification_filter);
    let header = row![
        text("Inbox").size(22),
        text(format!("{unread} unread")).size(13).style(muted_style),
        iced::widget::space().width(Fill),
        button("Clear filters")
            .on_press(Message::ClearNotificationFilters)
            .style(iced::widget::button::secondary),
    ]
    .spacing(10)
    .align_y(Center);
    let mut list = column![].spacing(5);
    let mut shown = 0_usize;
    for row in rows {
        shown += 1;
        list = list.push(notification_row(app, row.host_id, row.record));
    }
    if shown == 0 {
        list = list.push(text("No notifications match the filters").size(13));
    }
    column![header, notification_filters(app), card(list),]
        .spacing(12)
        .into()
}

fn notification_filters(app: &PohunekApp) -> Element<'_, Message> {
    column![
        notification_status_filters(app),
        notification_severity_filters(app),
        notification_kind_filters(app),
        notification_provider_filters(app),
        notification_host_filters(app),
    ]
    .spacing(4)
    .into()
}

fn notification_status_filters(app: &PohunekApp) -> Element<'static, Message> {
    let statuses = [
        NotificationStatus::Unread,
        NotificationStatus::Read,
        NotificationStatus::Acknowledged,
        NotificationStatus::Archived,
    ];
    let mut row = row![
        text("Status").size(13).style(muted_style),
        notification_chip(
            "all",
            notification_count_with(app, |filter| filter.status = None),
            app.notification_filter.status.is_none(),
            Message::FilterNotificationStatus(None),
        ),
    ]
    .spacing(6)
    .align_y(Center);
    for status in statuses {
        let active = app.notification_filter.status == Some(status);
        row = row.push(notification_chip(
            notification_status_label(status),
            notification_count_with(app, |filter| filter.status = Some(status)),
            active,
            Message::FilterNotificationStatus((!active).then_some(status)),
        ));
    }
    row.into()
}

fn notification_severity_filters(app: &PohunekApp) -> Element<'static, Message> {
    let severities = [
        NotificationSeverity::ActionRequired,
        NotificationSeverity::Error,
        NotificationSeverity::Warning,
        NotificationSeverity::Info,
        NotificationSeverity::Success,
    ];
    let mut row = row![
        text("Severity").size(13).style(muted_style),
        notification_chip(
            "all",
            notification_count_with(app, |filter| filter.severity = None),
            app.notification_filter.severity.is_none(),
            Message::FilterNotificationSeverity(None),
        ),
    ]
    .spacing(6)
    .align_y(Center);
    for severity in severities {
        let active = app.notification_filter.severity == Some(severity);
        row = row.push(notification_chip(
            notification_severity_label(severity),
            notification_count_with(app, |filter| filter.severity = Some(severity)),
            active,
            Message::FilterNotificationSeverity((!active).then_some(severity)),
        ));
    }
    row.into()
}

fn notification_kind_filters(app: &PohunekApp) -> Element<'static, Message> {
    let kinds = [
        NotificationKind::AgentBlocked,
        NotificationKind::ApprovalRequired,
        NotificationKind::Error,
        NotificationKind::TurnCompleted,
        NotificationKind::SessionFinished,
        NotificationKind::System,
    ];
    let mut row = row![
        text("Kind").size(13).style(muted_style),
        notification_chip(
            "all",
            notification_count_with(app, |filter| filter.kind = None),
            app.notification_filter.kind.is_none(),
            Message::FilterNotificationKind(None),
        ),
    ]
    .spacing(6)
    .align_y(Center);
    for kind in kinds {
        let active = app.notification_filter.kind == Some(kind);
        row = row.push(notification_chip(
            notification_kind_label(kind),
            notification_count_with(app, |filter| filter.kind = Some(kind)),
            active,
            Message::FilterNotificationKind((!active).then_some(kind)),
        ));
    }
    row.into()
}

fn notification_provider_filters(app: &PohunekApp) -> Element<'static, Message> {
    let providers = notification_providers(app);
    let mut row = row![
        text("Provider").size(13).style(muted_style),
        notification_chip(
            "all",
            notification_count_with(app, |filter| filter.provider = None),
            app.notification_filter.provider.is_none(),
            Message::FilterNotificationProvider(None),
        ),
    ]
    .spacing(6)
    .align_y(Center);
    for provider in providers {
        let active = app.notification_filter.provider.as_ref() == Some(&provider);
        row = row.push(notification_chip(
            provider.clone(),
            notification_count_with(app, |filter| filter.provider = Some(provider.clone())),
            active,
            Message::FilterNotificationProvider((!active).then_some(provider)),
        ));
    }
    row.into()
}

fn notification_host_filters(app: &PohunekApp) -> Element<'static, Message> {
    let mut row = row![
        text("Host").size(13).style(muted_style),
        notification_chip(
            "all",
            notification_count_with(app, |filter| filter.host_id = None),
            app.notification_filter.host_id.is_none(),
            Message::FilterNotificationHost(None),
        ),
    ]
    .spacing(6)
    .align_y(Center);
    for host_id in app.workspace.hosts.keys() {
        let active = app.notification_filter.host_id.as_ref() == Some(host_id);
        row = row.push(notification_chip(
            host_id.to_string(),
            notification_count_with(app, |filter| filter.host_id = Some(host_id.clone())),
            active,
            Message::FilterNotificationHost((!active).then(|| host_id.clone())),
        ));
    }
    row.into()
}

fn notification_chip(
    label: impl Into<String>,
    count: usize,
    active: bool,
    message: Message,
) -> Element<'static, Message> {
    let chip = button(text(format!("{} {count}", label.into())).size(13)).on_press(message);
    if active {
        chip.style(iced::widget::button::primary).into()
    } else {
        chip.style(iced::widget::button::text).into()
    }
}

fn notification_count_with(
    app: &PohunekApp,
    update: impl FnOnce(&mut NotificationFilter),
) -> usize {
    let mut filter = app.notification_filter.clone();
    update(&mut filter);
    app.workspace.notifications(&filter).len()
}

fn notification_providers(app: &PohunekApp) -> Vec<String> {
    let mut providers = BTreeSet::new();
    for host in app.workspace.hosts.values() {
        for record in host.notifications.values() {
            providers.insert(record.source.provider.clone());
        }
    }
    providers.into_iter().collect()
}

fn notification_row(
    app: &PohunekApp,
    host_id: HostId,
    record: NotificationRecord,
) -> Element<'static, Message> {
    let selected = matches!(
        app.ui_state.selection.as_ref(),
        Some(Selection::Notification { host_id: h, notification_id })
            if h == &host_id && notification_id == &record.id
    );
    let notification_id = record.id.clone();
    let mut meta = String::new();
    push_meta(&mut meta, notification_status_label(record.status));
    push_meta(&mut meta, notification_severity_label(record.severity));
    push_meta(&mut meta, &host_id.to_string());
    push_meta(
        &mut meta,
        record
            .session_id
            .as_ref()
            .map_or("no session", |session_id| session_id.0.as_str()),
    );
    push_meta(&mut meta, notification_kind_label(record.kind));
    push_meta(&mut meta, &notification_age_label(&record.created_at));
    row![
        notification_dot(record.severity),
        list_button(
            column![
                text(record.title)
                    .size(14)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                text(meta).size(11).style(muted_style),
            ]
            .spacing(1),
            Message::SelectNotification {
                host_id,
                notification_id,
            },
            selected,
        ),
    ]
    .spacing(6)
    .align_y(Center)
    .into()
}

pub(crate) fn notification_pane(app: &PohunekApp) -> Element<'_, Message> {
    let Some((host_id, record)) = selected_notification(app) else {
        return card(column![
            section_title("Notification"),
            text("Notification not found").size(13)
        ]);
    };
    let mut detail = column![
        section_title("Notification"),
        text(record.title.as_str())
            .size(16)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        text(record.body.as_str())
            .size(14)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        notification_summary(record, host_id),
        notification_actions(host_id, record),
    ]
    .spacing(8);
    if let Some(link) = notification_link_action(app, host_id, record) {
        detail = detail.push(link);
    }
    detail = detail.push(notification_metadata(record));
    card(detail)
}

fn notification_summary<'a>(
    record: &'a NotificationRecord,
    host_id: &'a HostId,
) -> Element<'a, Message> {
    let mut rows = column![text("Summary").size(15)].spacing(4);
    rows = rows
        .push(text(format!("host: {host_id}")).size(13))
        .push(
            text(format!(
                "status: {}",
                notification_status_label(record.status)
            ))
            .size(13),
        )
        .push(
            text(format!(
                "severity: {}",
                notification_severity_label(record.severity)
            ))
            .size(13),
        )
        .push(text(format!("kind: {}", notification_kind_label(record.kind))).size(13))
        .push(text(format!("created: {}", record.created_at)).size(13))
        .push(
            text(format!(
                "age: {}",
                notification_age_label(&record.created_at)
            ))
            .size(13),
        )
        .push(
            text(format!(
                "source: {} / {} / {}",
                record.source.provider,
                record.source.provider_event,
                record.source.host_local_source_id
            ))
            .size(13),
        );
    if let Some(session_id) = &record.session_id {
        rows = rows.push(text(format!("session: {}", session_id.0)).size(13));
    }
    if let Some(agent_kind) = record.agent_kind {
        rows = rows.push(text(format!("agent: {}", agent_kind_label(agent_kind))).size(13));
    }
    if let Some(project_id) = &record.project_id {
        rows = rows.push(text(format!("project: {project_id}")).size(13));
    }
    rows.into()
}

fn notification_actions(
    host_id: &HostId,
    record: &NotificationRecord,
) -> Element<'static, Message> {
    let mut actions = row![].spacing(8);
    if record.status == NotificationStatus::Unread {
        actions = actions.push(notification_action_button(
            "Mark read",
            host_id,
            &record.id,
            NotificationAction::Read,
            iced::widget::button::secondary,
        ));
    }
    if record.status != NotificationStatus::Acknowledged {
        actions = actions.push(notification_action_button(
            "Acknowledge",
            host_id,
            &record.id,
            NotificationAction::Acknowledge,
            iced::widget::button::secondary,
        ));
    }
    if record.status != NotificationStatus::Archived {
        actions = actions.push(notification_action_button(
            "Archive",
            host_id,
            &record.id,
            NotificationAction::Archive,
            iced::widget::button::secondary,
        ));
    }
    actions = actions.push(notification_action_button(
        "Delete",
        host_id,
        &record.id,
        NotificationAction::Delete,
        iced::widget::button::danger,
    ));
    actions.into()
}

fn notification_action_button(
    label: &'static str,
    host_id: &HostId,
    notification_id: &NotificationId,
    action: NotificationAction,
    style: fn(&Theme, iced::widget::button::Status) -> iced::widget::button::Style,
) -> Element<'static, Message> {
    button(text(label).size(13))
        .padding([5, 9])
        .on_press(Message::ActOnNotification {
            host_id: host_id.clone(),
            notification_id: notification_id.clone(),
            action,
        })
        .style(style)
        .into()
}

fn notification_metadata(record: &NotificationRecord) -> Element<'_, Message> {
    let mut metadata = column![text("Metadata").size(15)].spacing(4);
    if record.metadata.is_empty()
        && record.source_id.is_none()
        && record.dedupe_key.is_none()
        && record.superseded_by.is_none()
    {
        return metadata.push(text("No metadata").size(13)).into();
    }
    for (key, value) in &record.metadata {
        metadata = metadata.push(text(format!("{key}: {value}")).size(13));
    }
    if let Some(source_id) = &record.source_id {
        metadata = metadata.push(text(format!("source_id: {source_id}")).size(13));
    }
    if let Some(dedupe_key) = &record.dedupe_key {
        metadata = metadata.push(text(format!("dedupe_key: {dedupe_key}")).size(13));
    }
    if let Some(superseded_by) = &record.superseded_by {
        metadata = metadata.push(text(format!("superseded_by: {}", superseded_by.0)).size(13));
    }
    metadata.into()
}

fn notification_link_action<'a>(
    app: &'a PohunekApp,
    host_id: &'a HostId,
    record: &'a NotificationRecord,
) -> Option<Element<'a, Message>> {
    let session_id = record.session_id.as_ref()?;
    let live = app
        .workspace
        .hosts
        .get(host_id)
        .is_some_and(|host| host.sessions.contains_key(&session_id.0));
    let content: Element<'a, Message> = if live {
        button("Open linked session")
            .on_press(Message::OpenNotificationLink {
                host_id: host_id.clone(),
                notification_id: record.id.clone(),
            })
            .style(iced::widget::button::primary)
            .into()
    } else {
        text(format!("Linked session {} is no longer live", session_id.0))
            .size(13)
            .style(muted_style)
            .into()
    };
    Some(content)
}

fn selected_notification(app: &PohunekApp) -> Option<(&HostId, &NotificationRecord)> {
    let Some(Selection::Notification {
        host_id,
        notification_id,
    }) = app.ui_state.selection.as_ref()
    else {
        return None;
    };
    app.workspace
        .notification(host_id, notification_id)
        .map(|record| (host_id, record))
}

fn notification_status_label(status: NotificationStatus) -> &'static str {
    match status {
        NotificationStatus::Unread => "unread",
        NotificationStatus::Read => "read",
        NotificationStatus::Acknowledged => "ack",
        NotificationStatus::Archived => "archived",
        NotificationStatus::Deleted => "deleted",
    }
}

fn notification_severity_label(severity: NotificationSeverity) -> &'static str {
    match severity {
        NotificationSeverity::Info => "info",
        NotificationSeverity::Success => "success",
        NotificationSeverity::Warning => "warning",
        NotificationSeverity::Error => "error",
        NotificationSeverity::ActionRequired => "action req",
    }
}

fn notification_kind_label(kind: NotificationKind) -> &'static str {
    match kind {
        NotificationKind::AgentBlocked => "agent blocked",
        NotificationKind::ApprovalRequired => "approval required",
        NotificationKind::TurnCompleted => "turn complete",
        NotificationKind::SessionFinished => "session finished",
        NotificationKind::Error => "error",
        NotificationKind::System => "system",
    }
}

fn agent_kind_label(kind: protocol::AgentKind) -> &'static str {
    match kind {
        protocol::AgentKind::Shell => "shell",
        protocol::AgentKind::Codex => "codex",
        protocol::AgentKind::Claude => "claude",
    }
}

fn notification_dot(severity: NotificationSeverity) -> Element<'static, Message> {
    text(STATUS_DOT)
        .size(13)
        .style(move |theme: &Theme| iced::widget::text::Style {
            color: Some(notification_color(theme, severity)),
        })
        .into()
}

fn notification_color(theme: &Theme, severity: NotificationSeverity) -> iced::Color {
    let palette = theme.extended_palette();
    match severity {
        NotificationSeverity::ActionRequired | NotificationSeverity::Error => {
            palette.danger.base.color
        }
        NotificationSeverity::Warning => palette.warning.base.color,
        NotificationSeverity::Success => palette.success.base.color,
        NotificationSeverity::Info => palette.secondary.base.color,
    }
}

fn notification_age_label(created_at: &str) -> String {
    let Some(created) = parse_rfc3339_utc_seconds(created_at) else {
        return date_part(created_at).to_owned();
    };
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return date_part(created_at).to_owned();
    };
    let elapsed = now.as_secs().saturating_sub(created);
    if elapsed < SECONDS_PER_MINUTE {
        "now".to_owned()
    } else if elapsed < SECONDS_PER_HOUR {
        format!("{}m", elapsed / SECONDS_PER_MINUTE)
    } else if elapsed < SECONDS_PER_DAY {
        format!("{}h", elapsed / SECONDS_PER_HOUR)
    } else if elapsed < SECONDS_PER_WEEK {
        format!("{}d", elapsed / SECONDS_PER_DAY)
    } else {
        date_part(created_at).to_owned()
    }
}

pub(crate) fn parse_rfc3339_utc_seconds(value: &str) -> Option<u64> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let mut date_parts = date.split('-');
    let year = date_parts.next()?.parse::<i32>().ok()?;
    let month = date_parts.next()?.parse::<u32>().ok()?;
    let day = date_parts.next()?.parse::<u32>().ok()?;
    if date_parts.next().is_some() || !valid_civil_date(year, month, day) {
        return None;
    }
    let mut time_parts = time.split(':');
    let hour = time_parts.next()?.parse::<u32>().ok()?;
    let minute = time_parts.next()?.parse::<u32>().ok()?;
    let second = time_parts.next()?.split('.').next()?.parse::<u32>().ok()?;
    if time_parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = unix_days_from_civil(year, month, day)?;
    let seconds = days.checked_mul(i64::try_from(SECONDS_PER_DAY).ok()?)?
        + i64::from(hour) * i64::try_from(SECONDS_PER_HOUR).ok()?
        + i64::from(minute) * i64::try_from(SECONDS_PER_MINUTE).ok()?
        + i64::from(second);
    u64::try_from(seconds).ok()
}

fn valid_civil_date(year: i32, month: u32, day: u32) -> bool {
    year >= 1970 && (1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn unix_days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = if year >= 0 {
        year
    } else {
        year - (YEARS_PER_ERA - 1)
    } / YEARS_PER_ERA;
    let year_of_era = year - era * YEARS_PER_ERA;
    let month = i64::from(month);
    let month = month
        + if month > 2 {
            -3
        } else {
            MARCH_BASED_MONTH_OFFSET
        };
    let day_of_year = (MONTH_DAY_NUMERATOR * month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    (era * DAYS_PER_ERA + day_of_era).checked_sub(UNIX_EPOCH_DAY_OFFSET)
}

/// Extracts the `YYYY-MM-DD` date from an RFC 3339 timestamp.
pub(crate) fn date_part(timestamp: &str) -> &str {
    timestamp
        .split_once('T')
        .map_or(timestamp, |(date, _)| date)
}

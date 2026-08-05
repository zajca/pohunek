//! Inbox modal: notification list and message-detail layers, plus the
//! age/date formatting they share.

use std::collections::BTreeSet;

use iced::widget::{button, column, container, pick_list, row, scrollable, text};
use iced::{Center, Element, Fill, Theme};
use pohunek_gui_core::{HostId, NotificationScope, Selection};
use protocol::{
    NotificationId, NotificationKind, NotificationKindPolicy, NotificationRecord,
    NotificationSeverity, NotificationStatus, SessionId,
};

use crate::attach::window_dimension_to_f32;
use crate::message::{InboxView, Message, NotificationAction};
use crate::view::provider::{status_pill, PillTone};
use crate::PohunekApp;

use super::{agent_kind_label, card, list_button, muted_style, push_meta, STATUS_DOT};

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

// Wide enough to read a notification body comfortably; the other modals use
// `dialog_card`'s fixed 640px, which felt cramped for triage + full text.
const INBOX_MODAL_WIDTH: f32 = 760.0;

// Leaves headroom above/below the dialog for the dimmed backdrop; the body
// scrolls internally beyond this so a long notification list or body never
// grows the dialog past the operator's window.
const INBOX_MODAL_HEIGHT_RATIO: f32 = 0.8;

// Sentinel `pick_list` option meaning "no host filter"; it can never collide
// with a real host id, which comes from config-defined host slugs.
const INBOX_ALL_HOSTS_LABEL: &str = "All hosts";

/// Routes the inbox modal to its current layer: the notification list, or one
/// message's detail (auto-marked read by `Message::SelectNotification`).
pub(crate) fn inbox_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    match &app.inbox_view {
        InboxView::List => inbox_list_content(app),
        InboxView::Message {
            host_id,
            notification_id,
        } => inbox_message_content(app, host_id, notification_id),
    }
}

/// Layer 1: header with unread count, the scope/host controls, and the
/// notification list. Rows are pre-sorted for triage by `Workspace::inbox_rows`.
fn inbox_list_content(app: &PohunekApp) -> Element<'_, Message> {
    let unread = app.workspace.unread_notification_count();
    let header = row![
        text(format!("Inbox · {unread} unread")).size(20),
        iced::widget::space().width(Fill),
        button("Close")
            .on_press(Message::CloseModal)
            .style(iced::widget::button::secondary),
    ]
    .align_y(Center);

    let rows = app
        .workspace
        .inbox_rows(app.inbox_scope, &app.notification_filter);
    let mut list = column![].spacing(6);
    if rows.is_empty() {
        list = list.push(text(inbox_empty_label(app.inbox_scope)).size(13));
    } else {
        for row in rows {
            let selected = app
                .inbox_cursor
                .as_ref()
                .is_some_and(|(host_id, id)| host_id == &row.host_id && id == &row.record.id);
            list = list.push(notification_row(app, row.host_id, row.record, selected));
        }
    }

    inbox_dialog(
        app,
        column![
            header,
            inbox_controls(app),
            notification_policy_card(app),
            card(list)
        ]
        .spacing(12),
    )
}

fn notification_policy_card(app: &PohunekApp) -> Element<'_, Message> {
    let Some(host_id) = notification_policy_host(app) else {
        return card(text("Select a host to manage its notification policy").size(12));
    };
    let load = button("Load notification policy")
        .on_press(Message::LoadNotificationPolicy(host_id.clone()))
        .style(iced::widget::button::secondary);
    let Some(policy) = app.workspace.notification_policy(&host_id) else {
        return card(column![text(format!("Notification policy · {host_id}")), load].spacing(6));
    };

    let mut providers = BTreeSet::new();
    if let Some(host) = app.workspace.hosts.get(&host_id) {
        providers.extend(host.notification_providers.iter().cloned());
    }
    providers.extend(policy.providers.keys().cloned());

    let mut rows = column![
        row![
            text(format!("Notification policy · {host_id}")).size(14),
            iced::widget::space().width(Fill),
            load,
            button("Save")
                .on_press(Message::SaveNotificationPolicy(host_id.clone()))
                .style(iced::widget::button::primary),
        ]
        .spacing(6)
        .align_y(Center),
        policy_kind_row(&host_id, None, "base", &policy.enabled),
    ]
    .spacing(6);
    for provider in providers {
        let provider_policy = policy.for_provider(&provider);
        rows = rows.push(policy_kind_row(
            &host_id,
            Some(&provider),
            &provider,
            provider_policy,
        ));
    }
    card(rows)
}

fn notification_policy_host(app: &PohunekApp) -> Option<HostId> {
    app.notification_filter.host_id.clone().or_else(|| {
        app.workspace
            .selection
            .as_ref()
            .map(|selection| match selection {
                Selection::Host { host_id }
                | Selection::Project { host_id, .. }
                | Selection::Session { host_id, .. } => host_id.clone(),
            })
    })
}

fn policy_kind_row(
    host_id: &HostId,
    provider: Option<&str>,
    label: &str,
    policy: &NotificationKindPolicy,
) -> Element<'static, Message> {
    let mut values = row![text(label.to_owned()).size(12)]
        .spacing(4)
        .align_y(Center);
    for (kind, kind_label, enabled) in [
        (
            NotificationKind::AgentBlocked,
            "blocked",
            policy.agent_blocked,
        ),
        (
            NotificationKind::ApprovalRequired,
            "approval",
            policy.approval_required,
        ),
        (
            NotificationKind::TurnCompleted,
            "turn",
            policy.turn_completed,
        ),
        (
            NotificationKind::SessionFinished,
            "finished",
            policy.session_finished,
        ),
        (NotificationKind::Error, "error", policy.error),
        (NotificationKind::System, "system", policy.system),
    ] {
        values = values.push(
            button(text(format!(
                "{kind_label}: {}",
                if enabled { "on" } else { "off" }
            )))
            .on_press(Message::SetNotificationPolicyKind {
                host_id: host_id.clone(),
                provider: provider.map(str::to_owned),
                kind,
                enabled: !enabled,
            })
            .style(iced::widget::button::text),
        );
    }
    values.into()
}

/// The scope segmented control, plus (when 2+ hosts have notifications) the
/// host `pick_list` that replaces the old per-axis filter-chip rows.
fn inbox_controls(app: &PohunekApp) -> Element<'_, Message> {
    let mut controls = row![
        inbox_scope_button(
            "Needs action",
            NotificationScope::NeedsAction,
            app.inbox_scope
        ),
        inbox_scope_button("All", NotificationScope::All, app.inbox_scope),
        inbox_scope_button("Archived", NotificationScope::Archived, app.inbox_scope),
    ]
    .spacing(6)
    .align_y(Center);
    if let Some(picker) = inbox_host_picker(app) {
        controls = controls
            .push(iced::widget::space().width(Fill))
            .push(picker);
    }
    controls.into()
}

fn inbox_scope_button(
    label: &'static str,
    scope: NotificationScope,
    active: NotificationScope,
) -> Element<'static, Message> {
    let chip = button(text(label).size(13)).on_press(Message::SetInboxScope(scope));
    if scope == active {
        chip.style(iced::widget::button::primary).into()
    } else {
        chip.style(iced::widget::button::text).into()
    }
}

/// Host filter picker; hidden entirely below two hosts, since a single-host
/// workspace has nothing to narrow.
fn inbox_host_picker(app: &PohunekApp) -> Option<Element<'_, Message>> {
    let hosts_with_notifications: Vec<HostId> = app
        .workspace
        .hosts
        .iter()
        .filter(|(_, host)| !host.notifications.is_empty())
        .map(|(host_id, _)| host_id.clone())
        .collect();
    if hosts_with_notifications.len() < 2 {
        return None;
    }
    let mut options = vec![INBOX_ALL_HOSTS_LABEL.to_owned()];
    options.extend(hosts_with_notifications.iter().map(HostId::to_string));
    let selected = app
        .notification_filter
        .host_id
        .as_ref()
        .map_or_else(|| INBOX_ALL_HOSTS_LABEL.to_owned(), HostId::to_string);
    Some(
        pick_list(options, Some(selected), |value| {
            Message::FilterNotificationHost(
                (value != INBOX_ALL_HOSTS_LABEL).then(|| HostId::new(value)),
            )
        })
        .into(),
    )
}

fn inbox_empty_label(scope: NotificationScope) -> &'static str {
    match scope {
        NotificationScope::NeedsAction => "All clear.",
        NotificationScope::All => "No notifications",
        NotificationScope::Archived => "No archived notifications",
    }
}

/// One inbox row: severity dot (+ an `action` pill for agent-blocked/approval
/// prompts) and title on the first line, age right-aligned; host, linked
/// session, and kind on a muted second line. Clicking opens the message layer.
fn notification_row(
    app: &PohunekApp,
    host_id: HostId,
    record: NotificationRecord,
    selected: bool,
) -> Element<'static, Message> {
    let unread = record.status == NotificationStatus::Unread;
    let needs_action = matches!(
        record.kind,
        NotificationKind::AgentBlocked | NotificationKind::ApprovalRequired
    );
    let session_label = record
        .session_id
        .as_ref()
        .map(|session_id| session_display_label(app, &host_id, session_id));

    let mut meta = String::new();
    push_meta(&mut meta, &host_id.to_string());
    push_meta(&mut meta, session_label.as_deref().unwrap_or("no session"));
    push_meta(&mut meta, notification_kind_label(record.kind));

    let mut title = text(record.title)
        .size(14)
        .wrapping(iced::widget::text::Wrapping::WordOrGlyph);
    if unread {
        title = title.font(iced::Font {
            weight: iced::font::Weight::Bold,
            ..iced::Font::DEFAULT
        });
    }

    let mut title_row = row![notification_dot(record.severity)]
        .spacing(6)
        .align_y(Center);
    if needs_action {
        title_row = title_row.push(status_pill("action", PillTone::Danger));
    }
    title_row = title_row
        .push(title)
        .push(iced::widget::space().width(Fill))
        .push(
            text(notification_age_label(&record.created_at))
                .size(11)
                .style(muted_style),
        );

    let notification_id = record.id.clone();
    list_button(
        column![title_row, text(meta).size(11).style(muted_style)].spacing(2),
        Message::SelectNotification {
            host_id,
            notification_id,
        },
        selected,
    )
}

/// Layer 2: back/title/severity header, meta line, scrollable body, primary
/// actions, and a collapsible `> Details` expander.
fn inbox_message_content<'a>(
    app: &'a PohunekApp,
    host_id: &'a HostId,
    notification_id: &'a NotificationId,
) -> Element<'a, Message> {
    let Some(record) = app.workspace.notification(host_id, notification_id) else {
        // The record vanished from under the operator (e.g. deleted from
        // another client); fall back to the list instead of a dead end.
        return inbox_list_content(app);
    };
    let header = row![
        button("‹ Back")
            .on_press(Message::InboxBack)
            .style(iced::widget::button::text),
        text(record.title.as_str())
            .size(18)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        notification_severity_pill(record.severity),
        iced::widget::space().width(Fill),
        button("Close")
            .on_press(Message::CloseModal)
            .style(iced::widget::button::secondary),
    ]
    .spacing(10)
    .align_y(Center);

    let session_label = record
        .session_id
        .as_ref()
        .map(|session_id| session_display_label(app, host_id, session_id));
    let mut meta = String::new();
    push_meta(&mut meta, &host_id.to_string());
    push_meta(&mut meta, session_label.as_deref().unwrap_or("no session"));
    push_meta(&mut meta, notification_kind_label(record.kind));
    push_meta(&mut meta, &notification_age_label(&record.created_at));

    let body = container(
        scrollable(
            text(record.body.as_str())
                .size(14)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        )
        .width(Fill),
    )
    .height(220);

    let mut content = column![
        header,
        text(meta).size(12).style(muted_style),
        body,
        inbox_message_actions(app, host_id, record),
        inbox_details_toggle(app),
    ]
    .spacing(10);
    if app.inbox_details_expanded {
        content = content.push(notification_details(host_id, record));
    }
    inbox_dialog(app, content)
}

/// Primary `[Open session]` action plus lifecycle buttons; there is no
/// separate "Mark read" button since opening a message marks it read.
fn inbox_message_actions<'a>(
    app: &'a PohunekApp,
    host_id: &'a HostId,
    record: &'a NotificationRecord,
) -> Element<'a, Message> {
    let mut actions = row![].spacing(8);
    if let Some(link) = notification_link_action(app, host_id, record) {
        actions = actions.push(link);
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

/// `[Open session]`, gated on the linked session still being live; renders
/// explanatory text instead of a dead button when it is not.
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
        button("Open session")
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

/// The message layer's collapsible `> Details`: source triplet, created
/// timestamp, linked project/agent, safe metadata, and dedupe/source ids.
fn notification_details<'a>(
    host_id: &'a HostId,
    record: &'a NotificationRecord,
) -> Element<'a, Message> {
    let mut rows = column![
        text(format!(
            "status: {}",
            notification_status_label(record.status)
        ))
        .size(12),
        text(format!("host: {host_id}")).size(12),
        text(format!(
            "source: {} / {} / {}",
            record.source.provider,
            record.source.provider_event,
            record.source.host_local_source_id
        ))
        .size(12),
        text(format!("created: {}", record.created_at)).size(12),
    ]
    .spacing(4);
    if let Some(project_id) = &record.project_id {
        rows = rows.push(text(format!("project: {project_id}")).size(12));
    }
    if let Some(agent_kind) = &record.agent_kind {
        rows = rows.push(text(format!("agent: {}", agent_kind_label(agent_kind))).size(12));
    }
    for (key, value) in &record.metadata {
        rows = rows.push(text(format!("{key}: {value}")).size(12));
    }
    if let Some(source_id) = &record.source_id {
        rows = rows.push(text(format!("source_id: {source_id}")).size(12));
    }
    if let Some(dedupe_key) = &record.dedupe_key {
        rows = rows.push(text(format!("dedupe_key: {dedupe_key}")).size(12));
    }
    if let Some(superseded_by) = &record.superseded_by {
        rows = rows.push(text(format!("superseded_by: {}", superseded_by.0)).size(12));
    }
    card(rows)
}

fn inbox_details_toggle(app: &PohunekApp) -> Element<'_, Message> {
    let label = if app.inbox_details_expanded {
        "v Details"
    } else {
        "> Details"
    };
    button(text(label).size(13))
        .on_press(Message::ToggleInboxDetails)
        .style(iced::widget::button::text)
        .into()
}

/// Session display label for row/detail meta lines: the session's name when
/// set, else its id.
fn session_display_label(app: &PohunekApp, host_id: &HostId, session_id: &SessionId) -> String {
    app.workspace
        .hosts
        .get(host_id)
        .and_then(|host| host.sessions.get(&session_id.0))
        .and_then(|session| session.name.clone())
        .unwrap_or_else(|| session_id.0.clone())
}

/// A fixed-width, height-capped dialog body for the inbox modal, wider than
/// the standard `dialog_card` and scrolling internally past
/// [`INBOX_MODAL_HEIGHT_RATIO`] of the window height.
fn inbox_dialog<'a>(
    app: &'a PohunekApp,
    body: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let max_height =
        window_dimension_to_f32(app.ui_state.window_size.height) * INBOX_MODAL_HEIGHT_RATIO;
    container(scrollable(body).width(Fill))
        .padding(20)
        .width(INBOX_MODAL_WIDTH)
        .max_height(max_height)
        .style(iced::widget::container::rounded_box)
        .into()
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

fn notification_dot(severity: NotificationSeverity) -> Element<'static, Message> {
    text(STATUS_DOT)
        .size(13)
        .style(move |theme: &Theme| iced::widget::text::Style {
            color: Some(notification_color(theme, severity)),
        })
        .into()
}

fn notification_severity_pill(severity: NotificationSeverity) -> Element<'static, Message> {
    let tone = match severity {
        NotificationSeverity::ActionRequired | NotificationSeverity::Error => PillTone::Danger,
        NotificationSeverity::Warning => PillTone::Warning,
        NotificationSeverity::Success => PillTone::Success,
        NotificationSeverity::Info => PillTone::Neutral,
    };
    status_pill(notification_severity_label(severity), tone)
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

/// Renders a timestamp as a coarse age label (`now`/`Xm`/`Xh`/`Xd`, falling
/// back to the `YYYY-MM-DD` date past a week); shared by notification rows and
/// the provider item modals' `updated <age>` meta line.
pub(crate) fn notification_age_label(created_at: &str) -> String {
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

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn explicit_inbox_host_scopes_notification_policy() {
        let mut app = PohunekApp::test_default();
        app.notification_filter.host_id = Some(HostId::new("host-b"));

        assert_eq!(notification_policy_host(&app), Some(HostId::new("host-b")));
    }

    #[test]
    fn future_provider_policy_row_renders_without_provider_enum_support() {
        let policy = NotificationKindPolicy {
            agent_blocked: true,
            approval_required: true,
            turn_completed: true,
            session_finished: true,
            error: true,
            system: false,
        };
        let _: Element<'static, Message> = policy_kind_row(
            &HostId::new("local"),
            Some("future-agent"),
            "future-agent",
            &policy,
        );
    }
}

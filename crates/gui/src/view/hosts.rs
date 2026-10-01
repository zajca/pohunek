//! Host connection overview: the header button and the Hosts modal.

// Rust guideline compliant 2026-10-01

use iced::widget::{button, column, row, scrollable, text};
use iced::{Center, Element, Theme};
use pohunek_gui_core::{ConnState, GovernanceState, HostId, HostView};
use protocol::HostOwner;

use crate::message::Message;
use crate::PohunekApp;

use super::{card, conn_dot, dialog_card, muted_style};

/// Horizontal gap between the connection dots in the header button.
const HOST_STRIP_SPACING: f32 = 3.0;

/// Spacing between rows inside one host card.
const HOST_CARD_SPACING: f32 = 4.0;

/// Header button that opens the Hosts modal; its dots mirror every host's
/// connection state so a lost connection is visible without opening it.
pub(crate) fn hosts_button(app: &PohunekApp) -> Element<'_, Message> {
    let mut strip = row![].spacing(HOST_STRIP_SPACING).align_y(Center);
    for host in app.workspace.hosts.values() {
        strip = strip.push(conn_dot(host.conn.clone()));
    }
    button(
        row![text("Hosts").size(14), strip]
            .spacing(8)
            .align_y(Center),
    )
    .padding([8, 10])
    .on_press(Message::OpenHostsModal)
    .style(iced::widget::button::secondary)
    .into()
}

/// Hosts modal: connection state, last error and governance for every host.
pub(crate) fn hosts_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let mut hosts = column![].spacing(12);
    for (host_id, host) in &app.workspace.hosts {
        hosts = hosts.push(host_card(host_id, host));
    }
    if app.workspace.hosts.is_empty() {
        hosts = hosts.push(text("No hosts connected yet.").size(13).style(muted_style));
    }
    dialog_card("Hosts", scrollable(hosts))
}

fn host_card<'a>(host_id: &'a HostId, host: &'a HostView) -> Element<'a, Message> {
    let mut rows = column![row![
        conn_dot(host.conn.clone()),
        text(host_id.to_string()).size(15)
    ]
    .spacing(6)
    .align_y(Center)]
    .spacing(HOST_CARD_SPACING);
    if let Some(error) = &host.last_error {
        rows = rows.push(text(error).size(12));
    }
    for line in governance_rows(&host.governance) {
        rows = rows.push(text(line).size(12).style(muted_style));
    }
    card(rows)
}

/// Render public read-only governance state. All fetching and state reduction
/// stay in `gui-core` and the command layer.
fn governance_rows(governance: &GovernanceState) -> Vec<String> {
    match governance {
        GovernanceState::NotLoaded => vec!["Governance: not loaded".to_owned()],
        GovernanceState::Loading { .. } => vec!["Governance: loading…".to_owned()],
        GovernanceState::Error(error) => vec![format!("Governance error: {error}")],
        GovernanceState::Loaded(status) => {
            let mut rows = vec![
                format!("Stable host ID: {}", status.host_id()),
                format!("Approval key: {}", status.approval_key_reference()),
            ];
            let Some(enrollment) = status.enrollment() else {
                rows.push("Enrollment: never enrolled".to_owned());
                return rows;
            };
            rows.push(format!(
                "Enrollment: {:?} via {} (revision {})",
                enrollment.status(),
                enrollment.relay_id(),
                enrollment.revision()
            ));
            if let (Some(owner), Some(revision)) = (status.owner(), status.owner_revision()) {
                let owner = match owner {
                    HostOwner::Principal(id) => format!("principal {id}"),
                    HostOwner::Team(id) => format!("team {id}"),
                };
                rows.push(format!("Owner: {owner} (revision {revision})"));
            }
            if let Some(reason) = status.quarantine() {
                rows.push(format!("Quarantine: {reason:?}"));
            }
            rows
        }
    }
}

pub(crate) fn conn_color(theme: &Theme, conn: &ConnState) -> iced::Color {
    let palette = theme.extended_palette();
    match conn {
        ConnState::Connected => palette.success.base.color,
        ConnState::Connecting => palette.warning.base.color,
        ConnState::Disconnected | ConnState::Unreachable => palette.danger.base.color,
    }
}

#[cfg(test)]
mod tests {
    use pohunek_gui_core::Workspace;
    use protocol::{
        ApprovalKeyReference, EnrollmentInfo, EnrollmentRevision, EnrollmentStatus,
        HostGovernanceStatus, HostId, OwnerRevision, PrincipalId, QuarantineReason, RelayId,
        TeamId,
    };

    use super::*;

    fn status(
        owner: Option<HostOwner>,
        quarantine: Option<QuarantineReason>,
    ) -> HostGovernanceStatus {
        let owner_revision = owner
            .as_ref()
            .map(|_| OwnerRevision::new(3).expect("nonzero owner revision"));
        HostGovernanceStatus::new(
            HostId::parse("host_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .expect("valid host id"),
            owner.as_ref().map(|_| {
                EnrollmentInfo::new(
                    RelayId::parse("relay_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                        .expect("valid relay id"),
                    if quarantine.is_some() {
                        EnrollmentStatus::Quarantined
                    } else {
                        EnrollmentStatus::Active
                    },
                    EnrollmentRevision::new(2).expect("nonzero enrollment revision"),
                )
            }),
            owner,
            owner_revision,
            quarantine,
            ApprovalKeyReference::from_ed25519_verifying_key_bytes([7; 32]),
        )
        .expect("valid governance status")
    }

    #[test]
    fn governance_rows_cover_every_public_presentation_branch() {
        assert_eq!(
            governance_rows(&GovernanceState::NotLoaded),
            ["Governance: not loaded"]
        );
        let route = pohunek_gui_core::HostId::new("local");
        let mut workspace = Workspace::default();
        workspace
            .begin_governance_request(route.clone())
            .expect("request id");
        assert_eq!(
            governance_rows(workspace.governance(&route).expect("loading state")),
            ["Governance: loading…"]
        );
        assert_eq!(
            governance_rows(&GovernanceState::Error("offline".to_owned())),
            ["Governance error: offline"]
        );
        assert_eq!(
            governance_rows(&GovernanceState::Loaded(status(None, None)))[2],
            "Enrollment: never enrolled"
        );

        let principal = HostOwner::Principal(
            PrincipalId::parse("principal_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .expect("valid principal id"),
        );
        assert!(
            governance_rows(&GovernanceState::Loaded(status(Some(principal), None)))
                .iter()
                .any(|row| row.starts_with("Owner: principal "))
        );
        let team = HostOwner::Team(
            TeamId::parse("team_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .expect("valid team id"),
        );
        assert!(
            governance_rows(&GovernanceState::Loaded(status(Some(team), None)))
                .iter()
                .any(|row| row.starts_with("Owner: team "))
        );
        let rows = governance_rows(&GovernanceState::Loaded(status(
            Some(HostOwner::Principal(
                PrincipalId::parse("principal_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                    .expect("valid principal id"),
            )),
            Some(QuarantineReason::ProjectionConflict),
        )));
        assert!(rows
            .iter()
            .any(|row| row == "Quarantine: ProjectionConflict"));
        assert!(rows
            .iter()
            .all(|row| !["seed", "nonce", "signature", "private key"]
                .iter()
                .any(|forbidden| row.contains(forbidden))));
    }
}

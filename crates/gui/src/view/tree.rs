//! Host and project navigation for the native GUI.

// Rust guideline compliant 2026-08-12

use std::collections::BTreeSet;

use iced::widget::{button, column, row, scrollable, text};
use iced::{Center, Element, Fill, Theme};
use pohunek_gui_core::{ConnState, GovernanceState, HostId, TreeNodeId};
use protocol::{HostOwner, ProjectInfo};

use crate::message::{Message, ModalView};
use crate::selection::project_is_selected;
use crate::PohunekApp;

use super::{caret, conn_dot, indent, list_button};

pub(crate) fn inbox_entry_button(app: &PohunekApp) -> Element<'_, Message> {
    let button = button(text("Activity").size(14))
        .width(Fill)
        .padding([8, 10])
        .on_press(Message::OpenInbox);
    if app.modal == ModalView::Inbox {
        button.style(iced::widget::button::primary).into()
    } else {
        button.style(iced::widget::button::secondary).into()
    }
}

pub(crate) fn assistant_entry_button() -> Element<'static, Message> {
    button(
        row![text("◎").size(14), text("Assistant").size(14)]
            .spacing(6)
            .align_y(Center),
    )
    .width(Fill)
    .padding([8, 10])
    .on_press(Message::OpenAssistantModal)
    .style(iced::widget::button::primary)
    .into()
}

pub(crate) fn workspace_tree(app: &PohunekApp) -> Element<'_, Message> {
    let mut tree = column![text("Projects").size(16)].spacing(4);
    if let Err(err) = &app.config {
        tree = tree.push(text(format!("configuration error: {err}")).size(14));
        return scrollable(tree).into();
    }
    for (host_id, host) in &app.workspace.hosts {
        let node = TreeNodeId::host(host_id.clone());
        let expanded = app.ui_state.expanded_nodes.contains(&node);
        let host_row = row![
            caret(expanded, node),
            conn_dot(host.conn.clone()),
            text(host_id.to_string()).size(15)
        ]
        .spacing(6)
        .align_y(Center);
        tree = tree.push(host_row);
        if let Some(error) = &host.last_error {
            tree = tree.push(indent(1, text(error).size(12)));
        }
        if expanded {
            tree = push_project_rows(tree, app, host_id, host);
        }
    }
    if app.workspace.hosts.is_empty() {
        tree = tree.push(text("connecting…").size(13));
    }
    scrollable(tree).into()
}

fn push_project_rows<'a>(
    mut tree: iced::widget::Column<'a, Message>,
    app: &'a PohunekApp,
    host_id: &'a HostId,
    host: &'a pohunek_gui_core::HostView,
) -> iced::widget::Column<'a, Message> {
    tree = push_governance_rows(tree, host);
    for project in host.projects.values() {
        tree = tree.push(project_row(app, host_id, project));
    }
    let missing_project_ids = host
        .sessions
        .values()
        .filter_map(|session| {
            let project_id = session.project_id.as_ref()?;
            (!host.projects.contains_key(project_id)).then(|| project_id.clone())
        })
        .collect::<BTreeSet<_>>();
    for project_id in missing_project_ids {
        tree = tree.push(missing_project_row(app, host_id, &project_id));
    }
    tree
}

/// Render public read-only governance state. All fetching and state reduction
/// stay in `gui-core` and the command layer.
fn push_governance_rows<'a>(
    mut tree: iced::widget::Column<'a, Message>,
    host: &'a pohunek_gui_core::HostView,
) -> iced::widget::Column<'a, Message> {
    for row in governance_rows(&host.governance) {
        tree = tree.push(indent(1, text(row).size(12)));
    }
    tree
}

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

fn project_row(
    app: &PohunekApp,
    host_id: &HostId,
    project: &ProjectInfo,
) -> Element<'static, Message> {
    indent(
        1,
        list_button(
            text(project.label.clone()).size(14),
            Message::SelectProject {
                host_id: host_id.clone(),
                project_id: project.id.clone(),
            },
            project_is_selected(app, host_id, &project.id),
        ),
    )
}

fn missing_project_row(
    app: &PohunekApp,
    host_id: &HostId,
    project_id: &str,
) -> Element<'static, Message> {
    indent(
        1,
        list_button(
            text(format!("Unknown project {project_id}")).size(14),
            Message::SelectProject {
                host_id: host_id.clone(),
                project_id: project_id.to_owned(),
            },
            project_is_selected(app, host_id, project_id),
        ),
    )
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

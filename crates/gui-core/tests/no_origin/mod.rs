//! Loopback-harness entry points that send no request origin.
//!
//! Each function has the signature of the same-named `pohunek_gui_core`
//! function but connects with [`super::test_connection_options`], so a harness
//! run never depends on the `POHUNEK_*` variables of the developer's session.

// Rust guideline compliant 2026-10-01

use pohunek_gui_core::{CoreError, HostConfig, HostSnapshot};
use protocol::{
    HostGovernanceStatus, ProjectActionParams, ProjectActionResult, ProjectActionsParams,
    ProjectActionsResult, ProjectAddParams, ProjectInfo, ProjectPromptParams, ProjectPromptResult,
    ProjectRemoveParams, ProjectRemoveResult, ProjectRenameParams, ProjectShowParams,
    ProjectShowResult, SessionDiffParams, SessionDiffResult, SessionId, SessionInfo,
    SessionNewParams, SessionNewResult, SessionOutputParams, SessionOutputResult,
    SessionScreenParams, SessionScreenResult, SessionSetMetadataParams, SessionSetMetadataResult,
    SessionStopResult, SessionWaitParams, SessionWaitResult,
};

use super::test_connection_options;

/// Defines `$name(config, args..)` as `pohunek_gui_core::$with(config, args..,
/// test_connection_options())`.
macro_rules! with_test_options {
    ($($name:ident => $with:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        $(
            pub(crate) async fn $name(
                config: &HostConfig,
                $($arg: $ty),*
            ) -> Result<$ret, CoreError> {
                pohunek_gui_core::$with(config, $($arg,)* test_connection_options()).await
            }
        )*
    };
}

with_test_options! {
    load_host_snapshot => load_host_snapshot_with_options() -> HostSnapshot;
    inspect_host_governance => inspect_host_governance_with_options() -> HostGovernanceStatus;
    inspect_session => inspect_session_with_options(session_id: &SessionId) -> SessionInfo;
    create_session => create_session_with_options(params: SessionNewParams) -> SessionNewResult;
    diff_session => diff_session_with_options(params: SessionDiffParams) -> SessionDiffResult;
    stop_session => stop_session_with_options(session_id: &SessionId) -> SessionStopResult;
    wait_for_session => wait_for_session_with_options(params: SessionWaitParams) -> SessionWaitResult;
    read_session_output => read_session_output_with_options(params: SessionOutputParams) -> SessionOutputResult;
    read_session_screen => read_session_screen_with_options(params: SessionScreenParams) -> SessionScreenResult;
    set_session_metadata => set_session_metadata_with_options(params: SessionSetMetadataParams) -> SessionSetMetadataResult;
    list_projects => list_projects_with_options() -> Vec<ProjectInfo>;
    add_project => add_project_with_options(params: ProjectAddParams) -> ProjectInfo;
    rename_project => rename_project_with_options(params: ProjectRenameParams) -> ProjectInfo;
    show_project => show_project_with_options(params: ProjectShowParams) -> ProjectShowResult;
    remove_project => remove_project_with_options(params: ProjectRemoveParams) -> ProjectRemoveResult;
    list_project_actions => list_project_actions_with_options(params: ProjectActionsParams) -> ProjectActionsResult;
    resolve_project_prompt => resolve_project_prompt_with_options(params: ProjectPromptParams) -> ProjectPromptResult;
    resolve_project_action => resolve_project_action_with_options(params: ProjectActionParams) -> ProjectActionResult;
}

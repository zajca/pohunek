//! The integration tests of `pohunek-cli` in one test binary, one module per
//! file. One binary links the dependency graph once instead of once per file.
//! `release_consumer.rs` and `release_upgrade.rs` stay test targets of their
//! own: the release evidence workflow and `scripts/upgrade-test` build and run
//! those binaries on their own.

#![expect(
    clippy::duplicate_mod,
    reason = "each test module declares the support modules it uses with its own cfg and \
              lint attributes, so a test file compiles the same code here as on its own"
)]

#[path = "agent_skill.rs"]
mod agent_skill;
#[path = "assistant.rs"]
mod assistant;
#[path = "attach_stream_input.rs"]
mod attach_stream_input;
#[path = "claude_package.rs"]
mod claude_package;
#[path = "codex_package.rs"]
mod codex_package;
#[path = "completion_dynamic.rs"]
mod completion_dynamic;
#[path = "daemon_packaging.rs"]
mod daemon_packaging;
#[path = "doctor_worker_compat.rs"]
mod doctor_worker_compat;
#[path = "hermes_integration.rs"]
mod hermes_integration;
#[path = "hermes_package.rs"]
mod hermes_package;
#[path = "hermes_plugin_argv.rs"]
mod hermes_plugin_argv;
#[path = "hermes_plugin_suites.rs"]
mod hermes_plugin_suites;
#[path = "host_command.rs"]
mod host_command;
#[path = "host_fanout_local_record.rs"]
mod host_fanout_local_record;
#[path = "host_pin_assistant.rs"]
mod host_pin_assistant;
#[path = "host_pin_attach.rs"]
mod host_pin_attach;
#[path = "host_pin_session.rs"]
mod host_pin_session;
#[path = "integration_hooks.rs"]
mod integration_hooks;
#[path = "json_usage_errors.rs"]
mod json_usage_errors;
#[path = "newer_daemon.rs"]
mod newer_daemon;
#[path = "notifications_list_paging.rs"]
mod notifications_list_paging;
#[path = "notifications_policy.rs"]
mod notifications_policy;
#[path = "notifications_watch.rs"]
mod notifications_watch;
#[path = "overlay_config.rs"]
mod overlay_config;
#[path = "overlay_diagnostics.rs"]
mod overlay_diagnostics;
#[path = "overlay_service_daemon_port.rs"]
mod overlay_service_daemon_port;
#[path = "overlay_tailscale.rs"]
mod overlay_tailscale;
#[path = "overlay_tailscale_daemon.rs"]
mod overlay_tailscale_daemon;
#[path = "pi_frame_properties.rs"]
mod pi_frame_properties;
#[path = "pi_package.rs"]
mod pi_package;
#[path = "plugin_catalog_install.rs"]
mod plugin_catalog_install;
#[path = "plugin_cli.rs"]
mod plugin_cli;
#[path = "plugin_daemon.rs"]
mod plugin_daemon;
#[path = "prompt_link.rs"]
mod prompt_link;
#[path = "prompt_render.rs"]
mod prompt_render;
#[path = "service_config_migration.rs"]
mod service_config_migration;
#[path = "service_launchd.rs"]
mod service_launchd;
#[path = "service_lock.rs"]
mod service_lock;
#[path = "service_lock_terminal.rs"]
mod service_lock_terminal;
#[path = "service_preflight.rs"]
mod service_preflight;
#[path = "service_systemd.rs"]
mod service_systemd;
#[path = "service_upgrade.rs"]
mod service_upgrade;
#[path = "session_background.rs"]
mod session_background;
#[path = "session_background_work.rs"]
mod session_background_work;
#[path = "session_idle_stop.rs"]
mod session_idle_stop;
#[path = "session_lifecycle_notice.rs"]
mod session_lifecycle_notice;
#[path = "session_metadata_filters.rs"]
mod session_metadata_filters;
#[path = "session_new_cwd.rs"]
mod session_new_cwd;
#[path = "session_new_input_ack.rs"]
mod session_new_input_ack;
#[path = "session_new_outcome.rs"]
mod session_new_outcome;
#[path = "session_process_api.rs"]
mod session_process_api;
#[path = "session_wait_turn.rs"]
mod session_wait_turn;
#[path = "setup_host.rs"]
mod setup_host;
#[path = "setup_host_native.rs"]
mod setup_host_native;
#[path = "standalone_discovery.rs"]
mod standalone_discovery;
#[path = "store_cli.rs"]
mod store_cli;
#[path = "workflow_launchd.rs"]
mod workflow_launchd;
#[path = "workflow_reactions.rs"]
mod workflow_reactions;
#[path = "workflow_run.rs"]
mod workflow_run;
#[path = "workflow_run_json.rs"]
mod workflow_run_json;
#[path = "workflow_store.rs"]
mod workflow_store;
#[path = "workflow_systemd.rs"]
mod workflow_systemd;

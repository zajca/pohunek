# Cross-Host Agent Notifications Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a complete durable notification system for Codex, Claude, and Pohunek-owned agent events, with per-host storage, cross-host aggregation, SDK helpers, CLI commands, GUI Inbox, provider hook adapters, retention, and documentation.

**Architecture:** Each daemon remains authoritative for notifications produced on its own host. The control protocol gains additive `notification.*` methods and notification events; the daemon stores notification records in an owner-private local store and publishes updates to subscriptions; the SDK, CLI, and GUI aggregate across configured hosts without introducing a central server. Provider hooks sanitize Codex and Claude events before creating local daemon notifications, while daemon-side projectors derive durable notifications from internal agent state and session lifecycle transitions. A source-independent dedupe key prevents projector-derived notifications and provider-hook notifications from producing two records for the same logical attention event.

**Tech Stack:** Rust 2021, Tokio, serde, serde_json, newline-delimited JSON protocol, clap, Iced, shell hook assets, existing NetBird/direct host transport, existing docs/knowledge bundle tooling.

---

## Product Decisions

- This is a full production feature, not a proof of concept and not a reduced first version.
- Cross-host aggregation is client-side. Do not add a central notification server.
- Supported notification kinds: `agent_blocked`, `approval_required`, `turn_completed`, `session_finished`, `error`, and `system`.
- Supported notification lifecycle: `unread`, `read`, `acknowledged`, `archived`, and `deleted`.
- `turn_completed` is fully implemented but disabled by default in notification policy because it is noisy.
- Default enabled kinds are `agent_blocked`, `approval_required`, and `error`.
- Retention is implemented as a policy with explicit config and manual cleanup support. The daemon must not silently delete notifications through an undocumented hardcoded default.
- Provider hooks have precedence over daemon projectors for the same session attention event. If a provider hook and the screen/session projector both report that one session needs attention within the configured dedupe window, the hook-created notification wins and the projector record is suppressed or upgraded into the provider-backed record.
- Codex notification integration must use lifecycle hooks (`PermissionRequest` and `Stop`) discovered from `hooks.json` or inline `[hooks]` config. Do not build the Codex approval path on `notify`, because `notify` currently only supports `agent-turn-complete`.
- Claude notification integration must map `Notification` by matcher/type, not as one blanket event. Known mappings are `permission_prompt` and `elicitation_dialog` to `approval_required`, `idle_prompt` to `agent_blocked`, `auth_success`, `elicitation_complete`, and `elicitation_response` to `system`, `Stop` to `turn_completed`, and `StopFailure` to `error`.
- Notification payloads must not store raw terminal output, prompts, secrets, environment dumps, or full tool results.
- All new repository files, code comments, docs, and config text must be written in English.

## Provider Hook Surface Requirements

- Pohunek targets the current bleeding-edge Codex and Claude hook surfaces.
- Do not add fallback shims for old Codex or Claude versions.
- Codex notification support requires lifecycle hooks including `PermissionRequest` and `Stop`.
- Claude notification support requires hook events including `Notification` matcher values, `Stop`, and `StopFailure`.
- `integration.install` must fail fast with a clear message when the target provider config cannot support the required hook surface.
- The current codebase stores Codex hooks in `~/.codex/hooks.json` and records trust metadata in `config.toml`; official Codex docs also allow inline `[hooks]` tables. Keep the existing `hooks.json` installer representation unless there is a concrete reason to migrate the whole installer.

## File Map

- Modify `crates/protocol/src/lib.rs`: export notification module, method constants, and event constants.
- Create `crates/protocol/src/notification.rs`: protocol types for records, filters, policies, retention, requests, responses, and events.
- Modify `crates/protocol/tests/roundtrip.rs`: protocol serialization coverage.
- Create `crates/daemon/src/notifications/mod.rs`: daemon notification service public API.
- Create `crates/daemon/src/notifications/store.rs`: append-only durable store and replay logic.
- Create `crates/daemon/src/notifications/policy.rs`: notification policy and retention evaluation.
- Create `crates/daemon/src/notifications/projector.rs`: derived notifications from daemon session events.
- Modify `crates/daemon/src/api/handler.rs`: dispatch `notification.*` requests.
- Modify `crates/daemon/src/api/mod.rs`: merge notification events into `serve_connection` / `run_event_subscription`.
- Modify `crates/daemon/src/main.rs`: initialize notification service and projector.
- Modify `crates/daemon/src/events/mod.rs`: persist notification control events if the event log records all structured events.
- Modify `crates/daemon/src/integration/mod.rs`: install notification hook assets with provider integrations.
- Create `crates/daemon/src/integration/assets/codex/pohunek-agent-notify.sh`: Codex hook adapter.
- Create `crates/daemon/src/integration/assets/claude/pohunek-agent-notify.sh`: Claude hook adapter.
- Modify `crates/client/src/lib.rs`: export notification SDK helpers.
- Modify `crates/client/src/transport.rs`: add typed subscription event helper if it fits the existing transport boundary.
- Create `crates/client/src/notifications.rs`: typed notification SDK methods.
- Modify `crates/client/tests/request_response.rs`: SDK request helper coverage.
- Modify `crates/client/tests/subscription.rs`: typed event helper coverage.
- Modify `crates/cli/src/lib.rs`: add `notifications` command.
- Modify `crates/cli/src/commands/mod.rs`: register command module.
- Create `crates/cli/src/commands/host_fanout.rs`: shared CLI fan-out helpers for commands that need to query multiple configured/discovered hosts.
- Create `crates/cli/src/commands/notifications.rs`: list, watch, ack, read, archive, delete, policy, and retention commands.
- Modify `crates/cli/tests/json_usage_errors.rs`: command usage and JSON error coverage.
- Modify or create CLI rendering tests according to existing CLI test patterns.
- Modify `crates/gui-core/src/lib.rs`: Inbox state, events, effects, host snapshots, filters, and acknowledgements.
- Modify `crates/gui-core/tests/loopback.rs`: cross-host notification behavior.
- Modify `crates/gui/src/main.rs`: Inbox UI, unread counts, actions, and OS notification deduplication.
- Modify `crates/gui/src/runtime.rs`: runtime calls for notification list/update operations if the GUI shell owns those tasks.
- Modify `docs/public-api.md`: protocol, SDK, CLI, and event contract.
- Modify `docs/knowledge/guides/gui.md`: GUI Inbox behavior.
- Modify `docs/knowledge/guides/remote-hosts.md`: cross-host notification aggregation.
- Modify `docs/knowledge/concepts/sessions.md`: session-linked notifications.
- Modify `docs/knowledge/runbooks/debug-daemon.md`: debugging notification store, hooks, and subscriptions.
- Modify `docs/knowledge/assistant/source-map.md`: source map entries for new files.
- Modify `docs/knowledge/log.md`: knowledge bundle change log.

## Task 0: Required Orientation

**Files:**
- Read `.agents/rust-guidelines/SKILL.md`
- Read `.agents/rust-guidelines/11_universal_guidelines.md`
- Read `.agents/rust-guidelines/02_application_guidelines.md`
- Read `.agents/rust-guidelines/03_correctness_guidelines.md`
- Read `.agents/rust-guidelines/06_library_guidelines.md`
- Read `.agents/rust-guidelines/14_libraries_resilience_guidelines.md`
- Read `.agents/rust-guidelines/15_libraries_ux_guidelines.md`
- Read `docs/architecture.md`
- Read `docs/public-api.md`

- [ ] Read the Rust guideline index and the listed guideline files before modifying any `.rs` file.
- [ ] Confirm that the daemon remains single-user, per-host authoritative, and direct over local socket or NetBird TCP.
- [ ] Confirm that provider integrations stay in client-facing surfaces and hook assets; do not move GitHub, Linear, Codex, or Claude provider responsibilities into the daemon beyond local hook ingestion.
- [ ] Run `rtk git status --short`.
- [ ] Expected: either a clean tree or only user-owned changes that are unrelated to this feature.

## Task 1: Protocol Notification Contract

**Files:**
- Create `crates/protocol/src/notification.rs`
- Modify `crates/protocol/src/lib.rs`
- Modify `crates/protocol/tests/roundtrip.rs`

- [ ] Add failing roundtrip tests for `NotificationKind` snake_case values: `agent_blocked`, `approval_required`, `turn_completed`, `session_finished`, `error`, and `system`.
- [ ] Add failing roundtrip tests for `NotificationSeverity` snake_case values: `info`, `success`, `warning`, `error`, and `action_required`.
- [ ] Add failing roundtrip tests for `NotificationStatus` snake_case values: `unread`, `read`, `acknowledged`, `archived`, and `deleted`.
- [ ] Add a failing test for `NotificationRecord` serialization with required fields `id`, `source`, `kind`, `severity`, `status`, `title`, `body`, and `created_at`.
- [ ] Add a failing test proving optional fields such as `session_id`, `agent_kind`, `source_id`, `dedupe_key`, `project_id`, `read_at`, `acked_at`, `archived_at`, `deleted_at`, and `superseded_by` are omitted when `None`.
- [ ] Add failing tests for `NotificationCreateParams`, `NotificationListParams`, `NotificationUpdateParams`, `NotificationDeleteParams`, `NotificationPolicyParams`, and `NotificationRetentionParams`.
- [ ] Add a failing test proving `NotificationCreateParams` can carry a source-independent `dedupe_key` for cross-producer dedupe.
- [ ] Add a failing test proving notification policy can carry `attention_dedupe_window_secs`.
- [ ] Add failing event tests for `event::NOTIFICATION_CREATED`, `event::NOTIFICATION_UPDATED`, and `event::NOTIFICATION_DELETED`.
- [ ] Run `rtk cargo test -p pohunek-protocol --test roundtrip notification`.
- [ ] Expected: tests fail because notification protocol types and constants do not exist.
- [ ] Implement `NotificationId` as a transparent string wrapper that follows the existing ID wrapper style in the protocol crate.
- [ ] Implement enums with `#[serde(rename_all = "snake_case")]`.
- [ ] Implement `NotificationSource { provider, provider_event, host_local_source_id }` with redacting `Debug` if any source payload can contain sensitive values.
- [ ] Implement `NotificationRecord` with optional fields using `#[serde(default, skip_serializing_if = "Option::is_none")]`.
- [ ] Implement request and response structs for create, list, update/read/ack/archive, delete, policy get/set, and retention prune.
- [ ] Include `dedupe_key` on create params and stored records so provider hooks and daemon projectors can intentionally refer to the same logical event without sharing a producer-specific `source_id`.
- [ ] Include `superseded_by` on stored records so an implementation can preserve audit history when a lower-priority projector record is upgraded to a provider-backed notification.
- [ ] Add method constants: `notification.create`, `notification.list`, `notification.update`, `notification.delete`, `notification.policy.get`, `notification.policy.set`, and `notification.retention.prune`.
- [ ] Add event constants: `notification_created`, `notification_updated`, and `notification_deleted`.
- [ ] Export the notification module and public types from `crates/protocol/src/lib.rs`.
- [ ] Run `rtk cargo test -p pohunek-protocol --test roundtrip notification`.
- [ ] Expected: all notification protocol roundtrip tests pass.

## Task 2: Daemon Notification Policy And Store

**Files:**
- Create `crates/daemon/src/notifications/mod.rs`
- Create `crates/daemon/src/notifications/store.rs`
- Create `crates/daemon/src/notifications/policy.rs`
- Modify `crates/daemon/src/main.rs`
- Test notification modules inline with `#[cfg(test)]`

- [ ] Add failing store tests for creating the notifications directory with owner-private permissions.
- [ ] Add failing store tests for append-only create, update, archive, delete, and replay after reopening the store.
- [ ] Add a failing test for idempotent create when `source_id` is present for the same source.
- [ ] Add a failing test for preserving two records with the same `source_id` when the provider/source namespace differs.
- [ ] Add a failing cross-producer dedupe test where a projector-created `agent_blocked` notification is upgraded when a provider hook creates `approval_required` for the same session and `dedupe_key` inside the configured window.
- [ ] Add a failing cross-producer dedupe test where a later projector-created `agent_blocked` notification is suppressed because a provider-backed `approval_required` notification already exists for the same `dedupe_key`.
- [ ] Add a failing cross-producer dedupe test proving records outside the configured dedupe window are preserved as separate notifications.
- [ ] Add failing list tests for filters by status, kind, severity, provider, session, and time range.
- [ ] Add failing cursor tests proving stable ordering by `created_at desc`, then `id`.
- [ ] Add failing tests for rejecting secret-like metadata keys: `token`, `secret`, `password`, `api_key`, `authorization`, and `cookie`.
- [ ] Add failing tests for title/body length normalization using named constants with rationale comments.
- [ ] Add failing policy tests proving default enabled kinds are `agent_blocked`, `approval_required`, and `error`, while `turn_completed` is disabled.
- [ ] Add failing policy tests for provider-specific overrides for Codex and Claude.
- [ ] Add failing policy tests for configuring the attention dedupe window used by provider/projector cross-producer dedupe.
- [ ] Add failing retention tests for dry-run and apply modes.
- [ ] Run `rtk cargo test -p pohunek-daemon notifications`.
- [ ] Expected: tests fail because the notification modules do not exist.
- [ ] Implement `NotificationService` as the daemon-facing API that owns `NotificationStore`, `NotificationPolicy`, and a broadcast sender for notification events.
- [ ] Implement store replay from `<data_dir>/notifications/notifications.jsonl`.
- [ ] Implement action records for `created`, `updated`, and `deleted` so replay reconstructs current state.
- [ ] Implement logical delete by setting status `deleted` and `deleted_at`; keep the action log append-only.
- [ ] Implement `NotificationPolicy` with explicit defaults in one module and public methods for get/set/evaluate.
- [ ] Implement source priority for dedupe: provider hooks outrank daemon projectors; user-created/external records do not automatically supersede provider records.
- [ ] Implement `dedupe_key` lookup inside the configured attention window before assigning a new notification id.
- [ ] When a provider hook supersedes a projector record, update the existing visible record to the provider-backed kind/title/body/source and emit `notification_updated` rather than creating a second visible notification.
- [ ] When a projector record is suppressed by an existing provider record, return the existing record in the create result with `created: false`.
- [ ] Implement retention pruning as an explicit method that marks records as deleted according to policy.
- [ ] Ensure all user-controlled strings are length-bounded before storage.
- [ ] Ensure metadata is allowlisted or rejected before storage.
- [ ] Initialize `NotificationService` from `crates/daemon/src/main.rs`.
- [ ] Run `rtk cargo test -p pohunek-daemon notifications`.
- [ ] Expected: all notification store and policy tests pass.

## Task 3: Daemon API And Subscription Events

**Files:**
- Modify `crates/daemon/src/api/handler.rs`
- Modify the current subscribe handling file under `crates/daemon/src/api/`
- Modify `crates/daemon/src/events/mod.rs`
- Modify `crates/daemon/tests/health_socket.rs`

- [ ] Add failing handler tests or integration tests for `notification.create`, `notification.list`, `notification.update`, `notification.delete`, `notification.policy.get`, `notification.policy.set`, and `notification.retention.prune`.
- [ ] Add a failing test proving `notification.create` enriches a record with session context when `session_id` exists.
- [ ] Add a failing test proving `notification.create` still succeeds with a session reference when the session no longer exists.
- [ ] Add a failing subscription test proving a subscriber receives `notification_created` after `notification.create`.
- [ ] Add a failing subscription test proving a subscriber receives `notification_updated` after read/ack/archive.
- [ ] Add a failing subscription test proving a subscriber receives `notification_deleted` after delete.
- [ ] Add a failing event-log test if the event log is expected to persist all structured control-plane events.
- [ ] Run `rtk cargo test -p pohunek-daemon --test health_socket notification`.
- [ ] Expected: tests fail because API handlers and subscription merge do not exist.
- [ ] Extend `DaemonState` with `notifications: NotificationService`.
- [ ] Add request dispatch for all `notification.*` methods.
- [ ] Return typed protocol errors for malformed filters, invalid transitions, and missing records.
- [ ] Implement status transitions: `unread -> read`, `read -> acknowledged`, `unread -> acknowledged`, `unread/read/acknowledged -> archived`, and any non-deleted status -> deleted.
- [ ] Reject updates to `deleted` records except idempotent delete.
- [ ] Merge notification broadcast events into the existing `subscribe` stream without changing existing session event payloads.
- [ ] Ensure slow notification subscribers follow the same lag behavior as existing session subscribers.
- [ ] Persist notification control events through the event log if the event log is documented as a structured control-plane event log.
- [ ] Run `rtk cargo test -p pohunek-daemon --test health_socket notification`.
- [ ] Expected: notification API and subscription tests pass.

## Task 4: Derived Notifications From Agent State And Session Lifecycle

**Files:**
- Create `crates/daemon/src/notifications/projector.rs`
- Modify `crates/daemon/src/notifications/mod.rs`
- Modify `crates/daemon/src/main.rs`
- Test `crates/daemon/src/notifications/projector.rs`

- [ ] Add failing projector tests for creating `agent_blocked` when a session transitions into blocked activity.
- [ ] Add a failing projector test proving repeated blocked events for the same uninterrupted blocked period do not create duplicates.
- [ ] Add a failing projector test proving a provider-created `approval_required` notification suppresses or upgrades a projector-created `agent_blocked` notification for the same session attention dedupe key.
- [ ] Add failing projector tests for creating `error` when a lifecycle event carries `SessionInfo.state == Failed`.
- [ ] Add failing projector tests proving `error` includes the safe `exit_code` value when `SessionInfo.exit_code` is present.
- [ ] Add failing projector tests for creating `session_finished` when a lifecycle event carries `SessionInfo.state == Done` and the policy enables that kind.
- [ ] Add failing projector tests proving `SessionState::Stopped` caused by explicit user stop does not create `error`.
- [ ] Add failing projector tests proving disabled policy kinds do not create records.
- [ ] Add failing tests proving projector-created notifications have deterministic `source_id` values.
- [ ] Run `rtk cargo test -p pohunek-daemon notifications::projector`.
- [ ] Expected: tests fail because the projector does not exist.
- [ ] Implement `NotificationProjector` that consumes existing session event broadcasts and branches on event name plus the `SessionInfo` payload.
- [ ] Consume `agent_state` events for activity-derived `agent_blocked`.
- [ ] Consume `session_updated` events whose payload has `SessionInfo.state == Failed` for daemon-derived `error`.
- [ ] Consume `session_updated` events whose payload has `SessionInfo.state == Done` for policy-enabled `session_finished`.
- [ ] Treat `session_stopped` as an explicit user stop signal, not as an error.
- [ ] Generate deterministic source IDs from host-local session id, notification kind, and transition epoch.
- [ ] Generate the same attention `dedupe_key` for projector `agent_blocked` and provider-hook `approval_required` events that refer to the same session waiting-for-input condition.
- [ ] Create `agent_blocked`, `error`, and policy-enabled `session_finished` records through `NotificationService`.
- [ ] Do not derive `turn_completed` from PTY idleness; only provider hooks should create `turn_completed`.
- [ ] Start the projector in `crates/daemon/src/main.rs` after session registry and notification service initialization.
- [ ] Run `rtk cargo test -p pohunek-daemon notifications::projector`.
- [ ] Expected: projector tests pass.

## Task 5: Codex And Claude Hook Adapters

**Files:**
- Create `crates/daemon/src/integration/assets/codex/pohunek-agent-notify.sh`
- Create `crates/daemon/src/integration/assets/claude/pohunek-agent-notify.sh`
- Modify `crates/daemon/src/integration/mod.rs`
- Modify existing integration tests or add inline tests for rendered assets

- [ ] Add failing tests proving `integration.install` includes the Codex notification hook asset.
- [ ] Add failing tests proving `integration.install` includes the Claude notification hook asset.
- [ ] Add failing tests proving the Claude installer can merge command hooks for `Notification`, `Stop`, and `StopFailure`, not only `SessionStart`.
- [ ] Add failing tests proving the Codex installer can merge command hooks for `PermissionRequest` and `Stop`, not only `SessionStart`.
- [ ] Add failing tests proving the Codex installer does not rely on the legacy `notify` key for approval notifications.
- [ ] Add shell fixture tests or Rust asset-render tests for Claude `Notification` matcher/type `permission_prompt` mapping to `approval_required`.
- [ ] Add shell fixture tests or Rust asset-render tests for Claude `Notification` matcher/type `elicitation_dialog` mapping to `approval_required`.
- [ ] Add shell fixture tests or Rust asset-render tests for Claude `Notification` matcher/type `idle_prompt` mapping to `agent_blocked`.
- [ ] Add shell fixture tests or Rust asset-render tests for Claude `Notification` matcher/type `auth_success`, `elicitation_complete`, and `elicitation_response` mapping to `system`.
- [ ] Add shell fixture tests or Rust asset-render tests for Claude `Stop` input mapping to `turn_completed`.
- [ ] Add shell fixture tests or Rust asset-render tests for Claude `StopFailure` input mapping to `error`.
- [ ] Add shell fixture tests or Rust asset-render tests for Codex `PermissionRequest` input mapping to `approval_required`.
- [ ] Add shell fixture tests or Rust asset-render tests for Codex `Stop` input mapping to `turn_completed`.
- [ ] Add tests proving Codex hook configuration is written through the existing `hooks.json` representation and corresponding trust metadata, unless the implementation intentionally migrates the entire Codex installer to inline `[hooks]`.
- [ ] Add tests proving provider-hook records and projector records use compatible `dedupe_key` values for the same session attention event.
- [ ] Add tests proving hook payloads omit raw prompt, raw terminal output, environment variables, and full tool output.
- [ ] Add tests proving hook scripts exit successfully when the daemon socket is unavailable.
- [ ] Add documentation tests or fixture assertions proving the integration docs mention the required modern Codex and Claude hook support.
- [ ] Run `rtk cargo test -p pohunek-daemon integration`.
- [ ] Expected: tests fail because notification hook assets are not installed.
- [ ] Implement Codex notification hook script with strict shell mode where compatible with provider hook execution.
- [ ] Implement Claude notification hook script with the same sanitization behavior.
- [ ] Extend the existing integration installer so it can install multiple owned hook scripts and multiple event keys idempotently.
- [ ] Keep the existing integration asset install pattern; do not invent a separate top-level installer.
- [ ] For Codex, register notification hooks as lifecycle hooks for `PermissionRequest` and `Stop`; do not use `notify` for `approval_required`.
- [ ] For Claude, register notification hooks for `Notification`, `Stop`, and `StopFailure`.
- [ ] Preserve unrelated user hooks while stripping and replacing only hook handlers owned by Pohunek.
- [ ] Send notifications through the local daemon control socket or existing CLI helper used by current integration assets.
- [ ] Ensure hook scripts never print secrets or raw provider payloads.
- [ ] Ensure hook scripts return success when Pohunek is not running so agent sessions are not disrupted.
- [ ] Update integration installation to wire provider notification hooks consistently with current provider state hooks and to report all touched config files.
- [ ] Run `rtk cargo test -p pohunek-daemon integration`.
- [ ] Expected: integration tests pass.

## Task 6: Client SDK Helpers

**Files:**
- Create `crates/client/src/notifications.rs`
- Modify `crates/client/src/lib.rs`
- Modify `crates/client/src/transport.rs`
- Modify `crates/client/tests/request_response.rs`
- Modify `crates/client/tests/subscription.rs`

- [ ] Add failing client tests for `create_notification`, `list_notifications`, `update_notification`, `delete_notification`, `get_notification_policy`, `set_notification_policy`, and `prune_notifications`.
- [ ] Add a failing test for `Subscription::next_event()` decoding `notification_created`.
- [ ] Add a failing test for `Subscription::next_event()` returning a typed error on malformed JSON.
- [ ] Run `rtk cargo test -p pohunek-client notification`.
- [ ] Expected: tests fail because SDK notification helpers do not exist.
- [ ] Implement notification helper methods using existing `Client::request` behavior and protocol request/response types.
- [ ] Keep `Subscription::next_line()` as the compatibility API.
- [ ] Add `Subscription::next_event()` as an additive typed helper.
- [ ] Export notification helpers from `crates/client/src/lib.rs`.
- [ ] Run `rtk cargo test -p pohunek-client notification`.
- [ ] Expected: client notification tests pass.

## Task 7: CLI Host Fan-Out And Notifications Command

**Files:**
- Create `crates/cli/src/commands/host_fanout.rs`
- Create `crates/cli/src/commands/notifications.rs`
- Modify `crates/cli/src/commands/mod.rs`
- Modify `crates/cli/src/lib.rs`
- Modify `crates/cli/tests/json_usage_errors.rs`
- Add or modify CLI tests following the existing command test layout

- [ ] Add failing tests for a CLI host fan-out helper that turns local-only execution into one target and `--all-hosts` execution into local plus discovered reachable hosts.
- [ ] Add failing tests proving host fan-out preserves per-host successes when another host fails.
- [ ] Add failing tests proving host fan-out includes host id, transport target, and error details in a stable result shape.
- [ ] Add failing clap tests for `pohunek notifications list`, `watch`, `read`, `ack`, `archive`, `delete`, `policy get`, `policy set`, and `retention prune`.
- [ ] Add failing tests for rejecting `--host` together with `--all-hosts` where the combination is ambiguous.
- [ ] Add failing tests for parsing notification targets in bare `id` and `host/id` forms.
- [ ] Add failing human-render tests for columns `HOST`, `STATUS`, `SEVERITY`, `AGE`, `SESSION`, `KIND`, and `TITLE`.
- [ ] Add failing JSON-render tests proving host id is included when listing across hosts.
- [ ] Add failing watch-render tests for `notification_created`, `notification_updated`, and `notification_deleted`.
- [ ] Add failing policy command tests for enabling/disabling `turn_completed`.
- [ ] Add failing retention dry-run and apply command tests.
- [ ] Run `rtk cargo test -p pohunek-cli notifications`.
- [ ] Expected: tests fail because the CLI fan-out helper and notifications command do not exist.
- [ ] Implement `host_fanout` as a reusable CLI-only helper. It should use the local daemon's `host.discover` output when `--all-hosts` is selected and should not move GUI-core runtime types into the CLI command layer.
- [ ] Implement fan-out concurrency with bounded task count and deterministic output ordering by host id.
- [ ] Ensure fan-out reports per-host errors in human and JSON output without hiding successful host results.
- [ ] Add `Notifications` to the top-level command enum.
- [ ] Implement `notifications list` with filters for host, all-hosts, unread, status, kind, severity, agent/provider, session, limit, cursor, and JSON output.
- [ ] Implement `notifications watch` using existing subscribe transport and typed event decoding.
- [ ] Implement `notifications read`, `ack`, `archive`, and `delete` with target parsing for local and cross-host records.
- [ ] Implement `notifications policy get` and `notifications policy set` with explicit provider/kind toggles.
- [ ] Implement `notifications retention prune --dry-run` and `notifications retention prune --apply`.
- [ ] Use `host_fanout` for `--all-hosts` list/watch/policy/retention operations instead of embedding one-off fan-out logic in each subcommand.
- [ ] Run `rtk cargo test -p pohunek-cli notifications`.
- [ ] Expected: CLI notification tests pass.

## Task 8: GUI-Core Inbox State

**Files:**
- Modify `crates/gui-core/src/lib.rs`
- Modify `crates/gui-core/tests/loopback.rs`

- [ ] Add failing tests proving `HostSnapshot` can carry notification records.
- [ ] Add failing tests proving `HostEvent::NotificationCreated`, `HostEvent::NotificationUpdated`, and `HostEvent::NotificationDeleted` parse from subscription messages.
- [ ] Add failing tests proving the workspace stores notification records per host.
- [ ] Add failing tests proving global unread count and per-host unread count are updated after create/read/ack/archive/delete.
- [ ] Add failing tests proving reconnect reconciliation loads missed notifications through `notification.list`.
- [ ] Add failing tests proving selecting a linked notification can select the linked session when that session exists.
- [ ] Add failing tests proving selecting a notification whose session no longer exists still opens notification detail.
- [ ] Add failing tests proving OS notification intents are generated from durable notification events and do not duplicate blocked-session transient intents.
- [ ] Run `rtk cargo test -p pohunek-gui-core notification`.
- [ ] Expected: tests fail because Inbox state does not exist.
- [ ] Add notification storage to `HostView`.
- [ ] Add notification data to host snapshots and snapshot application logic.
- [ ] Extend `HostEvent` and `parse_event_message` for notification events.
- [ ] Add workspace selectors for unread counts, filtered notification lists, and selected notification detail.
- [ ] Add effects for notification update actions.
- [ ] Add reconnect reconciliation by requesting recent unread and recent archived/read records according to GUI policy.
- [ ] Route durable action-required/error notification events into `NotificationIntent`.
- [ ] Remove or deduplicate the existing blocked-session transient OS notification path.
- [ ] Run `rtk cargo test -p pohunek-gui-core notification`.
- [ ] Expected: GUI-core notification tests pass.

## Task 9: GUI Inbox UI

**Files:**
- Modify `crates/gui/src/main.rs`
- Modify `crates/gui/src/runtime.rs`

- [ ] Add failing compile-time coverage by introducing GUI-core calls first and running `rtk cargo check -p pohunek-gui`.
- [ ] Expected: compile fails because the GUI shell does not render or handle the new Inbox messages.
- [ ] Add an Inbox entry to the existing navigation with a global unread count.
- [ ] Add per-host unread counts where host rows are already displayed.
- [ ] Render a compact Inbox list with status, severity, title, host, session, kind, and age.
- [ ] Render notification detail with title, body, safe metadata, created time, status, source, and linked session action.
- [ ] Add actions for read, acknowledge, archive, delete, and open linked session.
- [ ] Add filters for status, severity, kind, provider, and host.
- [ ] Keep controls dense and consistent with existing GUI style; do not add marketing-style cards or nested cards.
- [ ] Wire GUI runtime tasks for list/update/delete/policy calls if those calls live outside gui-core.
- [ ] Ensure OS notifications are emitted once per durable notification event according to policy.
- [ ] Run `rtk cargo check -p pohunek-gui`.
- [ ] Expected: GUI compiles.
- [ ] Run `rtk cargo test -p pohunek-gui-core notification`.
- [ ] Expected: GUI-core notification tests still pass after GUI integration.

## Task 10: Documentation And Knowledge Bundle

**Files:**
- Modify `docs/public-api.md`
- Modify `docs/knowledge/guides/gui.md`
- Modify `docs/knowledge/guides/remote-hosts.md`
- Modify `docs/knowledge/concepts/sessions.md`
- Modify `docs/knowledge/runbooks/debug-daemon.md`
- Modify `docs/knowledge/assistant/source-map.md`
- Modify `docs/knowledge/log.md`
- Modify `README.md` if the notification CLI becomes part of the quick-start surface

- [ ] Document all `notification.*` methods in `docs/public-api.md`, including request params, response shapes, status transitions, and events.
- [ ] Document `dedupe_key`, provider/projector source priority, and the attention dedupe window in `docs/public-api.md`.
- [ ] Document SDK notification helpers in `docs/public-api.md`.
- [ ] Document CLI notification commands in `docs/public-api.md`.
- [ ] Document GUI Inbox behavior in `docs/knowledge/guides/gui.md`.
- [ ] Document cross-host aggregation behavior in `docs/knowledge/guides/remote-hosts.md`.
- [ ] Document session-linked notifications in `docs/knowledge/concepts/sessions.md`.
- [ ] Document daemon debugging steps for notification store, policy, hooks, dedupe, and subscription events in `docs/knowledge/runbooks/debug-daemon.md`.
- [ ] Document provider hook surface requirements: Codex lifecycle hooks for `PermissionRequest` and `Stop`; Claude hooks for `Notification`, `Stop`, and `StopFailure`; Codex `notify` is not sufficient for approval notifications; no fallback support exists for older provider hook APIs.
- [ ] Add all new notification source files to `docs/knowledge/assistant/source-map.md`.
- [ ] Add a concise entry to `docs/knowledge/log.md`.
- [ ] Run `rtk cargo xtask docs check`.
- [ ] Expected: docs check passes.

## Task 11: End-To-End Verification

**Files:**
- Whole workspace

- [ ] Run `rtk cargo fmt --all --check`.
- [ ] Expected: formatting passes.
- [ ] Run `rtk cargo clippy --workspace --all-targets --all-features`.
- [ ] Expected: clippy passes with no warnings.
- [ ] Run `rtk cargo test --workspace --all-features`.
- [ ] Expected: all tests pass.
- [ ] Run `rtk cargo build --workspace --release`.
- [ ] Expected: release build passes.
- [ ] Run `rtk cargo xtask docs check`.
- [ ] Expected: docs check passes.
- [ ] Run `rtk git diff --stat`.
- [ ] Expected: diff only contains notification feature files and required docs/knowledge updates.
- [ ] Run `rtk git diff --check`.
- [ ] Expected: no whitespace errors.
- [ ] Do not commit unless the user explicitly asks for a commit.

## Execution Notes

- Use subagent-driven development when executing this plan because protocol, daemon, CLI, SDK, and GUI work can be reviewed at task boundaries.
- Keep each task independently reviewable and testable.
- Do not use a cheaper subagent model for protocol, daemon eventing, security-sensitive hook sanitization, or final integration review.
- Prefer `gpt-5.3-codex-spark` only for bounded mechanical tasks such as docs cross-reference edits or small render test updates after the main design is implemented.
- Treat notification payload security as part of the core feature, not as a later hardening step.

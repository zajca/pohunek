---
type: SourceMap
id: assistant/source-map
title: Assistant source map
description: Existing repository paths that matter when verifying assistant behavior, client behavior, daemon behavior, project handling, and protocol contracts.
source_kind: manual
intents: [setup, project, update, debug, help]
---

# Assistant Source Map

Use this map when the knowledge bundle is not precise enough and exact current
implementation behavior must be verified against the source tree.

Current CLI and command surface:

- `crates/cli/src/main.rs`
- `crates/cli/src/commands/mod.rs`
- `crates/cli/src/commands/assistant/mod.rs`
- `crates/cli/src/commands/assistant/bootstrap.rs`
- `crates/cli/src/commands/doctor.rs`
- `crates/cli/src/commands/daemon.rs`
- `crates/cli/src/commands/attach.rs`
- `crates/cli/src/commands/health.rs`
- `crates/cli/src/commands/session.rs`
- `crates/cli/src/commands/project.rs`
- `crates/cli/src/commands/notifications.rs`
- `crates/cli/src/commands/host_fanout.rs`
- `crates/cli/src/commands/discovery_cache.rs`
- `crates/cli/src/commands/setup.rs`
- `crates/cli/src/commands/host.rs`
- `crates/cli/src/commands/integration.rs`
- `crates/cli/src/commands/prompt.rs`
- `crates/cli/src/client.rs`
- `crates/cli/src/target.rs`
- `crates/cli/src/paths.rs`
- `crates/cli/src/error.rs`
- `crates/cli/tests/notifications_clap.rs`
- `crates/cli/tests/prompt_link.rs`
- `crates/cli/tests/scripts.rs`
- `crates/cli/tests/standalone_discovery.rs`
- `crates/cli/tests/session_process_api.rs`

Native GUI client:

- `crates/gui/src/main.rs`
- `crates/gui/src/config.rs`
- `crates/gui/src/command.rs`
- `crates/gui/src/keyboard.rs`
- `crates/gui/src/runtime.rs`
- `crates/gui/src/view/modals.rs`
- `crates/gui/src/view/inbox.rs`
- `crates/gui/src/view/provider.rs`
- `crates/gui/src/view/session.rs`
- `crates/gui/src/view/review.rs`
- `crates/gui-core/src/assistant.rs`
- `crates/gui-core/src/lib.rs`
- `crates/gui-core/src/sdk.rs`
- `crates/gui-core/src/state.rs`
- `crates/gui-core/src/providers/linear.rs`
- `crates/gui-core/src/providers/github.rs`
- `crates/gui-core/src/review/mod.rs`
- `crates/gui-core/src/review/diff.rs`
- `crates/gui-core/src/review/model.rs`
- `crates/gui-core/src/review/store.rs`
- `crates/gui-core/src/review/dispatch.rs`
- `crates/gui-core/tests/loopback.rs`
- `crates/gui-core/tests/linear_provider.rs`
- `crates/gui-core/tests/github_provider.rs`
- `crates/prompt/src/lib.rs`
- `crates/prompt/src/link.rs`
- `crates/cli/tests/gui_prompt_parity.rs`
- `docs/knowledge/guides/gui.md`
- `docs/phases/06-native-app.md`
- `docs/design/track-d-native-app.md`

Launcher scripts:

- `scripts/lib.sh`
- `scripts/pohunek-launch-issue`
- `scripts/pohunek-launch-pr`
- `docs/knowledge/guides/launcher.md`

Web control center client:

- `web/sdk/src/index.browser.ts`
- `web/sdk/README.md`
- `web/sdk/src/client.ts`
- `web/sdk/src/envelope.ts`
- `web/sdk/src/origin.ts`
- `web/sdk/src/transport.ts`
- `web/backend/src/config.ts`
- `web/backend/src/hosts.ts`
- `web/backend/src/server.ts`
- `web/backend/systemd/pohunek-backend.service`
- `web/backend/systemd/pohunek-backend.service.in`
- `web/release/`
- `web/client-core/src/index.ts`
- `web/frontend/src/App.svelte`
- `web/frontend/src/lib/agent-presentation.ts`
- `web/frontend/src/components/NewSessionDialog.svelte`
- `web/scripts/dev.ts`
- `docs/knowledge/guides/web-control-center.md`
- `docs/design/track-b-web-control-center-plan-2026-07-22.md`
- `docs/phases/04-browser-control-center.md`

Release packaging:

- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`
- `README.md`
- `packaging/install-daemon.sh`
- `packaging/systemd/pohunekd.service.in`
- `packaging/systemd/pohunek-session@.service.in`
- `packaging/systemd/pohunek-sessions.slice`
- `scripts/release`
- `scripts/provision-hermes-compat`
- `scripts/tests/provision-hermes-compat.sh`
- `scripts/smoke-hermes-plugin-release`
- `docs/runbooks/hermes-operator-plugin.md`
- `docs/migrations/hermes-operator-plugin.md`

Daemon, sessions, integrations, and project state:

- `crates/daemon/src/assistant.rs`
- `crates/daemon/src/main.rs`
- `crates/daemon/src/lib.rs`
- `crates/daemon/src/api/handler/mod.rs`
- `crates/daemon/src/session/mod.rs`
- `crates/daemon/src/session/observation.rs`
- `crates/daemon/src/session/diff.rs`
- `crates/daemon/src/session/hooks.rs`
- `crates/daemon/src/session/detector.rs`
- `crates/daemon/src/session/procwatch.rs`
- `crates/daemon/src/runtime/`
- `crates/daemon/src/notify.rs`
- `crates/daemon/src/external/mod.rs`
- `crates/daemon/tests/procwatch.rs`
- `crates/daemon/src/procwatch/mod.rs`
- `crates/daemon/src/procwatch/linux.rs`
- `crates/daemon/src/project/mod.rs`
- `crates/daemon/src/project/config.rs`
- `crates/daemon/src/project/detect.rs`
- `crates/daemon/src/worktree/mod.rs`
- `crates/daemon/src/store/mod.rs`
- `crates/daemon/src/capabilities.rs`
- `crates/daemon/src/integration/mod.rs`
- `crates/daemon/src/integration/assets/codex/pohunek-agent-state.sh`
- `crates/daemon/src/integration/assets/codex/pohunek-agent-notify.sh`
- `crates/daemon/src/integration/assets/claude/pohunek-agent-state.sh`
- `crates/daemon/src/integration/assets/claude/pohunek-agent-notify.sh`
- `crates/daemon/src/notifications/mod.rs`
- `crates/daemon/src/notifications/store.rs`
- `crates/daemon/src/notifications/coordinator.rs`
- `crates/daemon/src/notifications/policy.rs`
- `crates/daemon/src/notifications/projector.rs`
- `crates/daemon/src/paths.rs`
- `crates/daemon/src/logging.rs`
- `crates/logging/src/config.rs`
- `crates/logging/src/lib.rs`

Agent runtime and profile resolution:

- `crates/session-worker/`
- `crates/worker-protocol/`
- `crates/daemon/src/agent/mod.rs`
- `crates/daemon/src/agent/profile.rs`
- `crates/daemon/src/agent/codex.rs`
- `crates/daemon/src/agent/claude.rs`
- `crates/daemon/src/agent/hermes.rs`
- `crates/daemon/src/agent/shell.rs`
- `crates/daemon/src/detect/mod.rs`
- `crates/daemon/src/detect/osc.rs`
- `crates/daemon/src/detect/manifest/mod.rs`
- `crates/daemon/src/detect/manifests/codex.toml`
- `crates/daemon/src/detect/manifests/claude.toml`
- `crates/daemon/src/detect/manifests/hermes.toml`
- `crates/daemon/src/detect/manifests/shell.toml`

Hermes compatibility evidence:

- `compat/hermes/compatibility-lock.json`
- `compat/hermes/README.md`
- `compat/hermes/goldens/manifest.json`
- `crates/xtask/src/hermes.rs`
- `crates/xtask/src/hermes_mock.rs`
- `crates/xtask/src/eval.rs`
- `crates/xtask/src/hermes_skill.rs`

Hermes operator plugin and managed lifecycle:

- `crates/cli/src/hermes_integration/mod.rs`
- `crates/cli/src/hermes_integration/error.rs`
- `crates/cli/src/hermes_integration/target.rs`
- `crates/cli/src/hermes_integration/policy.rs`
- `crates/cli/src/hermes_integration/lifecycle.rs`
- `crates/cli/src/hermes_integration/runner.rs`
- `crates/cli/src/hermes_integration/assets.rs`
- `crates/cli/src/hermes_integration/doctor.rs`
- `crates/cli/src/hermes_integration/skill.rs`
- `crates/cli/src/hermes_integration/assets/pohunek/plugin.yaml`
- `crates/cli/src/hermes_integration/assets/pohunek/__init__.py`
- `crates/cli/src/hermes_integration/assets/pohunek/cli.py`
- `crates/cli/src/hermes_integration/assets/pohunek/hooks.py`
- `crates/cli/src/hermes_integration/assets/pohunek/policy.py`
- `crates/cli/src/hermes_integration/assets/pohunek/redact.py`
- `crates/cli/src/hermes_integration/assets/pohunek/tools.py`
- `crates/cli/src/hermes_integration/assets/tests/test_plugin_runtime.py`
- `docs/knowledge/guides/hermes-operator.md`
- `docs/design/hermes-agent-integration.md`
- `docs/design/hermes-agent-integration-plan.md`
- `docs/public-api.md`

Protocol contracts and transport:

- `crates/client/src/discovery.rs`

- `crates/client/src/lib.rs`
- `crates/client/src/notifications.rs`
- `crates/client/src/transport.rs`
- `crates/paths/src/lib.rs`
- `crates/protocol/src/assistant.rs`
- `crates/protocol/src/envelope.rs`
- `crates/protocol/src/decimal.rs`
- `crates/protocol/src/limits.rs`
- `crates/protocol/src/lib.rs`
- `crates/protocol/src/method.rs`
- `crates/protocol/src/notification.rs`
- `crates/protocol/src/session.rs`
- `crates/protocol/src/project.rs`
- `crates/protocol/src/capabilities.rs`
- `crates/protocol/src/integration.rs`
- `crates/protocol/src/error.rs`
- `crates/protocol/src/version.rs`
- `crates/protocol/tests/roundtrip.rs`
- `crates/netbird/src/lib.rs`
- `crates/netbird/src/status.rs`

Assistant knowledge implementation work in progress:

- `crates/knowledge/src/lib.rs`
- `crates/knowledge/src/assistant.rs`
- `docs/knowledge/`

Design inputs:

- `docs/design/universal-assistant.md`
- `docs/design/universal-assistant-plan.md`
- `docs/design/durable-session-workers-rfc.md`
- `docs/migrations/durable-session-workers.md`
- `docs/runbooks/durable-session-workers.md`
- `docs/public-api.md`

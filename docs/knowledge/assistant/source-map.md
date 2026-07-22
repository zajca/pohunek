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

Native GUI client:

- `crates/gui/src/main.rs`
- `crates/gui/src/config.rs`
- `crates/gui/src/command.rs`
- `crates/gui/src/keyboard.rs`
- `crates/gui/src/runtime.rs`
- `crates/gui/src/view/inbox.rs`
- `crates/gui/src/view/provider.rs`
- `crates/gui/src/view/session.rs`
- `crates/gui/src/view/review.rs`
- `crates/gui-core/src/assistant.rs`
- `crates/gui-core/src/lib.rs`
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
- `web/backend/src/config.ts`
- `web/backend/src/hosts.ts`
- `web/backend/src/server.ts`
- `web/backend/systemd/pohunek-backend.service`
- `web/client-core/src/index.ts`
- `web/frontend/src/App.svelte`
- `web/scripts/dev.ts`
- `docs/knowledge/guides/web-control-center.md`
- `docs/design/track-b-web-control-center-plan-2026-07-22.md`
- `docs/phases/04-browser-control-center.md`

Release packaging:

- `.github/workflows/release.yml`
- `README.md`
- `scripts/release`

Daemon, sessions, integrations, and project state:

- `crates/daemon/src/assistant.rs`
- `crates/daemon/src/main.rs`
- `crates/daemon/src/lib.rs`
- `crates/daemon/src/api/handler/mod.rs`
- `crates/daemon/src/session/mod.rs`
- `crates/daemon/src/session/diff.rs`
- `crates/daemon/src/session/hooks.rs`
- `crates/daemon/src/session/detector.rs`
- `crates/daemon/src/session/procwatch.rs`
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

Agent runtime and profile resolution:

- `crates/daemon/src/agent/mod.rs`
- `crates/daemon/src/agent/profile.rs`
- `crates/daemon/src/agent/codex.rs`
- `crates/daemon/src/agent/claude.rs`
- `crates/daemon/src/agent/shell.rs`
- `crates/daemon/src/detect/mod.rs`
- `crates/daemon/src/detect/osc.rs`
- `crates/daemon/src/detect/manifest/mod.rs`
- `crates/daemon/src/detect/manifests/codex.toml`
- `crates/daemon/src/detect/manifests/claude.toml`
- `crates/daemon/src/detect/manifests/shell.toml`

Protocol contracts and transport:

- `crates/client/src/lib.rs`
- `crates/client/src/notifications.rs`
- `crates/client/src/transport.rs`
- `crates/paths/src/lib.rs`
- `crates/protocol/src/assistant.rs`
- `crates/protocol/src/envelope.rs`
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
- `docs/public-api.md`

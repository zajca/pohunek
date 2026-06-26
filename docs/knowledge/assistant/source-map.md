---
type: SourceMap
id: assistant/source-map
title: Assistant source map
description: Existing repository paths that matter when verifying assistant behavior, CLI behavior, daemon behavior, project handling, and protocol contracts.
source_kind: manual
intents: [setup, project, update, debug, help]
---

# Assistant Source Map

Use this map when the knowledge bundle is not precise enough and exact current
implementation behavior must be verified against the source tree.

Current CLI and command surface:

- `crates/cli/src/assistant.rs`
- `crates/cli/src/main.rs`
- `crates/cli/src/commands/mod.rs`
- `crates/cli/src/commands/assistant/mod.rs`
- `crates/cli/src/commands/assistant/bootstrap.rs`
- `crates/cli/src/commands/assistant/prompt.rs`
- `crates/cli/src/commands/assistant/select.rs`
- `crates/cli/src/commands/assistant/snapshot.rs`
- `crates/cli/src/commands/doctor.rs`
- `crates/cli/src/commands/daemon.rs`
- `crates/cli/src/commands/health.rs`
- `crates/cli/src/commands/session.rs`
- `crates/cli/src/commands/project.rs`
- `crates/cli/src/commands/setup.rs`
- `crates/cli/src/commands/host.rs`
- `crates/cli/src/commands/integration.rs`
- `crates/cli/src/client.rs`
- `crates/cli/src/target.rs`
- `crates/cli/src/paths.rs`
- `crates/cli/src/error.rs`

Daemon, sessions, integrations, and project state:

- `crates/daemon/src/assistant.rs`
- `crates/daemon/src/main.rs`
- `crates/daemon/src/lib.rs`
- `crates/daemon/src/api/handler.rs`
- `crates/daemon/src/session/mod.rs`
- `crates/daemon/src/project/mod.rs`
- `crates/daemon/src/project/config.rs`
- `crates/daemon/src/project/detect.rs`
- `crates/daemon/src/worktree/mod.rs`
- `crates/daemon/src/store/mod.rs`
- `crates/daemon/src/capabilities.rs`
- `crates/daemon/src/integration/mod.rs`
- `crates/daemon/src/paths.rs`
- `crates/daemon/src/logging.rs`

Agent runtime and profile resolution:

- `crates/daemon/src/agent/mod.rs`
- `crates/daemon/src/agent/profile.rs`
- `crates/daemon/src/agent/codex.rs`
- `crates/daemon/src/agent/claude.rs`
- `crates/daemon/src/agent/shell.rs`
- `crates/daemon/src/detect/manifest.rs`
- `crates/daemon/src/detect/manifests/codex.toml`
- `crates/daemon/src/detect/manifests/claude.toml`
- `crates/daemon/src/detect/manifests/shell.toml`

Protocol contracts and transport:

- `crates/protocol/src/assistant.rs`
- `crates/protocol/src/lib.rs`
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

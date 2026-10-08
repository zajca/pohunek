# pohunek Documentation

This directory turns the product idea into implementation-oriented planning.

## Reference

- [Features and architecture overview](features.md) — what the core does,
  how a host is put together, and the trust boundary.
- [Manual installation](install.md) — release archives, the login service,
  upgrades, building from source, and a first session.
- [CLI guide](cli.md) — every command, multihost targeting, automation,
  notifications, the assistant, and Hermes.
- [SDKs and building your own client](sdk.md) — Rust and TypeScript SDK
  examples.
- [Development](development.md) — workspace layout, gates, fast loops, and
  release.

## Source of Truth

- [Project idea](../idea.md) — the original, broad brainstorm (kept for context).
- [Application architecture](architecture.md) — the **authoritative current
  direction**. Where it disagrees with `idea.md`, it wins.

## Committed Direction (summary)

The shipped protocol v4 product is an **owner-operated personal multi-host
tool**: durable coding-agent sessions across the owner's machines locally or on
a NetBird (WireGuard) network. The accepted, not-yet-implemented
[team relay RFC](design/team-relay-control-plane-rfc.md) adds an optional public
multi-team control plane without replacing either owner path.

- CLI-first; Rust daemon + Rust CLI.
- Daemon owns PTYs; clients attach/detach. Agents run PTY/TUI-first.
- Codex, Claude Code, and the local interactive Hermes Agent runtime are
  first-class. Hermes is pinned to version 0.20.0. Its operator plugin is
  installed explicitly per profile or custom owner-private home; it provides
  typed tools, generated skill, and best-effort lifecycle reporting without
  reading Hermes `state.db`.
- Remote transport is **direct over NetBird**, not an SSH bridge.
- Discovery is **tokenless NetBird-local** + live capability query (no signed
  manifests, no mesh crypto).
- Control protocol: newline-delimited JSON over a Unix socket (local) and a TCP
  listener bound to the NetBird interface (remote). Attach uses a **separate
  raw-byte connection** per PTY.
- Protocol v4 has no multi-user authorization; socket permissions and NetBird
  are its owner trust boundary. The planned relay owns principals, service
  accounts, teams, roles, and session ACLs, while `pohunekd` enforces only the
  enrolled relay and host-approved `HostShare` ceilings.
- The Rust SDK, native desktop app, and optional owner-path browser control
  center are shipped; the desktop app and the browser control center live in
  `zajca/pohunek-work`. The accepted team relay adds a separate team web surface;
  it does not replace the owner WebUI or require a relay local mode.

## Phases

- [Phase 1: Core Local Sessions](phases/01-core-local-sessions.md)
- [Phase 2: Remote Hosts over NetBird](phases/02-remote-netbird.md)
- [Phase 3 (superseded): Later Providers and libghostty GUI](phases/03-later-providers-and-gui.md)
  — historical; replaced by the SDK-first and native-desktop direction in the roadmap.
- [Phase 4: Browser Control Center](phases/04-browser-control-center.md)
  — historical plan behind the shipped optional owner-path browser control center.
- [Phase 5: rofi / sway Launcher](phases/05-rofi-sway-launcher.md)
  — historical; the launcher now lives in `zajca/pohunek-work`.

## Detailed Plans

- [Phase 1 implementation plan](plan-phase-1.md)
- [Public API](public-api.md) — versioned control protocol, envelopes, methods,
  errors, events, attach stream, and Rust SDK surface.

## Design Notes (proposals, pre-phase)

- [Track B web control center plan](design/track-b-web-control-center-plan-2026-07-22.md)
  — milestone split and reconciled decisions for the browser control center
  (thin owner gateway + browser-side aggregation in the client-core package,
  now in `zajca/pohunek-work`).

- [Delegated task runs RFC](design/delegated-task-runs-rfc.md) — accepted:
  task and turn records over ordinary sessions, causal turn settlement, blocking
  `task.wait`, typed results with repository evidence and checks, OpenCode
  2.x as the first provider with structured turn evidence, and
  manager/auditor composition above the daemon (the relay dark factory).
  Amended 2026-10-08 (design-only): durable client work items, a client
  acceptance eligibility policy, and host-enforced human input control
  (`task.input_control`).
- [Relay dark factory RFC](design/relay-dark-factory-rfc.md) — accepted:
  relay authorization, budgets, audit, task projections and escalation for
  unattended manager/auditor delegation over the team relay; the factory is a
  client, never relay or daemon logic. Amended 2026-10-08 (design-only):
  protected audit reserves, the versioned FactorySpec run manifest, and a
  bounded semantic progress policy; factory tracking and recovery state stay
  client-owned.
- [Universal Pohunek Assistant](design/universal-assistant.md) - one ordinary
  session-backed assistant, steered by intent and a live snapshot, for setup,
  project configuration, updates, troubleshooting, and general help.
- [First-class Hermes agent integration](design/hermes-agent-integration.md) —
  managed Hermes runtime plus a profile-scoped Hermes plugin with typed Pohunek
  tools, lifecycle hooks, and a generated skill.
  - [Implementation plan](design/hermes-agent-integration-plan.md) — complete
    protocol, worker, daemon, CLI, plugin, client, security, testing, and release
    workstreams.
- [Hermes operator guide](knowledge/guides/hermes-operator.md) — explicit
  profile/home selection, install/status/doctor/update/uninstall, access modes,
  safe tool loop, and recovery.
- [Hermes plugin rollout and recovery](runbooks/hermes-operator-plugin.md) —
  ordinary M3 rollout, canary verification, incident recovery, and safe removal.
- [Hermes plugin migration and rollback](migrations/hermes-operator-plugin.md) —
  one-time M1 context, upgrade-forward limits, and rollback boundaries.
- [Projects: automatic git-repo awareness](design/projects.md) — detect the repo
  / worktree a session runs in and record lightweight projects; auto on session
  start or manual via CLI, no filesystem scan.
  - [Implementation plan](design/projects-plan.md) — milestones M1–M5, code
    touch-points, protocol compatibility.

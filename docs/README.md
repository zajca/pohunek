# pohunek Documentation

This directory turns the product idea into implementation-oriented planning.

## Source of Truth

- [Project idea](../idea.md) — the original, broad brainstorm (kept for context).
- [Application architecture](architecture.md) — the **authoritative current
  direction**. Where it disagrees with `idea.md`, it wins.

## Committed Direction (summary)

`pohunek` is a **single-user, personal multi-host tool**: durable coding-agent
sessions across your own machines on a NetBird (WireGuard) network.

- CLI-first; Rust daemon + Rust CLI.
- Daemon owns PTYs; clients attach/detach. Agents run PTY/TUI-first.
- Codex and Claude Code are both first-class.
- Remote transport is **direct over NetBird**, not an SSH bridge.
- Discovery is **tokenless NetBird-local** + live capability query (no signed
  manifests, no mesh crypto).
- Control protocol: newline-delimited JSON over a Unix socket (local) and a TCP
  listener bound to the NetBird interface (remote). Attach uses a **separate
  raw-byte connection** per PTY.
- No multi-user authorization (single operator; socket perms + NetBird are the
  boundary).
- Providers (Linear/GitHub) and a GUI are **deferred**; the eventual GUI is a
  **browser control center** served by a standalone TS aggregator backend
  (Phase 4), not a native libghostty client (dropped).

## Phases

- [Phase 1: Core Local Sessions](phases/01-core-local-sessions.md)
- [Phase 2: Remote Hosts over NetBird](phases/02-remote-netbird.md)
- [Phase 3 (superseded): Later Providers and libghostty GUI](phases/03-later-providers-and-gui.md)
  — the GUI direction is replaced by Phase 4; its provider track is absorbed there.
- [Phase 4: Browser Control Center](phases/04-browser-control-center.md)
  — browser GUI via a standalone TS aggregator backend, with Linear/GitHub support.
- [Phase 5: rofi / sway Launcher](phases/05-rofi-sway-launcher.md)
  — keyboard-driven launcher; the thinnest proof the chassis is an API.

## Detailed Plans

- [Phase 1 implementation plan](plan-phase-1.md)

## Design Notes (proposals, pre-phase)

- [Universal Pohunek Assistant](design/universal-assistant.md) - one ordinary
  session-backed assistant, steered by intent and a live snapshot, for setup,
  project configuration, updates, troubleshooting, and general help.
- [Projects: automatic git-repo awareness](design/projects.md) — detect the repo
  / worktree a session runs in and record lightweight projects; auto on session
  start or manual via CLI, no filesystem scan.
  - [Implementation plan](design/projects-plan.md) — milestones M1–M5, code
    touch-points, protocol compatibility.

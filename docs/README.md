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
- The next client-surface work starts with a Rust SDK, then a pure-native Rust
  desktop companion app as the primary GUI. The browser control center is later
  and optional.

## Phases

- [Phase 1: Core Local Sessions](phases/01-core-local-sessions.md)
- [Phase 2: Remote Hosts over NetBird](phases/02-remote-netbird.md)
- [Phase 3 (superseded): Later Providers and libghostty GUI](phases/03-later-providers-and-gui.md)
  — historical; replaced by the SDK-first and native-desktop direction in the roadmap.
- [Phase 4: Browser Control Center](phases/04-browser-control-center.md)
  — historical/deferred browser GUI plan; now later and optional after the desktop app.
- [Phase 5: rofi / sway Launcher](phases/05-rofi-sway-launcher.md)
  — keyboard-driven launcher; the thinnest proof the chassis is an API.

## Detailed Plans

- [Phase 1 implementation plan](plan-phase-1.md)
- [Public API](public-api.md) — versioned control protocol, envelopes, methods,
  errors, events, attach stream, and Rust SDK surface.

## Design Notes (proposals, pre-phase)

- [Track B web control center plan](design/track-b-web-control-center-plan-2026-07-22.md)
  — milestone split and reconciled decisions for the browser control center
  (thin relay backend + browser-side aggregation in `web/client-core`).

- [Universal Pohunek Assistant](design/universal-assistant.md) - one ordinary
  session-backed assistant, steered by intent and a live snapshot, for setup,
  project configuration, updates, troubleshooting, and general help.
- [Projects: automatic git-repo awareness](design/projects.md) — detect the repo
  / worktree a session runs in and record lightweight projects; auto on session
  start or manual via CLI, no filesystem scan.
  - [Implementation plan](design/projects-plan.md) — milestones M1–M5, code
    touch-points, protocol compatibility.

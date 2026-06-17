# zagentmesh Documentation

This directory turns the product idea into implementation-oriented planning.

## Source of Truth

- [Project idea](../idea.md) — the original, broad brainstorm (kept for context).
- [Application architecture](architecture.md) — the **authoritative current
  direction**. Where it disagrees with `idea.md`, it wins.

## Committed Direction (summary)

`zagentmesh` is a **single-user, personal multi-host tool**: durable coding-agent
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
- Providers (Linear/GitHub) and a libghostty GUI are **deferred**; libghostty
  remains the eventual GUI target.

## Phases

- [Phase 1: Core Local Sessions](phases/01-core-local-sessions.md)
- [Phase 2: Remote Hosts over NetBird](phases/02-remote-netbird.md)
- [Later (Deferred): Providers and libghostty GUI](phases/03-later-providers-and-gui.md)

## Detailed Plans

- [Phase 1 implementation plan](plan-phase-1.md)

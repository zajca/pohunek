# Later (Deferred): Provider Integration and libghostty GUI

This document captures work that is intentionally **deferred until the core
(Phase 1 + Phase 2) is in daily use**. It is not the committed near-term path. It
exists so the direction stays recorded and the core is built without foreclosing
it.

Both items are pulled forward only when daily use of the core shows the need.

---

## Later A: Provider Integration (Linear / GitHub) via Shell-Out

### Objective

Link sessions to work items and pull requests without maintaining in-tree API
adapters. GitHub goes through `gh`; Linear through its MCP or GraphQL API.

### Approach

- **No in-tree provider adapters in the core path.** The daemon shells out to
  battle-tested tools (`gh` for GitHub; Linear via MCP/API) so provider API and
  permission churn is not zagentmesh's maintenance burden.
- Worktree-per-session isolation already lives in the core (Phase 1) and is the
  foundation this builds on — provider linking is metadata on top of an existing
  session+worktree.

### Likely commands

```bash
zagentmesh session link --work-item <id> --session <session>   # store link metadata
zagentmesh pr open --session <session>                          # -> `gh pr create`
zagentmesh pr status --session <session>                        # -> `gh pr view`
zagentmesh review <session>                                     # diff + linked task/PR/checks
```

### Notes

- Provider actions are recorded in the local event log (no secrets).
- Credentials stay in `gh`'s own auth / OS keychain / env — never in zagentmesh
  state.
- Session metadata gains optional `work_item` and `pr` link fields; these are not
  required for core sessions to work.
- Revisit an in-tree adapter only if shell-out proves insufficient.

---

## Later B: libghostty GUI Client

### Objective

A native Linux multi-pane workspace GUI for local and remote sessions. libghostty
remains the intended rendering technology.

### Why deferred

- The core PTY/TUI choice means `zagentmesh attach <session>` already renders the
  agent's TUI in your existing terminal. A native GUI is needed only for the
  multi-pane workspace (multiple sessions, sidebar, state badges), which is a
  nice-to-have, not a functional requirement.
- A feasibility review (June 2026) found that, at that time, the released
  embeddable piece was `libghostty-vt` (terminal logic only: VT parsing,
  render-state, key encoding), while GPU rendering, a GTK widget, and surface
  handoff were roadmap. The Rust crate `libghostty-vt` existed but was pre-1.0
  with a Zig toolchain dependency. **Re-verify libghostty's current state at the
  time this work actually starts** — it moves fast and may have advanced.

### Approach (when built)

- Keep PTY ownership in the daemon; the GUI is a pure client over the same
  control protocol + separate attach stream used by the CLI and remote transport.
- Re-scope the spike to reality: a window (winit/wgpu or GTK4) + a terminal grid
  renderer + font rasterization, driven by libghostty-vt's render-state and key
  encoder. PTY is owned by the daemon, not the GUI.
- Run a short parallel spike against `alacritty_terminal` (stable pure-Rust,
  includes PTY) as a no-regret fallback, and choose on evidence.
- Deliver: workspace with tabs/panes/splits, host/session navigation, agent state
  badges, notification inbox, attach/detach/reconnect — all via daemon APIs.

### Notes

- The GUI must not move PTY ownership into the client or force a protocol
  redesign.
- Layout state is client-local; authoritative session state stays with the
  owning host's daemon.

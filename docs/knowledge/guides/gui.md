---
type: Guide
id: guide/gui
title: GUI setup
description: Configure and troubleshoot the native pohunek-gui desktop control plane.
source_kind: manual
intents: [setup, debug, help]
---

# GUI Setup

`pohunek-gui` is the native, session-first desktop control plane. It shows
hosts and project context in a narrow left rail and a prioritized cross-host
session list in the main pane. It does not embed a terminal: opening a session
spawns the configured `attach_command`.

The native GUI intentionally has no Linear, GitHub, review, worktree-management,
or Agents-monitor surfaces. Those removals do not change the daemon protocol,
CLI project/worktree commands, linked session metadata, or the web control
center.

## Preconditions

1. Run `pohunek doctor --json`.
2. Start the daemon with `pohunek daemon start --detach` if needed.
3. Run `pohunek health --json` or `pohunek status --json`.
4. Run `pohunek host inspect local --json` to confirm agent capabilities.

The Linux v1 GUI is Wayland-only. If `WAYLAND_DISPLAY` is missing or empty,
`pohunek-gui` exits before starting Iced. An X11-only `DISPLAY` is not a
supported fallback.

## Configuration

The GUI reads `$XDG_CONFIG_HOME/pohunek/gui.toml`, or
`~/.config/pohunek/gui.toml` when `XDG_CONFIG_HOME` is unset.

```toml
pohunek_bin = "/path/to/pohunek"
attach_command = "$TERMINAL -e sh -c 'exec {bin} attach --host {host} {id}'"
notification_command = "notify-send"

[gui]
connect_timeout_ms = 2000
request_timeout_ms = 5000
reconcile_secs = 30
backoff_initial_ms = 1000
backoff_max_ms = 30000
terminal_cols = 80
terminal_rows = 24
```

Use an absolute `pohunek_bin` when a desktop launcher may have a different
`PATH`. `attach_command` supports exactly `{bin}`, `{host}`, and `{id}`.
`notification_command` defaults to `notify-send` and receives title and body as
separate arguments.

Provider-specific GUI configuration and `open_url_command` are no longer read.
Unknown legacy TOML fields are ignored by Serde, but they should be removed from
maintained configuration files.

## Running

From a source checkout:

```sh
cargo run -p pohunek-gui
```

From an installed GUI component archive:

```sh
pohunek-gui
```

## Session List

The main pane always groups every loaded session in this priority order:

1. **Needs action** — blocked sessions and sessions with actionable durable
   notifications.
2. **Idle** — live, attachable sessions that are not currently working.
3. **Running** — working, starting, or reconnecting sessions.
4. **Unavailable** — terminal, external, conflicting, incompatible, lost, or
   otherwise unusable sessions.

Rows are stable within a group by host id and session id. Activity or runtime
changes may move a row between groups, but do not reorder unrelated rows inside
the same group. Every row identifies its project explicitly as
`project:<label>`, falling back to the project id or `project:unassigned`.

Each eligible row exposes direct actions:

- **Open** attaches to a live PTY.
- **Resume** relaunches from valid native recovery metadata and then attaches.
- **Terminate** calls `session.stop` for a safe managed runtime.
- **Delete** opens a confirmation modal and then calls `session.remove`.

Actions fail closed for external sessions and conflicting or incompatible
runtimes. A stale click is revalidated before terminate or delete is sent.

Clicking the row itself opens session detail in a modal over the unchanged
session list. The modal contains inspection, terminal observation, fork,
rename, metadata, terminate, and delete controls according to current
capabilities. Worktree path and branch can still appear as read-only session
metadata; the GUI does not browse or manage worktrees.

## Navigation and Keyboard

The left rail contains Assistant, Inbox, hosts, and projects. Select a project
before starting a session. Sessions do not appear in the left tree because the
main pane is their single navigation surface.

Double-clicking a project row selects that project and opens a fresh Start
session modal scoped to it.

Default global bindings:

| Name | Default | Behavior |
|------|---------|----------|
| `open_inbox` | `i` | Open the Inbox modal. |
| `open_selected_session` | `o` | Open or resume the selected session in a terminal. |
| `show_selected_session` | `enter` | Open the selected session detail modal. |
| `open_keymap_help` | `shift+?` | Show the effective keymap. |
| `new_session` | `n` | Open the Start session modal when a project is selected. |
| `open_assistant` | `a` | Open the Assistant modal. |

Modal bindings include `escape`, `enter`, `shift+enter`, `o`, and `j`/`k` or
the arrow keys for Inbox navigation. Launch forms reserve Enter for select
confirmation and use Ctrl+Enter for submission. Add a partial `[keybindings]`
table to override supported names. Unknown removed binding names fail
configuration validation instead of silently doing nothing.

Tab and Shift+Tab are conventional, non-configurable form navigation in the
Start session and Assistant modals. Focus cycles through both leading select
fields, the prompt/name inputs, and visible Advanced branch fields, never into
controls behind the modal overlay. On a focused select, Up or Down opens its
options, the arrow keys move the option cursor, and Enter confirms the choice.
Ctrl+Enter submits either launch form from any focused field.

## Session and Assistant Launch

The Start session modal calls `project.actions`, resolves the chosen action with
`project.action`, renders through the shared prompt crate, and creates the
session with `session.new`. A blank session uses provider `none`. Runtime choices
come from `host.inspect` and fail closed when a runtime is unavailable or
unsupported.

The Assistant entry opens a native launch modal. It is scoped to the selected
project, or to the project linked from the selected session. The shared
`gui-core::assistant` launcher performs host inspection, snapshot creation,
knowledge materialization, prompt composition, and `session.new`.

## Inbox

Inbox is a modal over durable cross-host notifications. It offers `Needs action`,
`All`, and `Archived` scopes, auto-marks a notification read when opened, and
can select its linked session. Linked actionable notifications also promote the
session into the main list's Needs action group.

The daemon remains the source of truth for notification lifecycle. The GUI
raises desktop notifications only for newly created `action_required` or
`error` records. Acknowledged `action_required` and `error` notifications remain
actionable until archived; acknowledgement alone does not remove their linked
session from the Needs action group.

## Troubleshooting

- No sessions: verify `session.list` and daemon health first.
- Host unavailable: inspect the surfaced per-host error and run
  `pohunek host inspect <host> --json`.
- Open unavailable: inspect runtime state and resume capability in the session
  modal; external/conflict/incompatible states are intentionally read-only.
- Attach command does not launch: verify the configured binary, terminal, and
  placeholders outside the GUI.
- Legacy provider keys remain in `gui.toml`: remove them; the native GUI no
  longer consumes them.

Relevant implementation sources:

- `crates/gui/src/view/detail.rs` — prioritized list and quick actions.
- `crates/gui/src/view/session.rs` — detail and delete-confirmation modals.
- `crates/gui/src/view/tree.rs` — host/project context rail.
- `crates/gui/src/keyboard.rs` — supported bindings and routing.
- `crates/gui-core/src/state.rs` — grouping and capability-derived action model.
- `crates/gui-core/src/ui_state.rs` — persisted window, tree, and selection state.

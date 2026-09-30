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
2. If the daemon does not answer, check `pohunek service status --json` and
   install the login service with `pohunek service install` if needed.
3. Run `pohunek health --json` or `pohunek status --json`.
4. Run `pohunek host inspect local --json` to confirm agent capabilities.
5. Run `pohunek host governance inspect local --json` when verifying the
   stable host identity and safe governance projection shown after a GUI host
   snapshot. The GUI displays loading, failure, never-enrolled, enrolled, and
   quarantined states without offering a governance mutation.

The Linux v1 GUI is Wayland-only. If `WAYLAND_DISPLAY` is missing or empty,
`pohunek-gui` exits before starting Iced. An X11-only `DISPLAY` is not a
supported fallback.

On macOS the GUI starts with the native window backend and checks no display
server variable. It reads `~/.config/pohunek/gui.toml` (not `~/Library`) unless
`XDG_CONFIG_HOME` is set, which also holds for a Finder or launchd start because
those set `HOME`. A missing or invalid `gui.toml` never crashes the app: the
typed error is shown in the status line and the workspace stays empty.

## Configuration

The GUI reads `$XDG_CONFIG_HOME/pohunek/gui.toml`, or
`~/.config/pohunek/gui.toml` when `XDG_CONFIG_HOME` is unset.

Set exactly one of `attach_command` or `attach_terminal`; setting both, or
neither, is a startup configuration error shown in the UI.

```toml
pohunek_bin = "/path/to/pohunek"
attach_command = "$TERMINAL -e sh -c 'exec \"$@\"' sh {bin} attach --host {host} {id}"
attach_command_mode = "shell"   # optional: "shell" (default) or "argv"
# attach_terminal = "terminal-app"   # macOS only, instead of attach_command
notification_command = "notify-send"   # optional; see Notifications

[gui]
connect_timeout_ms = 2000
request_timeout_ms = 5000
reconcile_secs = 30
backoff_initial_ms = 1000
backoff_max_ms = 30000
terminal_cols = 80
terminal_rows = 24
open_timeout_ms = 5000                 # attach_terminal: wait for `open`
login_shell_timeout_ms = 10000         # macOS: bare pohunek_bin discovery
login_shell_max_output_bytes = 65536   # macOS: bare pohunek_bin discovery
notification_timeout_ms = 5000         # per-notification backend deadline
```

### `pohunek_bin` resolution

An absolute `pohunek_bin` is used as is and must be an executable file. A bare
name such as `pohunek` is looked up per attach, off the UI thread, in a search
path resolved by the shared environment policy
([environment-resolution](environment-resolution.md)): on macOS one bounded
non-interactive login-shell probe (Finder and launchd start apps with only the
system directories on `PATH`), elsewhere the inherited `PATH`, then the fixed
fallback directories (`~/.local/bin`, `~/.cargo/bin`, Homebrew, `/usr/local`).
The resolved search path is cached; a miss discards it once and discovers again,
so a `pohunek` installed after the GUI started is found without a restart. An
unresolvable name is an attach error in the status line, never a guessed
default. `{bin}` in a template is always the resolved absolute path.

### Attach launch modes

- **`attach_command`, `attach_command_mode = "shell"` (default).** The template
  is rendered with shell-escaped `{bin}`, `{host}`, `{id}` values and run
  through `sh -c`. Values cannot change the command structure, but the
  surrounding template is shell text.
- **`attach_command`, `attach_command_mode = "argv"` (preferred for new
  configurations).** The template is split into words with POSIX quoting rules
  and executed without a shell; `$VAR`, `~`, globs, and command substitution stay
  literal, and each placeholder value stays one argument. Use it for terminals
  that take a command as arguments.
- **`attach_terminal = "terminal-app"` (macOS).** The GUI writes a private
  self-deleting `.command` script and runs `open -a Terminal <script>` with an
  argument array. The script lives in an owner-only (`0700`) directory below the
  pohunek runtime directory, is created exclusively under an unpredictable name,
  removes itself first, then `exec`s `<resolved pohunek_bin> [--host=<host>]
  attach -- <id>` with every value single-quoted. No AppleScript is built, so no
  Automation permission prompt appears. `open` gets `open_timeout_ms` to accept
  the request; a failure or timeout is an attach error and removes the script.
  On any other operating system `attach_terminal` is a configuration error.

Every attach process starts in its own process group with null stdio and is
reaped by a helper thread. Closing the GUI never stops or kills attached
terminals or supervised sessions: the GUI sends no stop or kill on exit.

### Third-party terminals on macOS or Linux (argv mode)

These are documented forms of each terminal's command-line syntax. They are not
exercised in CI (only the argv rendering and the `open`-based Terminal path are);
verify them on your machine.

```toml
# kitty
attach_command_mode = "argv"
attach_command = "kitty -e {bin} --host={host} attach -- {id}"
# Alacritty
attach_command = "alacritty -e {bin} --host={host} attach -- {id}"
# WezTerm
attach_command = "wezterm start -- {bin} --host={host} attach -- {id}"
# Ghostty
attach_command = "ghostty -e {bin} --host={host} attach -- {id}"
```

For the local host `{host}` is empty, and an empty `--host=` selects the local
daemon.

Use an absolute `pohunek_bin` when you want no lookup at all. `attach_command`
supports exactly `{bin}`, `{host}`, and `{id}`.
For a discovered remote host, `{host}` is a provider-qualified
`<overlay>:<canonical-identity>@<discovered-port>` route. The identity is a
slash-safe, unpadded base64url `peer~` selector, or an `fqdn~` fallback when the
provider supplies no stable peer ID. The spawned CLI decodes, re-resolves, and
validates that typed identity through current provider state, then uses the
discovered port instead of its own registry environment. GUI reconnects use the
same process and never retain the discovered IP as identity.
The exact resolved socket endpoint lives only inside one SDK client so its raw
attach remains on the same route as its control connection.

### Notifications

- **Linux default:** `notify-send`. An explicit `notification_command` (any
  platform) replaces the default and receives the title and body as two
  positional arguments, unchanged.
- **macOS default:** `/usr/bin/osascript` running
  `-e 'on run argv' -e 'display notification (item 2 of argv) with title (item 1 of argv)' -e 'end run' -- <title> <body>`.
  Title and body are only argv items after `--`, never script text, so quotes,
  `$()`, newlines, Unicode, and a leading `-` are inert.
- A backend name that is not absolute is resolved with the same environment
  policy as `pohunek_bin` (login-shell discovery on macOS). A configured or
  defaulted `notify-send` that cannot be found on macOS is reported as
  unavailable, not treated as working.
- Each notification gets `[gui] notification_timeout_ms` (default 5000, zero is
  rejected); a backend that does not exit by then is killed and reaped.
- The status line shows one message when the state changes to unavailable
  (missing executable, spawn failure, timeout, non-zero exit) or denied, and one
  when it recovers; repeated identical failures stay silent.

What cannot be confirmed: `osascript` exits 0 whether the notification was shown
or suppressed by System Settings (Notifications, Focus), and an unbundled binary
cannot use `UNUserNotificationCenter` to ask. "Submitted" therefore means the
request was accepted, not that it was displayed. The only denial the GUI reports
is `osascript` printing error `-1743` (a refused permission); that is a
best-effort mapping, and ordinary notification suppression is undetectable until
the GUI ships as an app bundle (issue #104).

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

1. **Needs you** — sessions currently blocked, carrying an unresolved approval,
   or waiting for failure review.
2. **Running** — working, starting, or reconnecting sessions.
3. **Ready** — live, attachable sessions at their normal idle prompt.
4. **Unavailable** — terminal, external, conflicting, incompatible, lost, or
   otherwise unusable sessions.

Rows are stable within a group by host id and session id. Activity or runtime
changes may move a row between groups, but do not reorder unrelated rows inside
the same group. Every row identifies its project explicitly as
`project:<label>`, falling back to the project id or `project:unassigned`.
Unread informational or historical notifications never move a session into
Needs you. A current input or approval request is labeled directly on the row.

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
metadata; the GUI does not browse or manage worktrees. Session detail separates
Current attention from Recent activity and links to the host-filtered Activity
view. It also lists the durable current and recent Claude/Codex subagents,
including how many are still working.

## Navigation and Keyboard

The left rail contains Assistant, Activity, hosts, and projects. Select a project
before starting a session. Sessions do not appear in the left tree because the
main pane is their single navigation surface.

Double-clicking a project row selects that project and opens a fresh Start
session modal scoped to it.

Default global bindings:

| Name | Default | Behavior |
|------|---------|----------|
| `open_inbox` | `i` | Open the Activity modal. |
| `open_selected_session` | `o` | Open or resume the selected session in a terminal. |
| `show_selected_session` | `enter` | Open the selected session detail modal. |
| `open_keymap_help` | `shift+?` | Show the effective keymap. |
| `new_session` | `n` | Open the Start session modal when a project is selected. |
| `open_assistant` | `a` | Open the Assistant modal. |

Modal bindings include `escape`, `enter`, `shift+enter`, `o`, and `j`/`k` or
the arrow keys for Activity navigation. Launch forms reserve Enter for select
confirmation and use Ctrl+Enter for submission; on macOS Command+Enter submits
as well, and Ctrl+Enter keeps working. The submit chords are fixed, not part of
the `[keybindings]` table. Add a partial `[keybindings]` table to override
supported names; chord modifiers are `ctrl`/`control`, `alt`/`option`/`opt`,
`shift`, and `logo`/`super`/`meta`/`cmd`/`command` (all Command/Super map to one
modifier), and a global binding such as `cmd+i` is valid. Unknown removed binding names fail
configuration validation instead of silently doing nothing.

Tab and Shift+Tab are conventional, non-configurable form navigation in the
Start session and Assistant modals. Focus cycles through both leading select
fields, the prompt/name inputs, and visible Advanced branch fields, never into
controls behind the modal overlay. On a focused select, Up or Down opens its
options, the arrow keys move the option cursor, and Enter confirms the choice.
Ctrl+Enter (and Command+Enter on macOS) submits either launch form from any
focused field. A Command-modified key never triggers a modal binding: Cmd+O,
Cmd+J, or Cmd+K do not act as bare `o`, `j`, or `k`, and Cmd+Tab is not form
navigation.

### macOS input review

The GUI relies on Iced for every text-entry behavior, and the shell only sees
events no widget consumed (`keyboard::listen` passes ignored events only,
`crates/gui/src/keyboard.rs:656`), so:

- **Clipboard and editing shortcuts:** the text inputs and the prompt editors
  are Iced widgets, which map Cmd+C/V/X/A and Option+arrows on macOS and
  Ctrl+... elsewhere. No GUI code reads or writes the clipboard directly.
- **Read-only selectable text** (`crates/gui/src/view/selectable_text.rs:302`)
  keeps Copy, select, and cursor movement and drops Cut, Paste, and edits, so
  Cmd+C copies and Cmd+V/X do nothing there.
- **Browser opening:** there is none. The GUI contains no URL-opening code, and
  `open_url_command` is not read, so there is nothing to port to `open(1)`.
- Nothing else needed to change for text selection or input handling.

## macOS launch verification

`scripts/acceptance/macos-gui-launch` verifies, on a Mac logged in at the GUI:
the real `pohunek-gui` starts from a Finder-like environment (`env -i`, only
`HOME`, `USER`, `LOGNAME`, and `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, plus XDG
variables that isolate the run) next to an isolated daemon and shell session
and stays up for `GUI_ACCEPT_UP_S` seconds without exiting or writing to
stderr; ending it with SIGTERM leaves the daemon, session worker, and session
child unchanged (same PIDs and start times) and the session live; and the
attach launcher tests pass on that host. It needs `GUI_ACCEPT_BIN_DIR`, a
directory with `pohunek`, `pohunekd`, `pohunek-sessiond`, and `pohunek-gui`.
Without a window server (a launchd session other than Aqua, such as SSH) it
exits 2 and prints the manual steps; it never passes without starting the GUI.

Not verified by the script: clicking a session row (no headless driver), the
Terminal.app window itself, and double-clicking an app icon, which need the
`.app` bundle from issue #104. Manual procedure until then:

1. Run `pohunek service install` and confirm `pohunek service status`.
2. Write `~/.config/pohunek/gui.toml` with `pohunek_bin = "pohunek"` and
   `attach_terminal = "terminal-app"`.
3. Start `pohunek-gui` from Finder (with the bundle, double-click it).
4. Confirm the window opens, the local host appears, and the status line shows
   no configuration error.
5. Start a session, click Open, and confirm Terminal.app opens attached to it.
6. Quit the GUI and confirm `pohunek session list` still shows the session live.

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

## Activity

Activity is a modal over durable cross-host notification history. It offers
`Recent`, `Unread`, and `Archived` scopes, stays newest-first regardless of read
state, auto-marks a record read when opened, and can select its linked session.
Unread is presentation state, not an action queue.

The daemon remains the source of truth for notification lifecycle. The GUI
raises desktop notifications only for newly created `action_required` or
`error` records. Resolving a durable attention record removes that record from
current attention, but a session still detected as blocked remains in Needs you.
The daemon automatically removes old quiet, resolved, and archived history
according to policy while retaining unresolved actions and errors indefinitely.

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

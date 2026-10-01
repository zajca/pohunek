---
type: Guide
id: guide/gui
title: GUI setup
description: Configure and troubleshoot the native pohunek-gui desktop control plane.
source_kind: manual
intents: [setup, debug, help]
---

# GUI Setup

`pohunek-gui` is the native, session-first desktop control plane. It is a single
pane: a header with the Assistant, Activity, Hosts, and New session controls above
a prioritized cross-host session list. It does not embed a terminal: opening a session
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
attach_observe_ms = 1000               # template launch: report a quick failed exit
attach_script_max_age_secs = 3600      # attach_terminal: sweep never-run scripts
```

### `pohunek_bin` resolution

An absolute `pohunek_bin` is used as is and must be an executable file. A bare
name such as `pohunek` is looked up per attach, off the UI thread, in a search
path built from tiers, first match wins, duplicates dropped. The GUI's own
inherited `PATH` always comes first, sanitized by the same rules as the other
tiers (absolute, existing, trusted directories; relative and empty entries are
skipped; a refused directory is named with its reason when a lookup misses), so a
GUI started from a shell finds what that shell finds. On macOS it is followed by
one bounded non-interactive login-shell probe (Finder and launchd start apps
with only the system directories on `PATH`) that receives `HOME`, `USER`,
`LOGNAME`, `SHELL` (`/bin/zsh` when unset), and, when set, `ZDOTDIR` and
`XDG_CONFIG_HOME`, so a profile that branches on them or lives in a custom
location is read (an unusable value of one of them skips the probe and the
status line names it), and then by the fixed fallback directories (`~/.local/bin`,
`~/.cargo/bin`, Homebrew, `/usr/local`); elsewhere the fallback directories
apply only when the inherited `PATH` yields nothing. The policy is the shared
[environment-resolution](environment-resolution.md) one.
The resolved search path is cached; a miss discards it once and discovers again,
so a `pohunek` installed after the GUI started is found without a restart. An
unresolvable name is an attach error in the status line, never a guessed
default. When the lookup misses, the status line also names why the search
path is what it is (a failed or timed-out login-shell probe, an unusable
inherited `PATH`). `{bin}` in a template is always the resolved absolute path.
The same resolver and cached search path serve the launcher program of an argv
template and the notification command, so a Finder launch finds `kitty`,
`alacritty`, `wezterm`, or `notify-send` in Homebrew or `~/.local/bin` without a
login shell per use.

### Attach launch modes

- **`attach_command`, `attach_command_mode = "shell"` (default).** The template
  is rendered with shell-escaped `{bin}`, `{host}`, `{id}` values and run
  through `/bin/sh -c` (an absolute path, never a `PATH` lookup). Values cannot
  change the command structure, but the surrounding template is shell text, and
  a bare command inside it is found through the shell's own `PATH`, which a
  Finder launch keeps minimal; prefer argv mode on macOS.
- **`attach_command`, `attach_command_mode = "argv"` (preferred for new
  configurations).** The template is split into words with POSIX quoting rules
  and executed without a shell; `$VAR`, `~`, globs, and command substitution stay
  literal, and each placeholder value stays one argument. A bare program word
  such as `kitty` is resolved through the environment policy (see above), so it
  works from a Finder launch; a word containing `/` must be absolute. Use it for
  terminals that take a command as arguments.
- **`attach_terminal = "terminal-app"` (macOS).** The GUI writes a private
  self-deleting `.command` script and runs `open -a Terminal <script>` with an
  argument array. The script lives in an owner-only (`0700`) directory below the
  pohunek runtime directory, is created exclusively under an unpredictable name,
  removes itself first, then `exec`s `<resolved pohunek_bin> [--host=<host>]
  attach -- <id>` with every value single-quoted. No AppleScript is built, so no
  Automation permission prompt appears. Terminal runs the script in its own
  environment, so the script first exports an allowlist of the GUI's endpoint
  and configuration roots: `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`,
  `XDG_DATA_HOME`, `XDG_STATE_HOME`, and `XDG_CACHE_HOME`, each only when set in
  the GUI process to an absolute value (a relative value is ignored, as the XDG
  specification says; an absolute value is forwarded byte for byte, UTF-8 or
  not), single-quoted like the arguments. Nothing else of the
  GUI's environment is forwarded, and the script first `unset`s all five names, so
  a value from Terminal's own login environment can never replace the GUI's (a
  name the GUI does not set stays unset). The CLI therefore reaches the same
  daemon socket as the GUI. `HOME` is forwarded too when the GUI has an absolute
  one (the CLI derives every root without an XDG variable from it, and Terminal's
  login environment may carry another); unlike the XDG names it is never unset, so
  a GUI without a usable `HOME` leaves Terminal's own `HOME` in place. The script
  removes itself with `/bin/rm` and otherwise uses only shell builtins and the
  absolute program path, so it does not depend on Terminal's `PATH`. `open` gets `open_timeout_ms` to accept
  the request; a failure or timeout is an attach error (carrying the first line
  of `open`'s error output) and removes the script. A script Terminal never ran
  stays on disk until the next terminal launch, which removes owner-private
  `attach-*.command` files older than `attach_script_max_age_secs` through
  no-symlink-following filesystem operations; a file it cannot remove is a
  warning in the status line and never fails the launch.
  On any other operating system `attach_terminal` is a configuration error.

Every attach process starts in its own process group with null stdio and is
reaped by a helper thread. A template launch that exits non-zero, or by a
signal, within `attach_observe_ms` (a dead template, a missing terminal) is
reported in the status line instead of "attach command spawned"; a terminal that
keeps running past the window counts as started, and the reaper keeps waiting
for it. Closing the GUI never stops or kills attached terminals or supervised
sessions: the GUI sends no stop or kill on exit.

### Third-party terminals on macOS or Linux (argv mode)

These are documented forms of each terminal's command-line syntax, not exercised
in CI (CI covers the argv rendering, the launcher resolution, and the
`open`-based Terminal path with stand-in executables); verify them on your
machine. The launcher program is resolved through the environment policy, so a
Finder launch does not need it on the GUI's own `PATH`.

```toml
# kitty
attach_command_mode = "argv"
attach_command = "kitty -e {bin} --host={host} attach -- {id}"
# Alacritty
attach_command = "alacritty -e {bin} --host={host} attach -- {id}"
# WezTerm
attach_command = "wezterm start -- {bin} --host={host} attach -- {id}"
```

For the local host `{host}` is empty, and an empty `--host=` selects the local
daemon.

`pohunek_bin`, `attach_command`, and `notification_command` must not be blank or
contain a NUL byte; that fails at load with an error naming the field.

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
  positional arguments. When the command's file name is `notify-send`, `--`
  precedes them so daemon-supplied text cannot be read as an option.
- **macOS default:** `/usr/bin/osascript` running
  `-e 'on run argv' -e 'display notification (item 2 of argv) with title (item 1 of argv)' -e 'end run' -- <title> <body>`.
  Title and body are only argv items after `--`, never script text, so quotes,
  `$()`, newlines, Unicode, and a leading `-` are inert.
- A command that is not an absolute path is resolved once, on the first
  notification, with the same environment policy as `pohunek_bin`. A completed
  lookup is kept for the session and shared by concurrent notifications: a found
  path, or a real not-found or untrusted verdict (restart the GUI after
  installing the command). A lookup still running when
  `notification_timeout_ms` passes is not a verdict: that notification is
  unavailable, the lookup finishes in the background, and the next notification
  uses or awaits its result. A command that cannot be resolved is reported as
  unavailable, not treated as working.
- Each notification gets `[gui] notification_timeout_ms` (default 5000, zero is
  rejected); a backend that does not exit by then is killed and reaped. It runs
  in its own process group. At the deadline the whole group is killed while the
  leader is still unreaped, so a wrapper's background children die with it. After
  a normal exit nothing is signalled: a child the wrapper backgrounded and left
  running survives, because the group id of an already reaped leader can belong
  to an unrelated group by then. A descendant that starts its own session or
  group escapes the deadline kill.
- The status line shows a message when the failure reason changes (unresolvable
  command, spawn failure, timeout, non-zero exit or signal) and one when it
  recovers; an identical repeated failure stays silent, but a different reason,
  such as a definitive not-found after an earlier timeout, replaces the old text.

What cannot be confirmed: `osascript` exits 0 whether the notification was shown
or suppressed by System Settings (Notifications, Focus), and an unbundled binary
cannot ask `UNUserNotificationCenter` for its authorization. "Submitted" means
the backend accepted the request, not that it was displayed, and the GUI cannot
detect that a user denied notifications. Denial detection needs the app bundle
and a native `UNUserNotificationCenter` call, which the GUI does not make yet
(tracked by issue #102).

## macOS app bundle

The release workflow builds the macOS GUI archive
`pohunek-gui-<version>-aarch64-apple-darwin.tar.gz` around `Pohunek.app` when
the repository's `macos-signing` credentials exist: signed with a Developer ID
Application certificate (hardened runtime, secure timestamp), notarized, ticket
stapled. macOS is not yet a published platform (see the README); once an archive
is published, copy `Pohunek.app` to `~/Applications` (or `/Applications`) and
open it from Finder.

- Bundle identifier `io.github.zajca.pohunek.gui`, executable `pohunek-gui`,
  minimum macOS 14.0, Apple Silicon only.
- The bundle holds no CLI, daemon, or worker. The app finds the installed
  `pohunek` the way a terminal-launched GUI does: `pohunek_bin` in `gui.toml`
  when set, otherwise a login-shell `PATH` lookup, then `~/.local/bin`, where
  `pohunek service install` places it. Install the daemon archive first.
- It reads the same `~/.config/pohunek/gui.toml` and the same keychain items as
  the unbundled binary.
- A development build (`packaging/macos/package --development gui ...`) is
  unsigned, named `...-unsigned-development`, and never released.
- If Gatekeeper refuses an app that was downloaded with a browser, verify it
  with `spctl --assess --type execute --verbose=4 Pohunek.app` and
  `codesign --verify --deep --strict Pohunek.app`; do not disable Gatekeeper or
  strip quarantine from other files. A signed and notarized app needs neither.
- The release's `Pohunek.app` has the bundle identity a notification
  authorization check needs, but the GUI does not call
  `UNUserNotificationCenter` yet (see the notification limits above).

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

Empty groups are hidden; when no session is loaded the pane shows one empty-state
message. Rows are stable within a group by project label, session name (or id),
host id, and session id. Activity or runtime changes may move a row between
groups, but do not reorder unrelated rows inside the same group. Every row leads
with a prominent project chip showing the project label, falling back to the
project id or `unassigned`; the row then shows the session name, branch, and a
muted host, agent, state, and activity line.

A project filter chip row above the list offers `All` plus one chip per project
that currently has sessions, each with its session count. Selecting a chip
restricts every group to that project; the filter is the only project browsing
surface and is not persisted.

The header's Hosts control opens a modal listing every host with its connection
state, last error, and the read-only governance projection. A configuration
error is shown as a banner above the list.

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

The header holds Assistant, Activity, Hosts, and New session. New session is
enabled whenever any project is known on any host. The Start
session and Assistant modals choose their own target project through a Project
select (see Session and Assistant Launch), so no project has to be selected
beforehand.

Default global bindings:

| Name | Default | Behavior |
|------|---------|----------|
| `open_inbox` | `i` | Open the Activity modal. |
| `open_selected_session` | `o` | Open or resume the selected session in a terminal. |
| `show_selected_session` | `enter` | Open the selected session detail modal. |
| `open_keymap_help` | `shift+?` | Show the effective keymap. |
| `new_session` | `n` | Open the Start session modal whenever any project exists. |
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
Start session and Assistant modals. Focus cycles through the Project select, the
other leading select fields, the prompt/name inputs, and visible Advanced branch fields, never into
controls behind the modal overlay. On a focused select, Up or Down opens its
options, the arrow keys move the option cursor, and Enter confirms the choice.
Ctrl+Enter (and Command+Enter on macOS) submits either launch form from any
focused field. A Command-modified key never triggers a modal binding: Cmd+O,
Cmd+J, or Cmd+K do not act as bare `o`, `j`, or `k`, and Cmd+Tab is not form
navigation.

### macOS input review

The GUI relies on Iced for every text-entry behavior, and the shell only sees
events no widget consumed (the `keyboard::subscription` function listens through `keyboard::listen`,
which passes ignored events only), so:

- **Clipboard and editing shortcuts:** the text inputs and the prompt editors
  are Iced widgets, which map Cmd+C/V/X/A and Option+arrows on macOS and
  Ctrl+... elsewhere. No GUI code reads or writes the clipboard directly.
- **Read-only selectable text** (`read_only_binding` in
  `crates/gui/src/view/selectable_text.rs`) keeps Copy, select, and cursor movement and drops Cut, Paste, and edits, so
  Cmd+C copies and Cmd+V/X do nothing there.
- **Browser opening:** there is none. The GUI contains no URL-opening code, and
  `open_url_command` is not read, so there is nothing to port to `open(1)`.
- Nothing else needed to change for text selection or input handling.

## macOS launch verification

`scripts/acceptance/macos-gui-launch` verifies, on a Mac logged in at the GUI:
the real `pohunek-gui` starts from a Finder-like environment (`env -i`, only
`HOME`, `USER`, `LOGNAME`, and `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, plus XDG
variables that isolate the run) next to an isolated daemon and shell session
and stays up for `GUI_ACCEPT_UP_S` seconds without exiting (its stderr is shown;
only a panic or fatal error fails the check); ending it with SIGTERM leaves the daemon, session worker, and session
child unchanged (same PIDs and start times) and the session live. It needs `GUI_ACCEPT_BIN_DIR`, a
directory with `pohunek`, `pohunekd`, `pohunek-sessiond`, and `pohunek-gui`.
Without a window server (a launchd session other than Aqua, such as SSH) it
exits 2 and prints the manual steps; it never passes without starting the GUI.

Not verified by the script: the attach launchers (no session-row click can be
driven headlessly; the pohunek-gui unit tests that CI runs on macOS cover them),
the Terminal.app window itself, and double-clicking an app icon, which need a
person at the Mac and the signed `Pohunek.app`. Manual procedure:

1. Run `pohunek service install` and confirm `pohunek service status`.
2. Write `~/.config/pohunek/gui.toml` with `pohunek_bin = "pohunek"` and
   `attach_terminal = "terminal-app"`.
3. Double-click `Pohunek.app` in Finder.
4. Confirm the window opens, the local host appears, and the status line shows
   no configuration error.
5. Start a session, click Open, and confirm Terminal.app opens attached to it.
6. Quit the GUI and confirm `pohunek session list` still shows the session live.

## Session and Assistant Launch

The Start session modal opens with a Project select as its first field. Options
are every known project on every host, labeled with the host (`label · host`).
The project is preselected from the active project filter, else from the
selected session's project, else the only project. Choosing another project
clears the template, reloads that project's actions, and re-validates the agent
against the target host's runtimes. The modal calls `project.actions`, resolves the chosen action with
`project.action`, renders through the shared prompt crate, and creates the
session with `session.new`. A blank session uses provider `none`. Runtime choices
come from `host.inspect` and fail closed when a runtime is unavailable or
unsupported.

The Assistant entry opens a native launch modal with the same Project select as
its first field and the same preselection rules. The shared
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
- `crates/gui/src/view/hosts.rs` — Hosts modal (connection state, errors, governance).
- `crates/gui/src/keyboard.rs` — supported bindings and routing.
- `crates/gui-core/src/state.rs` — grouping and capability-derived action model.
- `crates/gui-core/src/ui_state.rs` — persisted window and selection state.

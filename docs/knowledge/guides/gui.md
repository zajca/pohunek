---
type: Guide
id: guide/gui
title: GUI setup
description: Configure and troubleshoot the native pohunek-gui desktop control plane.
source_kind: manual
intents: [setup, debug, help]
---

# GUI Setup

`pohunek-gui` is the native desktop control plane. It lists hosts, sessions,
projects, worktrees, and agent state through the Rust SDK. It does not embed a
terminal: opening a live session delegates to the user's terminal by spawning the
configured `attach_command`. Opening a terminal session first calls
`session.resume` when native resume metadata is present, then attaches to the
relaunched PTY.

Use this guide when the user asks to configure or debug the GUI.

## Preconditions

Start with the normal local setup checks:

1. Run `pohunek doctor --json`.
2. Run `pohunek daemon start --detach` if the daemon is not running.
3. Run `pohunek health --json` or `pohunek status --json`.
4. Run `pohunek host inspect local --json` to confirm agent and worktree
   capabilities.

The GUI also needs a graphical session. On Linux v1 that means a Wayland session
with a reachable compositor. It is Wayland-only: if `WAYLAND_DISPLAY` is missing
or empty, `pohunek-gui` exits before starting Iced. If `DISPLAY` is set but
`WAYLAND_DISPLAY` is not, the user is in an X11-only environment and must start
the GUI from their normal Wayland desktop shell instead.

## Configuration File

The GUI reads `gui.toml` from the shared Pohunek config directory:

- `$XDG_CONFIG_HOME/pohunek/gui.toml` when `XDG_CONFIG_HOME` is set.
- `~/.config/pohunek/gui.toml` otherwise.

Minimal local configuration:

```toml
pohunek_bin = "/path/to/pohunek"
attach_command = "$TERMINAL -e sh -c 'exec {bin} attach --host {host} {id}'"

[gui]
connect_timeout_ms = 2000
request_timeout_ms = 5000
reconcile_secs = 30
backoff_initial_ms = 1000
backoff_max_ms = 30000
```

Use an absolute `pohunek_bin` when the GUI may be launched from a desktop
environment with a different `PATH`. For a source checkout, build first and point
at `target/debug/pohunek` or `target/release/pohunek`.

The `attach_command` template supports exactly these placeholders:

- `{bin}`: the configured Pohunek CLI binary.
- `{host}`: the host value to pass to `pohunek attach`; empty for the local
  daemon.
- `{id}`: the selected session id.

Keep attach delegation external. Do not configure or recommend an embedded
terminal path; that is intentionally out of scope for the GUI.

Two more top-level keys are optional and default to the freedesktop Linux CLI
tools of the same purpose:

- `notification_command` (default `notify-send`): spawned with the
  notification title and body as two argv arguments.
- `open_url_command` (default `xdg-open`): spawned with a provider item's URL
  (Linear issue, GitHub PR/issue) as a single argv argument when the operator
  clicks "Open in browser" in an item detail modal. Always argv-spawned, never
  a shell, so the URL cannot inject shell syntax.

## Running

From the repository during development:

```sh
cargo run -p pohunek-gui
```

From an installed build:

```sh
pohunek-gui
```

Install `pohunek-gui` from the GUI component release archive.

If the GUI starts but shows no sessions, verify daemon health and `session.list`
first. If host discovery fails, the GUI should still try the local host and
surface a per-host error instead of treating the whole app as failed.

## Navigation

The right pane is a persistent tab bar over a body that switches with the
active tab: `1 Detail · 2 Linear · 3 GitHub · 4 Worktrees · 5 Review`. Detail is
the selection-driven session/project/host/start-work landing; Linear, GitHub,
Worktrees, and Review are full-tab bodies scoped to the current project
(previously stacked underneath a project selection as `project_pane`
sections). A context chip at the right end of the strip shows the tabs'
project scope as `host / project-label`, or just the resolved host's
connection dot when no project is in scope.

Tabs 2-5 need a project: the scope resolves from a selected project directly,
or from a selected session's linked project. With no project in scope, those
tabs render with no click handler (a "Select a project" tooltip explains why)
and their body shows a "select a project" empty state. Unlike Linear/GitHub,
Review's body shows its own "no review open" placeholder once a project is in
scope but nothing has been opened yet — see [Review](#review).

Selecting a session anywhere — the workspace tree, the Agents monitor, or the
Inbox's Open-session action — always force-switches to the Detail tab, so
triage never lands behind a Linear/GitHub/Worktrees tab. Selecting a project
or host leaves the operator's chosen tab as-is. The active tab persists across
restarts in `UiState::active_tab` (`RightTab`).

## Keyboard Shortcuts

The GUI is keyboard-first. Global shortcuts fire only while no modal is open;
modal shortcuts apply only while a modal is open. The default keymap is:

| Context | Keybinding name | Default | Behavior |
|---------|-----------------|---------|----------|
| Global | `tab_detail` | `1` | Switch to the Detail tab. |
| Global | `tab_linear` | `2` | Switch to the Linear tab when a project is in scope. |
| Global | `tab_github` | `3` | Switch to the GitHub tab when a project is in scope. |
| Global | `tab_worktrees` | `4` | Switch to the Worktrees tab when a project is in scope. |
| Global | `tab_review` | `5` | Switch to the Review tab when a project is in scope. |
| Global | `open_inbox` | `i` | Open the Inbox. |
| Global | `cycle_blocked` | `b` | Select the next blocked agent, wrapping through the blocked subset. |
| Global | `open_selected_session` | `o` | Open the selected provider item when a provider tab is active; otherwise open the selected session in a terminal. |
| Global | `activate_selection` | `enter` | Activate the selected provider item or selected session; on the Review tab, opens the inline comment editor for the currently selected diff line. |
| Global | `open_keymap_help` | `shift+?` | Open the effective keyboard shortcut table. |
| Global | `list_up`, `list_up_arrow` | `k`, `arrowup` | Move the active provider list selection up; on the Review tab, moves the diff-line cursor to the previous line, walking backward across hunks and files. |
| Global | `list_down`, `list_down_arrow` | `j`, `arrowdown` | Move the active provider list selection down; on the Review tab, moves the diff-line cursor to the next line, walking forward across hunks and files. |
| Global | `focus_search` | `/` | Focus the active Linear/GitHub provider search box. |
| Global | `new_session` | `n` | Open the "Start a session" modal. |
| Global | `open_assistant` | `a` | Open the "Start assistant" modal. |
| Global | `refresh_tab` | `r` | Refresh the active tab (`project.show`, Linear issues, GitHub PRs+issues, or the Review tab's diff). |
| Modal | `modal_back` | `escape` | In Inbox message detail, step back to the list; otherwise close the modal. |
| Modal | `modal_primary` | `enter` | Run the modal primary action, or open the selected Inbox row from the list. |
| Modal | `modal_primary_with_terminal` | `shift+enter` | In Inbox message detail, open the linked session and also open it in a terminal. |
| Modal | `modal_list_up`, `modal_list_up_arrow` | `k`, `arrowup` | Move the Inbox list selection up. |
| Modal | `modal_list_down`, `modal_list_down_arrow` | `j`, `arrowdown` | Move the Inbox list selection down. |
| Modal | `modal_open_linked_session` | `o` | Jump from the selected Inbox row to its linked live session. |

Add an optional top-level `[keybindings]` table to `gui.toml` to remap any of
those binding names. Overrides are partial: names that are not listed keep their
default chord.

```toml
[keybindings]
open_inbox = "ctrl+i"
refresh_tab = "ctrl+r"
modal_primary_with_terminal = "shift+enter"
```

Key strings are case-insensitive. They can be a one-character key (`i`, `1`,
`/`) or a named key (`escape`, `enter`, `tab`, `space`, `backspace`, `delete`,
`home`, `end`, `pageup`, `pagedown`, `arrowup`, `arrowdown`, `arrowleft`,
`arrowright`) with optional `ctrl`, `alt`, `shift`, and `logo` modifiers joined
by `+`, such as `ctrl+r` or `shift+enter`.

The GUI fails fast on unknown keybinding names, invalid key strings, or two
different actions using the same chord in the same context. Global and modal
contexts are independent, so the same chord may be reused once globally and once
in a modal. Modal shortcuts support bare keys, plus the dedicated
`shift+enter` path for `modal_primary_with_terminal`; other modified modal
chords are rejected because the modal event router intentionally ignores those
modifiers.

Iced has no direct "is a text input focused" query, but a focused
`text_input`/`text_editor` already consumes (captures) the key presses it
handles — typed characters, Backspace, arrows, and Enter when the field has
`on_submit` — so `keyboard::listen()` never even delivers those presses to
the shortcut router; typing into a field cannot trigger a global shortcut.
`Escape` is a partial exception: it unfocuses a field on its own without
closing anything, so closing a modal from inside a focused field takes two
`Escape` presses.

## Project And Worktree Management

The GUI must use existing daemon methods:

- `project.list`
- `project.add`
- `project.show`
- `project.rename`
- `project.remove`
- `session.new`
- `session.inspect`
- `session.resume`
- `session.stop`
- `session.remove`
- `session.set_metadata`
- `session.rename`
- `notification.list`
- `notification.update`
- `notification.delete`
- `subscribe`
- `assistant.materialize`

Worktree creation is represented by `session.new` with a project or repo and a
branch. There is no standalone worktree daemon method. When explaining or fixing
GUI worktree behavior, preserve that protocol boundary.

## Session Names

Every session-creation surface (the Start modal and the provider-launch modal)
offers an optional name field, so a session can be named at any creation; the
name flows through `session.new`'s `name` parameter. The session detail pane also
renames an existing session through `session.rename` (and clears it). The display
name leads the row in the workspace tree and the Agents monitor, falling back to
the session id when unset. The name is cosmetic and never changes targeting.

## Session Launch

The native `Start session` modal creates a session on the selected host and
project through `session.new`. Its runtime picker sends the selected wire
string as `agent`; the options come from the selected host's `supported_agents`
(seeded from `host.inspect`), which lists the three compiled base kinds
(`shell`, `codex`, `claude`) plus every resolvable host agent profile (e.g.
`claude-otel`) — see [Agent Profiles](../concepts/agent-profiles.md). If the
snapshot hasn't loaded `supported_agents` yet (or an older daemon doesn't
answer `host.inspect`), the picker falls back to just the three base kinds.
`shell` starts the daemon host's configured default shell and uses the same
plain-shell runtime path as `pohunek session new` without `--agent`.

## Assistant Launch

The left workspace rail includes an `Assistant` entry above the workspace tree.
It opens a native `Start assistant` modal rather than shelling out to the CLI.
The modal chooses:

- assistant intent: `help`, `setup`, `project`, `update`, or `debug`;
- agent runtime/profile: `Auto`, `pohunek-assistant`, `codex`, `claude`, or a
  profile name observed in existing sessions;
- request text, sent as the assistant's initial prompt request;
- advanced branch/base-branch overrides;
- explicit snapshot options (`No snapshot`, `Degraded`).

Assistant launch is scoped to the selected project. If a session is selected,
the GUI uses that session's `project_id`; if no project context is selected, the
launch fails before contacting the daemon. The shared `gui-core::assistant`
launcher performs host inspection, agent selection, snapshot creation,
knowledge materialization, prompt composition, and finally `session.new` with the
composed prompt as `input`. The resulting session is applied through the normal
`SessionCreated` path, so it opens through the configured `attach_command`.

The daemon protocol remains unchanged: GUI assistant launch uses existing
`host.inspect`, `assistant.materialize`, and `session.new` methods. Do not add a
daemon-side `assistant.launch` method unless the architecture changes.

## Agents Monitor

The Agents monitor lists every session across hosts with a per-row activity dot
and working/blocked/idle counts. Rows are ordered by the stable `(host, session)`
identity, never by activity — activity flips constantly as agents work, and
ordering on it would reshuffle rows under the operator's cursor. Each row shows
the name (or id), agent, project, branch, and activity word. The left-rail
monitor is at least 360px tall so roughly five session rows are visible; older
persisted UI state below that height is raised when the GUI loads.

## Inbox

The native GUI Inbox is a durable notification view across all configured hosts.
Each host snapshot seeds recent records through `notification.list`, and the
subscription stream keeps the Inbox current with `notification_created`,
`notification_updated`, and `notification_deleted` events. A host whose daemon
does not support notifications may still connect; notification seeding is
non-fatal for the GUI. The GUI does not deduplicate notification rows locally;
it renders the daemon's durable records and lifecycle events as the source of
truth.

The Inbox is a modal, not a pane: the left-rail Inbox button and each host's
`inbox N` tree row open it over the workspace rather than replacing the detail
pane. It has two layers — a notification list, and one message's detail —
and the Back button (or closing the modal) always returns to the list.

The list layer narrows by two controls instead of five filter axes: a
`Needs action | All | Archived` scope (`Needs action`, the default, is unread
OR severity `action_required`/`error`, excluding anything archived) and a host
picker shown only when 2+ hosts have notifications. Rows are sorted for
triage — unresolved agent-blocked/approval-required records pinned first,
then unread by recency, then read — not by raw `created_at`, so marking a
record read or acknowledged does not reshuffle rows under the cursor. Deleted
records are removed from the Inbox when the daemon reports deleted status or a
delete event; if the record currently open in the message layer is deleted,
the modal steps back to the list instead of showing a dead end.
Daemon-side resolve and supersede processing controls what remains actionable:
when the operator resumes a session, visible `attention:<session_id>` and
`turn:<session_id>` records are acknowledged and fall out of `Needs action` but
remain visible in `All`; when a newer turn or attention record supersedes an
older unread `turn_completed`, the older record is also acknowledged with a
`superseded_by` link. At most one unread `turn_completed` row is visible for a
session.

Opening a message auto-marks it read (there is no separate "Mark read"
action) and shows its body, metadata, and actions. When the record has a
`session_id` and that session is still present on the same host, the message
layer offers a primary Open session action that closes the modal and selects
the session; if the linked session is gone, explanatory text replaces the
button so the record is never a dead end. Remaining actions call
`notification.update` to acknowledge or archive, and `notification.delete` to
delete.

Fresh durable notifications are the single source for desktop OS notification
intents. The GUI raises an OS notification only for newly created records whose
severity is `action_required` or `error`; informational, success, and warning
records land in the Inbox silently. Pending debounced records do not raise OS
notifications until the daemon emits a durable `notification_created` event, and
`notification_updated` events from resolve or supersede processing do not
re-raise desktop notifications.

## Prompt Management

The native GUI can browse project actions and prompt templates in read-only
form. It must resolve that data through the selected host's daemon, using the
same project layers that the daemon resolves for CLI project commands. Do not
read prompt files directly from the GUI process to explain or implement this
flow.

The supported daemon methods are:

- `project.actions` to list actions for a project.
- `project.action` to resolve a named action recipe.
- `project.prompt` to resolve a named prompt template.
- `session.new` to launch the rendered action prompt with `input` set to the
  rendered prompt.

Preview rendering uses the shared `crates/prompt` renderer. A GUI-rendered
preview should be byte-identical to `pohunek prompt render` for the same
template, provider, item id, and provider JSON. Launching from a preview should
create one session on the selected host and project. The GUI should not attach a
raw stream and should not embed a terminal for that session.

## Provider Integration

The native GUI v1 includes provider browse and launch flows for Linear issues
and GitHub pull requests. Provider integration lives in the GUI application and
`gui-core`; the daemon still sees only opaque session input, branch values, and
metadata. Do not add daemon methods or protocol types to implement provider UI.

Provider configuration belongs in `gui.toml` under `[providers]`:

```toml
[providers.linear]
token_key = "linear-token-ref"
endpoint = "https://api.linear.app/graphql"
token_timeout_ms = 5000

[providers.github]
gh_bin = "gh"
timeout_ms = 20000
```

## Provider Filters

Both provider panels expose a named-filter picker (for example *My PRs*,
*Ready to merge*, *Assigned to me*). Filters are resolved entirely client-side
from three layers, highest priority first:

1. **Project layer** — the selected project's in-repo
   `<repo_root>/.pohunek/providers.toml`, read by the GUI from the project's
   repository checkout. It shadows the host layer per filter name (a project
   filter replaces a host filter with the same name; project-only filters append
   after the host ones) — the same in-repo-over-host rule prompts and actions
   use. Because the GUI reads this file locally, it only applies to projects on
   the local host; a remote project's path is not readable, so only the host and
   built-in layers apply there.
2. **Host layer** — `filters` arrays under `[providers.github]` /
   `[providers.linear]` in `gui.toml`.
3. **Built-in defaults** — used per provider whenever the merged set for that
   provider is empty, so the picker is never empty.

A **GitHub** filter is a raw `gh pr list` search query plus an optional state;
it is passed through as `--search` / `--state` and so applies to pull requests
(issues keep a plain listing with a local text filter). A **Linear** filter is a
raw Linear `IssueFilter` object passed verbatim as the `$filter` variable to the
top-level `issues(filter:)` query, so filters are not limited to issues assigned
to the viewer — any team-wide filter the `IssueFilter` schema supports works.

Host `gui.toml`:

```toml
[[providers.github.filters]]
name = "Ready to merge"
search = "review:approved"
state = "open"            # open (default) | closed | merged | all

[[providers.linear.filters]]
name = "Team active"
filter = { state = { type = { in = ["started"] } } }
```

In-repo `<repo_root>/.pohunek/providers.toml`:

```toml
[[github]]
name = "Release blockers"
search = "label:release-blocker"

[[linear]]
name = "My active"
filter = { assignee = { isMe = { eq = true } }, state = { type = { in = ["started"] } } }
```

Filter definitions hold no secrets and never enter session metadata or logs;
they are plain query strings and `IssueFilter` objects.

Linear uses `token_key` as a keyring entry reference. The GUI reads the token at
call time through the keyring boundary and must never offer a token input field.
Store the raw Linear personal API key in that keyring entry; do not prefix it
with `Bearer`, which Linear reserves for OAuth access tokens.
`token_timeout_ms` is required when Linear is configured so a stuck keyring
backend cannot leave the provider task pending forever.

GitHub uses the `gh` CLI for all provider reads. Do not read GitHub auth files,
store GitHub tokens, or add token fields to GUI configuration. If `gh` is
missing, unauthenticated, or returns invalid JSON, surface the error in provider
state and leave existing sessions, projects, and workspace state intact.

Provider list and status requests are stateful GUI operations. The core state
must guard async completions with request ids so stale success or failure
responses cannot overwrite newer data. GitHub provider state is scoped by both
project id and repository root because `gh` commands run in the selected
project's checkout.

Launching a provider item uses the existing project prompt/action surface:

1. Resolve the selected project action with `project.action`.
2. Resolve the prompt template with `project.prompt`.
3. Render through `crates/prompt`; the rendered bytes must match
   `pohunek prompt render` for the same context.
4. Create exactly one session with `session.new`, setting `input` to the
   rendered prompt and branch to the provider/action result.

At session creation, linked provider launches write only these metadata keys:

- `link.provider = "linear" | "github"`
- `link.kind = "issue" | "pull_request"`
- `link.id`
- `link.url`
- `link.branch`

This schema is shared, not GUI-owned: it lives once in `pohunek_prompt::link`
(`crates/prompt/src/link.rs`), and `crates/gui-core` re-exports the types
rather than defining its own copy. The launch scripts (`pohunek-launch-issue`,
`pohunek-launch-pr`) write the same five keys through the `pohunek prompt link`
CLI subcommand, so a link made by a script and a link made by the GUI are
byte-identical for the same work item; see
[launcher](launcher.md#work-item-links).

The daemon treats those values as opaque. Do not write provider tokens, raw
provider payloads, GraphQL responses, `gh` output, or secret-bearing config into
metadata, logs, snapshots, fixtures, or prompt text.

GitHub PR sessions may display PR checks and review status next to the live
agent badge. That status is best effort and should degrade to an unknown/error
state when `gh` is unavailable or unauthenticated. GitHub issues can be browsed
for context, but native provider launch is currently implemented for GitHub pull
requests and Linear issues.

## Review

The Review tab (`5`, Track D.6) browses a change set — a session's worktree
diff against its base, or a GitHub pull request's diff — and turns inline
comments into a new session that acts on exactly the reviewed code.

Open a review from:

- A session's Detail pane: the "Review changes" button appears once the
  session has a bound worktree, and opens that session's worktree-vs-base
  diff (`session.diff`).
- A GitHub pull request modal: the "Review diff" button fetches the PR's diff
  with `gh pr diff`.

Opening a review from either entry point switches to the Review tab,
replacing whatever review was previously open for that host — there is one
active review per host at a time, the same scoping Linear/GitHub browsing
uses. It resumes the most-recently-updated persisted draft for the exact
same source (same session, or same pull request) when one exists, and starts
a brand-new empty draft only when it does not; a review that has already
been dispatched is never resumed, so returning to a dispatched session's
worktree always opens a fresh draft rather than the one that was already
sent.

Browsing: the left pane lists the changed files with a status glyph
(`M`/`A`/`D`/`R`/`B` for modified/added/deleted/renamed/binary) and a
per-file comment count; selecting a file shows its hunks in the right pane as
a scrollable unified diff with old/new line-number gutters and hunk headers.
`j`/`k`/the arrow keys step the selected line forward or backward across every
hunk in the currently selected file's diff, matching provider-list navigation
elsewhere in the GUI (see [Keyboard Shortcuts](#keyboard-shortcuts)); clicking
a file or a line selects it directly.

Commenting: click the selected line's "+ Comment" affordance, or press
`Enter`, to open an inline editor under that line; `Enter` in the editor
saves, `Escape`/Cancel discards. Existing comments render inline under their
line with Edit/Delete actions, and the same comments are collected in a
review tray below the diff pane with a running count. Every add, edit, or
delete is persisted immediately to that review's JSON file — there is no
separate "save" step for the draft as a whole, and reopening the same
session's or pull request's review later (see above) picks the comments back
up.

Dispatch: the tray's "Dispatch as session…" action opens a modal with an
agent picker (the same host `supported_agents` list as the Start modal,
including host agent profiles, seeded with the source session's current agent
and freely overridable), a rendered prompt preview (or the
render error, e.g. a missing `review.tmpl`), and — when the source session's
agent is currently `working` — a warning that dispatching now may interrupt
it. Confirming dispatch calls `session.new` with the picked agent, `cwd` set
to the source session's worktree path (the *same* worktree, not a new
checkout — git refuses a second worktree on a branch that is already checked
out), the rendered review as `input`, and
metadata `review.source` (this review's id) and `review.dispatched_at`
(RFC3339) alongside every `link.*` key already present on the source
session, copied verbatim. On success the review is marked dispatched and
shows its target session id instead of the dispatch button; on failure the
draft is untouched and the modal shows the error. A pull-request-sourced
review has no source session or worktree to dispatch into, so its "Dispatch
as session…" action is disabled with a tooltip explaining that the PR must be
reviewed from an existing session's worktree instead.

States: while a diff is loading the tab shows a fetching placeholder; a diff
with no changes shows "No changes vs `<base>`"; a fetch failure shows the
daemon or `gh` error text; and a diff that hit the size cap
(`MAX_SESSION_DIFF_BYTES`) shows a banner above the file list noting that
later files in the change set were cut and are not shown.

Dispatch renders `~/.config/pohunek/prompts/review.tmpl` through the shared
`crates/prompt` renderer, with `${branch}`, `${source}`, `${comments}`, and
`${comment_count}` available (`${provider}` is always `"review"`). A missing
`review.tmpl` is a typed error, not a silent default — run `pohunek setup
config` to install the starter template alongside `issue.tmpl`/`pr.tmpl`.

## Secrets

Do not put token values in `gui.toml`, session metadata, prompts, snapshots, or
logs. GUI provider configuration should use references such as keyring entry
names. For Linear, the intended GUI shape is a key name like
`linear.token_key`, not the token value.

If a user asks the assistant to edit GUI configuration, inspect or write only
non-secret config values. Never read `.env`, keyring contents, credentials, or
tokens to make the GUI start.

## Source Verification

When behavior must be checked against implementation, inspect:

- `crates/gui/src/main.rs` for config loading, attach spawning, and Iced shell
  behavior, provider task spawning, prompt management controls, provider panels,
  and Inbox rendering/actions.
- `crates/gui/src/view/inbox.rs` for Inbox list/detail rendering, modal
  controls, and notification action buttons.
- `crates/gui/src/view/detail.rs` for the right-pane tab bar (`RightTab`
  routing, the context chip, and the disabled-without-scope tab styling) and
  the Detail tab's session/project/host/start-work bodies.
- `crates/gui/src/keyboard.rs` for the keyboard shortcut router (the
  focus-guard rationale, the global/modal key tables, and the blocked-agent
  cycling) and `AgentMonitor::blocked_at` in `crates/gui-core/src/state.rs`
  for the cycling logic it calls into.
- `crates/gui/src/view/project.rs` and `crates/gui/src/view/provider.rs` for
  the Worktrees and Linear/GitHub tab bodies the tab bar promotes to full
  panes.
- `crates/gui-core/src/ui_state.rs` for the persisted `RightTab` enum and the
  legacy-`DetailTab`-tolerant `UiState::active_tab` load path.
- `crates/gui-core/src/lib.rs` for headless state transitions, SDK requests,
  prompt/action state, prompt preview rendering, provider request state, linked
  metadata helpers, Inbox notification state, OS notification intents, and
  attach command rendering.
- `crates/gui-core/src/state.rs` for notification snapshot/event application,
  Inbox scopes, and OS notification intent derivation.
- `crates/daemon/src/notifications/mod.rs`,
  `crates/daemon/src/notifications/store.rs`,
  `crates/daemon/src/notifications/coordinator.rs`, and
  `crates/daemon/src/notifications/projector.rs` for durable notification
  dedupe, debounce, resolve-on-resume, and supersede semantics.
- `crates/gui-core/src/providers/linear.rs` for Linear GraphQL requests,
  keyring-token lookup boundaries, and token lookup timeouts.
- `crates/gui-core/src/providers/github.rs` for `gh` command execution, timeout
  handling, JSON parsing, and stderr redaction.
- `crates/gui-core/src/providers/filters.rs` for the named-filter types,
  built-in defaults, and the host/project layer merge (in-repo-over-host
  shadowing) used by both provider pickers.
- `crates/gui-core/tests/loopback.rs` for loopback coverage of host-resolved
  prompt/action browse, preview, provider launch, linked metadata persistence,
  and notification Inbox behavior.
- `crates/gui-core/tests/linear_provider.rs` and
  `crates/gui-core/tests/github_provider.rs` for provider fixtures, fake token
  sources, fake `gh` scripts, parsing coverage, timeout behavior, and error
  paths.
- `crates/prompt/src/lib.rs` for prompt rendering rules shared by CLI and GUI.
- `crates/prompt/src/link.rs` for the shared `link.*` session-metadata schema
  (types, validation, `branch_from_provider_json`) `gui-core` re-exports.
- `crates/cli/tests/gui_prompt_parity.rs` for byte-identical GUI/CLI prompt
  rendering and GUI/script `link.*` coverage.
- `docs/phases/06-native-app.md` for Track D milestone scope and constraints.

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
project through `session.new`. Its runtime picker sends the selected base-kind
wire string as `agent`; supported built-in choices are `shell`, `codex`, and
`claude`. `shell` starts the daemon host's configured default shell and uses the
same plain-shell runtime path as `pohunek session new` without `--agent`.

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
non-fatal for the GUI.

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
records land in the Inbox silently. Updates do not re-raise desktop
notifications.

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

The daemon treats those values as opaque. Do not write provider tokens, raw
provider payloads, GraphQL responses, `gh` output, or secret-bearing config into
metadata, logs, snapshots, fixtures, or prompt text.

GitHub PR sessions may display PR checks and review status next to the live
agent badge. That status is best effort and should degrade to an unknown/error
state when `gh` is unavailable or unauthenticated. GitHub issues can be browsed
for context, but native provider launch is currently implemented for GitHub pull
requests and Linear issues.

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
- `crates/gui-core/src/lib.rs` for headless state transitions, SDK requests,
  prompt/action state, prompt preview rendering, provider request state, linked
  metadata helpers, Inbox notification state, OS notification intents, and
  attach command rendering.
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
- `crates/cli/tests/gui_prompt_parity.rs` for byte-identical GUI/CLI prompt
  rendering coverage.
- `docs/phases/06-native-app.md` for Track D milestone scope and constraints.

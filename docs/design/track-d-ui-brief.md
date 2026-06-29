# Track D — UI design brief (`pohunek-gui`)

A brief for a UI/UX designer. It states **what the app is**, **the full surface of
data and actions** it can show (taken from the real protocol), the **component
inventory** to design, the **states/flows** that matter, and the **technical
constraints** (so the result is buildable). Engineering background:
[`track-d-native-app.md`](track-d-native-app.md) and
[`../phases/06-native-app.md`](../phases/06-native-app.md).

---

## 1. What you are designing

`pohunek-gui` is a **native desktop control plane** ("operator's cockpit") for
running AI coding agents across **multiple machines** at once. Think: a single
window where an operator sees every agent on every reachable host, spots the ones
**waiting for them**, opens them, starts new ones on a Linear issue or GitHub PR,
and manages projects/worktrees/prompts.

**Hard constraints (non-negotiable):**

- **No terminal in the app.** It never shows a console. "Open" a session = it
  launches the operator's external terminal. Design *around* the terminal, not
  *for* one.
- **Native desktop, Linux/Wayland first.** Built in Rust with **Iced** — design
  within its capabilities (§8).
- **Keyboard-first.** The primary user lives in a tiling window manager (sway).
  Everything important needs a shortcut; mouse is secondary.
- **Multi-host, partial-failure-normal.** One machine being down must never blank
  the screen; it's an everyday state, not an error page.

**The #1 job:** make a **blocked agent** (one waiting on the human) impossible to
miss, and one action away from being opened. Everything else is secondary.

---

## 2. Users & primary jobs

A single technical operator (today). Jobs, in priority order:

1. **Triage:** "which of my agents need me right now?" (blocked-first).
2. **Open:** jump into a session's terminal in one action.
3. **Launch:** start an agent on a Linear issue / GitHub PR with a preset prompt.
4. **Manage:** start/stop sessions, manage projects, worktrees, prompts.
5. **Review (v1.1):** read a diff, comment inline, send the review back as a new
   agent run.

---

## 3. The data & action surface (what you can put on screen)

These are the **real fields** the app has. Show what helps triage; hide the rest
behind detail/expandable views. Field names are the source of truth.

### 3.1 Hosts

A host = one reachable machine running the daemon (plus the local machine).

- **Discovery fields:** `name`, `fqdn`, `netbird_ip`, and a **class**: one of
  `ReachableDaemon{daemon_version}`, `VersionMismatch{daemon_protocol_version}`,
  `Unreachable`, `Candidate` (not probed).
- **Connection status (app-side, live):** Connecting → Connected → Disconnected
  (with error) → reconnecting. Plus `Unreachable`.
- **Capabilities (on inspect):** `daemon_version`, `protocol_version`,
  `supported_agents[]`, `runtimes[]` = `{agent, available, path}`,
  `git_available`, `worktree_supported`.
- **Actions:** auto-connect (no button), reconnect, inspect.
- **Design needs:** a compact per-host status marker (5 states above), a way to
  show "this host speaks a different protocol version" distinctly from "down".

### 3.2 Sessions (the core object)

One running (or finished) agent/shell. **Two orthogonal status axes — do not
conflate them:**

- **Lifecycle `state`:** `Starting`, `Running`, `Stopped`, `Done`, `Failed`.
- **Agent `activity`:** `Working`, `Blocked`, `Idle` (optional — may be absent
  until detected). **`Blocked` = waiting for the human. This is the hero state.**

A session can be `Running` + `Blocked` at the same time — design must express both
without clutter (e.g. lifecycle as a subtle dot, activity as the loud badge).

- **Identity/context fields:** `id`, `agent` (profile name), `agent_base`
  (`Shell` | `Codex` | `Claude` — needs an icon each), `project_label`/`project_id`,
  `branch`, `repo`, `worktree_path`, `is_linked_worktree`, `cwd`, `pid`,
  `created_at`, `updated_at`, `exit_code` (when finished).
- **Resumability:** `native_session_id`/`native_session_path` present ⇒ resumable.
- **Warnings:** `warnings[]` (non-fatal worktree setup issues) — surface subtly.
- **Metadata:** `metadata{}` free key/value, **including the `link.*` work-item
  link** (`link.provider`, `link.kind`, `link.id`, `link.url`, `link.branch`) —
  show the linked Linear/GitHub item as a chip.
- **Actions:** **Open in terminal** (primary), **Stop**, **Inspect**, **Edit
  metadata**, **New session** (form: agent + project + branch + base branch +
  preset prompt + metadata).
- **Live updates:** the list reacts in real time (created/updated/stopped, state
  changes). Design for things changing under the user's eyes.

### 3.3 Agents monitor (a view, not a new object)

Derived from sessions' `activity` across all hosts. The triage surface.

- Counts: Working N / **Blocked N (loud)** / Idle N.
- A flat, **blocked-first** list; each row → select session + "open in terminal".
- Per-host runtime availability (from capabilities) can flag "this host can't run
  agent X".

### 3.4 Projects

A git repo the daemon knows about.

- **Fields:** `id` (`p-…`), `label`, `repo_root`, `git_common_dir`, `origin_url`
  (optional), `default_base_branch` (optional), `source` (auto-registered vs
  explicit), `is_bare`, `added_at`, `last_used_at`.
- **On show:** the project + its **live worktrees**.
- **Project actions (launchable presets):** each project exposes named actions —
  `{name, provider (none|linear_issue|github_pr), template, layer}`. These are the
  one-click "launch an agent on this kind of work" entries.
- **Actions:** list, add (by path), show, rename, remove (optionally prune
  worktrees → needs confirm).

### 3.5 Worktrees

- Listed under a project (branch, path, linked-or-main, bound session).
- **Create = start a session on a branch** (there is no standalone "make a
  worktree" — design the affordance as part of New Session, not as its own verb).
- Inspect only otherwise.

### 3.6 Prompts

- Templates resolved **host-side** (per project, host/repo layers) with `${var}`
  placeholders. **v1 is read-only** — design *browse + live preview + launch*, not
  a template editor (authoring happens in the user's own editor).
- **Variables available** when launching on a work item:
  - Linear: `provider, id, number, title, body, branch, url`.
  - GitHub PR: `provider, number, id, title, body, branch, url`.
- **Actions:** browse resolved prompts/actions, **live render preview** against a
  selected item, launch a session with the rendered prompt.

### 3.7 Linear (in-app)

- **Browse:** the operator's issues — **assigned-to-me by default**, plus state
  filter and fulltext search.
- **Fields to show:** `identifier` (e.g. `ENG-123`), `title`, `state`, `branchName`,
  `assignee`, `url`, (optionally) priority/team/updatedAt. Body/`description` in
  detail.
- **Action:** **launch an agent on an issue** with a preset prompt → a session on
  `issue.branchName`, linked via `link.*`.

### 3.8 GitHub (via `gh`)

- **Browse:** PRs and issues.
- **Fields to show:** number, `title`, branch (`headRefName`), `state`, **checks
  status**, **review status** (approved / changes-requested / pending), author,
  draft, `url`. Body in detail.
- **Actions:** view/open a PR, **launch an agent on a PR** (preset prompt, linked),
  and surface **PR checks + review status next to the live agent badge** for
  linked sessions.

### 3.9 Diff review (v1.1 — design later, scope now)

- Diff for a session's worktree, a worktree, or a PR: **files → hunks → lines**
  (old/new side).
- **Inline comments** anchored to `file:line` (+ side). Comments collect into a
  **review**.
- **Dispatch** the review → it becomes the prompt for a **new session on the same
  branch**; optionally also post to the PR.

### 3.10 Settings

- `gui.toml`: `attach_command` (terminal launch template), `connect_timeout_ms`,
  `reconcile_secs`, `linear.token_key`, `gh_bin`; shared `terminal`.
- The Linear token is **never entered in the app** — show only "Linear token:
  configured / not configured" (the user sets it out-of-band in the OS keyring).

---

## 4. Component inventory (what to deliver designs for)

**Shell & navigation**
- 3-pane resizable layout; left tree (host → project → session), bottom-left
  agents monitor, right tabbed detail area; a status bar.
- Workspace **tree** with expand/collapse, badges, per-host connection markers.
- **Tabs** for the detail area (session detail, Linear, GitHub, prompt editor,
  diff (v1.1), settings).
- Optional **command palette** / global search (keyboard-first).

**Status & indicators (most important visual system)**
- **Agent activity badge:** Working / **Blocked** / Idle — distinct, colorblind-
  safe (color + shape/icon + text), with **Blocked** dominant.
- **Session lifecycle dot:** Starting / Running / Stopped / Done / Failed — quiet,
  secondary to the activity badge.
- **Host connection marker:** Connected / Connecting / Disconnected / Unreachable /
  Version-mismatch (5 distinct, compact).
- **PR checks/review badges:** pass / fail / pending; approved / changes-requested.
- **Agent-kind icons:** Claude / Codex / Shell. **Provider icons:** Linear / GitHub.
- **Work-item chip** (from `link.*`): provider icon + id + state.

**Tables & lists**
- Sortable/filterable/searchable lists: sessions (by host/project/state),
  Linear issues, GitHub PRs. Row hover/selected/changing states. (Keep columns few
  — see virtualization note §8.)

**Detail & forms**
- **Session detail:** the two-axis status, identity/branch/worktree/repo,
  timestamps, pid, exit code, warnings, work-item chip, **metadata key/value**
  (link.* highlighted), action buttons.
- **New session form:** agent picker (with availability from capabilities), project
  picker, branch, base branch, prompt-preset picker **+ rendered preview**, metadata.
- **Add/Rename project** forms.
- **Prompt editor:** template text + variable hints + live render preview.
- **Launch-on-item flow:** pick item → pick action/preset → preview prompt →
  launch (modal or wizard).

**Feedback**
- **Toasts/notifications:** launched, stopped, error, host reconnected — and a
  distinct, attention-grabbing **"agent X is now blocked"** alert. Blocked also
  fires an **OS desktop notification** (works when the window is backgrounded), so
  design both the in-app toast and the OS-notification content/wording.
- **Confirmation dialogs:** stop session, remove project (+ prune worktrees).
- **Empty states:** no hosts reachable, no sessions, no issues/PRs, token not
  configured.
- **Loading/skeleton states:** hosts connecting, fetching issues/PRs/diff.
- **Error states:** host unreachable, version mismatch, `gh` missing, Linear token
  missing, provider API error.

**Theming**
- Dark + light. Monospace for ids/branches/paths/diff; a clear type scale and
  spacing system; an icon set covering the above.

**Diff (v1.1)**
- File tree + unified diff view (side-by-side later) + inline comment threads +
  a review "tray" (collected comments) + dispatch action.

---

## 5. Key states & information hierarchy

Rank, loudest to quietest:

1. **A blocked agent exists** — unmissable anywhere in the app (badge, count,
   notification, sort-to-top). This is the product's reason to exist.
2. Live activity changes (working→blocked etc.) animating in place.
3. A host became unreachable — visible, **non-blocking**, isolated to that host.
4. Lifecycle/finish states (Done/Failed/exit code).
5. Everything else (metadata, timestamps, paths) — on demand.

Call out explicitly in the design: **lifecycle `state` vs agent `activity` are two
different axes.** A correct design lets the user read "is the process alive?" and
"does it need me?" independently at a glance.

---

## 6. Core flows to storyboard

1. **Cold start:** launch → hosts auto-connecting (some up, one down) → sessions
   populate → blocked agents surface. Show the partial-failure path.
2. **Triage → open:** spot a blocked agent → open it in the terminal (one action).
3. **Launch on a Linear issue:** browse issues → pick → choose preset → preview
   prompt → launch → new session appears linked.
4. **New blank session:** New Session form on a project + branch.
5. **Stop & confirm.**
6. **(v1.1) Diff review:** open diff → comment on lines → collect → dispatch as a
   new session.

---

## 7. Non-goals (don't design these)

- Any embedded terminal / console / log stream.
- macOS/Windows chrome (Linux first).
- Login/accounts/RBAC (single operator; trust is the network).
- Entering the Linear token in-app.
- A standalone "create worktree" verb (it's part of New Session).

---

## 8. Technical constraints (Iced) — design buildable things

**You can rely on:** custom-drawn widgets, a code-defined theme (so any visual
system is implementable), resizable split panes, scrollable lists, text inputs and
a multi-line text editor, tooltips, modal overlays/dialogs, real-time updates,
keyboard shortcuts, bundled custom fonts/icons.

**Be careful with (talk to engineering before relying on these):**
- **No mature native data-grid.** Tables are built from rows; **large lists need
  virtualization** — keep table layouts simple (few columns, clear row height),
  avoid complex per-cell widgets in long lists.
- **Rich text / markdown is limited.** Issue/PR bodies likely render as plain or
  lightly-formatted text, not full HTML/markdown. Design body views accordingly.
- **Heavy animation, complex drag-and-drop, and free-form canvas** are costly —
  prefer discrete state changes, simple transitions, and explicit controls.
- **Custom fonts/icons must be bundled** (no remote assets).

Favor: clear, dense, keyboard-navigable, high-signal layouts over visual flourish.
This is a tool used many times a day, not a landing page.

---

## 9. Open questions for the designer

- How to show the two status axes (lifecycle vs activity) together without
  confusion — the central visual problem.
- Tree vs grouped-list for the workspace when there are many hosts/sessions.
- Whether the agents monitor is a panel, an overlay, or the default home view.
- Density/theme defaults for an all-day tiling-WM tool.

UI state (layout, tabs, selection) is **persisted** across restarts — design the
restored-state experience, not just a cold default.

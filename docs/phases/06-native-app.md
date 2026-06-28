# Phase 6: Native Desktop Companion App (Track D)

Detailed implementation assignment for **Track D**. The *what/why* lives in
[`docs/design/track-d-native-app.md`](../design/track-d-native-app.md) (read it
first — decisions D-01…D-12 are binding). This doc is the *how*: milestones,
tasks, file/crate targets, and per-task acceptance criteria.

## Objective

Ship `pohunek-gui`, a **pure-native Rust (Iced) control plane** for everyday agent
work over **multiple hosts** at once: sessions, hosts, agents, projects,
worktrees, prompts, Linear, and GitHub PRs in one window. It talks **directly to
each host's daemon over NetBird via the Rust SDK** (`crates/client`) — no backend.
**It renders no terminal**; "attach" delegates to an external terminal.

## User value

- One window that shows **every reachable host's** sessions with **live
  agent-state badges**, the **blocked** ones surfaced first (they wait on you).
- Click → open that session in your terminal; start/stop sessions; manage
  projects and worktrees; browse and edit prompt presets.
- Browse your **Linear issues** and **GitHub PRs** and **launch an agent on one**
  with a preset prompt, branch, and worktree wired up — parity with the sway
  launcher scripts, sharing one link/prompt convention.

## Design principles (binding)

- **Control plane, not a terminal.** GUI uses only SDK `Client` + `Subscription`.
  Attach is `attach_command` spawn (D-01/D-03).
- **No new chassis surface.** Zero new daemon methods. Everything goes through the
  existing protocol. Link metadata rides `SessionNewParams.metadata` (D-08).
- **Pure core, thin view.** All state + transitions in `crates/gui-core`,
  headless-testable against loopback-TCP stand-in daemons. `crates/gui` (Iced) is
  view + input + spawn glue only (§3.1 of the design).
- **One source of truth with the scripts.** Shared prompt render (D-11), shared
  config dir (D-09), shared `link.*` schema (§7).
- **Secrets are references.** Linear token by keyring name only; `gh` owns its
  auth; no token in daemon/metadata/log/event (D-12).

---

## M0 — Scaffold + spike + shared prompt crate

Foundational; unblocks everything. No user-facing feature yet.

- **M0.1 — `crates/prompt` (shared render, D-11).** Extract provider-context field
  mapping + single-pass `${var}` substitution (today the Python in
  `scripts/lib.sh::pohunek_render_provider_prompt`) into a pure crate.
  - Provider context builders for `linear_issue` and `github_pr` matching the
    Python field picks exactly (identifier/id, title, description/body,
    branchName/headRefName, url).
  - Single-pass substitution + unknown-variable check, byte-for-byte equal to the
    Python output.
  - *Acceptance:* fixture golden tests (the existing script fixtures) pass against
    both the Python and the Rust output; a property test confirms a provider value
    containing a literal `${other}` is never re-expanded.
- **M0.2 — `pohunek prompt render` CLI subcommand.** Wraps `crates/prompt`; reads
  template + provider + item-id + context JSON (stdin), writes the rendered prompt.
  - *Acceptance:* `pohunek prompt render` output is byte-identical to the Python
    for every fixture; documented in `--help`.
- **M0.3 — Rewrite scripts onto the subcommand.** Replace the Python renderer in
  `scripts/lib.sh` with a call to `pohunek prompt render`; keep
  `pohunek-launch-issue`/`-pr` behavior bit-for-bit.
  - *Acceptance:* a script-level golden test (recorded `session new` argv for a
    fixture issue/PR) is unchanged before/after the rewrite; the Python is deleted.
- **M0.4 — `crates/gui-core` + `crates/gui` scaffold.** Workspace members;
  `gui-core` depends on `client` + `protocol` + `prompt`; `gui` is the Iced binary
  `pohunek-gui`. `#![forbid(unsafe_code)]`.
- **M0.5 — tokio↔Iced bridge + loopback-TCP test harness.** Background tokio
  runtime; one Iced `Subscription` per host wrapping an SDK `Client` + event
  `Subscription`; `Command::perform` plumbing. A test harness that boots ≥2
  in-process daemons on loopback TCP (the SDK/data-layer CI rig).
  - *Acceptance:* a headless `gui-core` test connects to 2 loopback daemons, runs
    `daemon.health` + `session.list`, and receives one injected `agent_state`
    event as a `Message`.
- **M0.6 — Spike (1–2 days).** Minimal Iced window: connect localhost, render
  `session.list`, reflect one live `agent_state`, spawn `attach_command`.
  - *Acceptance:* the spike runs against a real local daemon; documents any Iced
    widget gaps for tables/trees before M1.
  - *M0 spike note:* Iced 0.14 covers the basic shell with rows, columns,
    scrollables, buttons, and subscriptions, but it does not provide a mature
    first-party table/tree widget. M1 should model the host/project/session tree
    in `gui-core` and render it with composed rows first; richer table/tree
    behavior or large diff views will likely need a small custom widget or a
    carefully-vetted companion crate.

---

## M1 — D.1 Workspace shell + multi-host connect *(first user-facing)*

- **M1.1 — Discovery + auto-connect-all (D-07).** On launch: `host.discover` +
  localhost; connect every reachable host concurrently with `connect_timeout_ms`;
  per-host `ConnState` (Connecting/Connected/Disconnected/Unreachable).
- **M1.2 — Snapshot seed + event reconciliation.** Per connected host:
  `session.list` + `project.list` once, then patch from events
  (`session_created/updated/stopped`, `agent_state`). Periodic re-list
  (`reconcile_secs`) + re-list on reconnect as the safety net (§3.4 of design).
- **M1.3 — Reconnect/backoff.** Exponential backoff 1s→30s cap; never block other
  hosts; surface `last_error` as a per-host marker.
- **M1.4 — Workspace tree view.** host → project → session, with connection
  markers and live `agent_state` badges.
- **M1.5 — Agents monitor (D-05).** Counts + flat list sorted blocked-first; click
  selects the session and offers "open in terminal".
- **M1.6 — Blocked notifications (D-15).** On a transition into `Blocked`, raise an
  OS notification (Wayland/libnotify) + an in-app toast.
- **M1.7 — UI state persistence (D-16).** Persist/restore pane sizes, open tabs,
  expanded nodes, window size, and selection in `~/.local/state/pohunek-gui/`.
- *Acceptance (maps to roadmap "Done when"):* against ≥2 loopback daemons the app
  lists both hosts' sessions, an injected state change is reflected live, a
  killed/unreachable host shows an error marker **without** affecting the other
  host's view, a transition into `Blocked` fires a notification, and UI state
  survives a restart. Headless `gui-core` tests cover seed+patch+reconnect.

---

## M2 — D.3 Session + project + worktree management

- **M2.1 — Session lifecycle.** `session.new` (agent/project/branch/base_branch/
  input/metadata), `session.stop`, `session.inspect`. Optimistic UI reconciled by
  events.
- **M2.2 — Session metadata view/edit.** Render `SessionInfo.metadata`; edit via
  `session.set_metadata` (merge/clear semantics per `SessionSetMetadataParams`).
- **M2.3 — Project management.** `project.list/add/show/rename/remove`; show live
  worktrees from `project.show`.
- **M2.4 — Worktree.** Inspect via `project.show`; **create = start a session on a
  branch** (`session.new --branch`); there is no standalone worktree method (this
  is a protocol fact, not a gap — see design §6 D.3).
- **M2.5 — Attach action (D.2 redefined).** "Open in terminal" fills
  `{bin}`/`{host}`/`{id}` into `attach_command` and spawns it; fire-and-forget;
  closing the terminal leaves the session running.
- *Acceptance:* create/stop/inspect a session on a loopback daemon end-to-end;
  metadata round-trips; spawning the attach command is asserted (mock spawner in
  tests records the resolved argv).

---

## M3 — D.4 Prompt management *(read-only, host-side; D-13)*

- **M3.1 — Resolve + browse.** List/resolve a project's prompts and actions from
  the **target host** via `project.prompt`/`project.action` (host/repo layers
  honored by the daemon). Read-only; **no in-GUI editing in v1** (open item A.1).
- **M3.2 — Render + preview + launch.** Render the host-resolved template via
  `crates/prompt` (M0.1), preview, and launch through `session.new input=`.
- *Acceptance:* a GUI-rendered preset for a fixture context is byte-identical to
  `pohunek prompt render`; launching produces exactly one session with the
  rendered prompt; prompts resolve correctly against a *remote* loopback host
  (not local files).

---

## M4 — D.5 Provider integration (Linear + GitHub) *(completes v1)*

- **M4.1 — Linear GraphQL client (D-11/D-12/D-14).** Personal API key read by
  `token_key` from the keyring at call time; default view **assigned-to-me** +
  state filter + fulltext search. No token input field; never log the value.
- **M4.2 — GitHub via `gh`.** List PRs/issues; fetch PR checks + review status;
  open/view a PR. Shell-out only.
- **M4.3 — Launch on an item (parity).** Resolve action → fetch item → render
  prompt (shared crate) → `session.new --branch <item.branch> --input <prompt>`
  with `link.*` metadata. Byte-identical to the `pohunek-launch-issue`/`-pr` path.
- **M4.4 — `link.*` schema (design §7).** Write
  `link.provider/kind/id/url/branch` atomically at `session.new`.
- **M4.5 — PR status next to the badge.** Surface checks/review state beside the
  live `agent_state` for linked sessions.
- *Acceptance:* launching on a fixture issue starts exactly one session on the
  expected branch with the rendered prompt; the `link.*` metadata **persists
  across daemon restart** and is **byte-identical** to a sway-script-launched
  link; a secret-leak test confirms the Linear token's value appears in **no**
  `session.new` metadata and **no** event.

---

## v1 release gate

All of M0–M4 acceptance criteria green; the roadmap Track D "Done when" subset for
v1 holds against ≥2 loopback daemons (list, live state, attach-command spawn,
launch-on-issue, link parity + persistence, no token leak); manual smoke on a real
2-host NetBird mesh.

---

## v1.1 — D.6 Diff review + comment-to-session loop

- **M5.1 — Diff source + parser.** worktree-vs-base for a session/worktree;
  `gh pr diff` for a PR. Parse to file/hunk/line model (record old/new side).
- **M5.2 — Diff view + inline comments.** Unified view first (side-by-side later);
  comments anchored to `file:line(+side)`.
- **M5.3 — App-local review store (D-08).** Comments + review in
  `~/.local/share/pohunek-gui` (SQLite or JSON). No daemon surface.
- **M5.4 — Dispatch loop.** Render review → preset prompt → `session.new --input`
  on the **same branch/worktree**; write `review.source`/`review.dispatched_at`
  into the new session's metadata. Optional `gh pr review`/`gh pr comment`.
- *Acceptance:* given a session/worktree/PR with changes the app renders the diff,
  accepts inline comments, and dispatching starts **exactly one** new session on
  the **same** branch with the comments delivered as the prompt and the
  review→session link persisted.

---

## Out of scope (this phase)

- In-app terminal rendering (D-01); macOS/Windows (D-04); app-level auth/RBAC; any
  new daemon method or network surface; cross-host project unification.

## Open risks

See design §11. Chief among them: Iced widget maturity for tables/trees/diff
(verify at the M0 spike) and link-parity drift (guarded by the shared render +
golden tests).

# Track D — Native Desktop Companion App (detailed design)

Status: **design** (pre-spike). Supersedes the terminal-renderer direction in
`docs/ROADMAP.md` §3 Track D — see [Roadmap corrections](#10-roadmap-corrections).

Last updated: 2026-06-27.

This document is the source of truth for *what* Track D builds and *why*. The
roadmap (`docs/ROADMAP.md`) remains the sequencing index; where the two disagree,
this doc wins for Track D and the roadmap is corrected to match.

---

## 1. One-paragraph summary

`pohunek-gui` is a **pure-native Rust control plane** for everyday agent work: an
operator's cockpit over **sessions, hosts, agents, projects/worktrees, prompts,
Linear, and GitHub** in one window. It is a client of the chassis over the **Rust
SDK (`crates/client`, Track S)**, talking **directly to each host's daemon over
NetBird** — no aggregator backend, no central server. **It does not render a
terminal.** "Attach" delegates to the operator's terminal emulator via a
configurable command template. The chassis stays provider-agnostic and gains **no
new network surface**.

---

## 2. Locked decisions (with rationale)

| # | Decision | Rationale |
|---|---|---|
| D-01 | **No embedded terminal.** Attach opens the session in an external terminal. | Removes the single largest engineering risk (VT grid renderer: glyph atlas, scrollback, selection, mouse, resize roundtrip). The CLI `pohunek attach` already streams the remote PTY over NetBird; the GUI just spawns it. |
| D-02 | **GUI = Iced** (Elm: `Message`/`update`/`view`/`subscription`). | Pure control plane (lists, trees, tables, forms, later a diff view). Iced subscriptions map cleanly onto the SDK's per-host event streams and async provider calls; retained model means fewer needless repaints. egui's only real edge (immediate-mode custom painting) was for the terminal grid, which is now gone. |
| D-03 | **Attach = configurable `attach_command` template.** GUI stays dumb: fills `{bin}` `{host}` `{id}` and spawns. | Works with sway/tmux/zellij/any terminal. Dedup/focus is the user's concern (their WM already does it — see `pohunek-rofi`). No OS-specific window logic in the GUI. |
| D-04 | **Linux only (Wayland/sway) for v1.** | Operator's environment. XDG dirs, `$TERMINAL`, secret-service keyring, no per-OS spawn/keychain abstraction. macOS/Windows addable later (Iced is cross-platform). |
| D-05 | **Agents view = live `agent_state` monitoring.** working/blocked/idle; **blocked floats to top** (it is waiting on the operator); click → attach. | The high-value question is "what are my agents doing right now and which one needs me". Agent-profile *configuration* is a smaller, later concern. |
| D-06 | **v1 = D.1 + D.3 + D.4 + D.5.** D.6 (diff review) = **v1.1** fast-follow. | Covers "super ovládání sessions/hosts/agents/Linear/GitHub". D.6 carries three unsolved sub-problems (diff parse, comment anchoring, storage) and should not gate v1. |
| D-07 | **Hosts: auto-discover + auto-connect all.** `host.discover` + localhost on launch, connect all reachable concurrently (short timeout), per-host error markers. | Zero-config; matches roadmap D.1 "enumerate reachable hosts". |
| D-08 | **D.6 review storage = app-local until dispatch.** Comments live in `~/.local/share/pohunek-gui` (SQLite/JSON). On dispatch, the `review→session` link is written into the **new session's** metadata. | Honors "chassis gains no new network surface". Trade-off: a PR-only review is not cross-surface until it is dispatched to a session. |
| D-09 | **Config: shared `~/.config/pohunek/`.** Reuse `launcher.conf` keys (`terminal`, `host`, …) + `prompts/*.tmpl`; GUI-specific keys in `gui.toml` alongside. | One source of truth with the sway scripts; no drift. |
| D-10 | **Build order: D.1 first** (workspace + multi-host + badges), then D.3 → D.4 → D.5. | The terminal risk is gone, so the value-first order (see the cockpit early) is now safe. |
| D-11 | **Linear = in-app GraphQL** (token from keyring); the **field-mapping + prompt render is a shared Rust implementation** (`crates/prompt` + `pohunek prompt render`), used by both the GUI and the (rewritten) sway scripts. | Browsing needs query power the `linear_cli` one-shot `issue view` can't give. Parity is guaranteed at the prompt/link layer by *sharing the render*, not the fetch — so the Python renderer in `lib.sh` is replaced by the shared path. |
| D-12 | **Linear token: out-of-band keyring only.** The user stores it (`secret-tool`/keyring); `gui.toml` holds only `token_key`. The GUI has **no token input field** and reads the value by name at call time. | Aligns with the "references, never values" secret rule — the raw token never passes through the GUI, so it can't leak via a widget, log, or clipboard. |
| D-13 | **Prompts/actions resolve host-side; the GUI is read-only on them.** Launching on any host calls `project.action`/`project.prompt` on **that host's** daemon (returns recipe + template content), renders via `crates/prompt`, and launches. **No in-GUI template editing in v1** (authoring stays in the user's editor; remote hosts have no prompt-write method and adding one is a forbidden new surface). | The daemon is the source of truth for which agent/template a project uses (host/repo layers). A GUI running on the operator's box could only edit the *local* host's files — asymmetric and low-value. *(Open: whether to add local-only editing later — see §6 D.4.)* |
| D-14 | **Linear = personal API key; default view "assigned to me" + state filter + fulltext search.** | Single operator; no OAuth flow needed. |
| D-15 | **Notify on blocked via OS notifications (Wayland/libnotify) + in-app toast.** | The #1 job is not missing a blocked agent even when the window is backgrounded; an in-app-only signal fails that when the GUI isn't focused. |
| D-16 | **Persist UI state** (pane sizes, open tabs, expanded nodes, window size, selection) to `~/.local/state/pohunek-gui/`. | All-day tool; restoring layout is expected. |

---

## 3. Architecture

### 3.1 Crate layout

```
crates/
  protocol/    (existing) typed envelopes — shared, unchanged
  client/      (existing, Track S) Rust SDK: Client, Subscription, transports
  daemon/      (existing) owns PTYs/stores — UNCHANGED by Track D
  cli/         (existing) owns `pohunek attach` (the terminal) — UNCHANGED
  gui-core/    (NEW) pure state + update logic. No Iced, no I/O surface beyond
               the SDK + a small command port. Headless-testable.
  gui/         (NEW) Iced binary `pohunek-gui`. View + input + spawn glue only.
```

The hard rule: **all state and transitions live in `gui-core` and are testable
without a display.** `gui/` is a thin shell that renders `gui-core` state and
turns user input into `gui-core` intents. This is what lets the roadmap "Done
when" criteria run in CI against loopback-TCP stand-in daemons (§9).

### 3.2 SDK surface used (and not used)

The GUI uses **only** `Client` (request/response) and `Subscription` (events)
from `crates/client`. It **does not** use the attach duplex byte stream
(`attach_raw*`) — the terminal is delegated to the CLI (D-01). This keeps the GUI
off the hardest part of the SDK and means the attach stream stays exercised by
the CLI alone.

Transports: `connect_raw_local*` (localhost Unix socket) and
`connect_raw_tcp_addr*` (remote host over NetBird).

### 3.3 Async model (Iced + tokio)

- A background **tokio** runtime hosts every SDK connection.
- **One Iced `Subscription` per connected host.** The subscription owns that
  host's `Client` + event `Subscription`, and emits `Message::Host(host_id, ev)`
  for every protocol event (`agent_state`, `session_created/updated/stopped`,
  `attach_opened/closed`) plus connection-lifecycle messages
  (`Connecting`/`Connected`/`Disconnected{error}`).
- **Requests are `Command::perform(async, Message)`**: `session.new`,
  `session.stop`, `project.*`, `host.inspect`, etc. Their results come back as
  `Message`s and patch `gui-core` state.
- **Provider calls** (Linear GraphQL, `gh`) are also `Command::perform` — they
  never touch the daemon connections.

This gives a single-threaded, deterministic `update` loop fed by N independent
host streams + async command results.

### 3.4 Workspace state model (`gui-core`)

```
Workspace
  hosts: BTreeMap<HostId, HostView>
  selection: Selection            // focused host/session/tab
  providers: ProviderState        // Linear/GitHub caches (D.5)

HostView
  conn: ConnState                 // Connecting | Connected | Disconnected(err) | Unreachable
  meta: Option<HostCapabilities>  // from host.inspect
  sessions: BTreeMap<SessionId, SessionInfo>   // snapshot, patched by events
  projects: BTreeMap<ProjectId, ProjectInfo>
  last_error: Option<String>      // surfaced as a per-host marker, never fatal
  backoff: Backoff
```

**Reconciliation.** On connect: `session.list` + `project.list` once to seed the
snapshot, then **event-driven** patching is the steady state. A periodic re-list
(default 30 s, config `gui.reconcile_secs`) and a re-list on every reconnect act
as a safety net against missed events. `agent_state` events update the badge in
place.

**Connection lifecycle.** Connect with a short timeout (default 2 s, config
`gui.connect_timeout_ms`). On drop, reconnect with exponential backoff
(1 s → 30 s cap). A host that never answers is marked `Unreachable` in the tree;
**one host's failure never blocks the others** (partial results, per-host
markers).

### 3.5 Secrets

- **Linear token** lives in the **OS keyring** (secret-service via the `keyring`
  crate), referenced by name from `gui.toml` (`linear.token_key`). Loaded into
  process memory only to call the Linear GraphQL API. **Never** written to a
  daemon request, session metadata, the event log, app logs, or any file.
- **GitHub** uses `gh`'s own auth (shell-out). The GUI never reads or stores a
  GitHub token.
- Invariant (carried from the roadmap): **no provider token appears in any daemon
  log, metadata, or event.** This is asserted in tests by scanning emitted
  `SessionNewParams.metadata` and the event stream for the token key.

---

## 4. Config (`~/.config/pohunek/`)

Shared with the sway scripts (D-09). New GUI file `gui.toml`:

```toml
# Attach delegation (D-03). {bin} {host} {id} are substituted; {host} is empty
# for the local daemon. The GUI spawns this verbatim and does not track the child.
attach_command = "$TERMINAL -e sh -c 'printf \"\\033]0;pohunek:%s\\007\" \"{id}\"; exec {bin} attach --host {host} {id}'"

[gui]
connect_timeout_ms = 2000
reconcile_secs     = 30

[linear]
token_key = "pohunek-linear"   # keyring entry name, NOT the token

[providers]
gh_bin = "gh"
```

Reused from `launcher.conf`: `terminal`, `pohunek_bin`, `host` (seed list),
`linear_cli` (if we choose to reuse the CLI rather than raw GraphQL — see §6 open
items). Prompt templates are the shared `~/.config/pohunek/prompts/*.tmpl`.

---

## 5. UX / layout (IDE 3-pane)

```
+-----------------+--------------------------------------------+
| WORKSPACE TREE  |  DETAIL (tabs)                             |
|  host: laptop ● |  [ session: feat-x ] [ Linear ] [ PRs ]    |
|   proj: api     |                                            |
|    ▸ feat-x  ◀ working   inspect / metadata / actions        |
|    ▸ bug-y   blocked(!)   project / worktree info            |
|   proj: web     |   "open in terminal"  "stop"  "new…"       |
|  host: dev ✗err |                                            |
| --------------- |                                            |
| AGENTS MONITOR  |                                            |
|  blocked(!) 1   |                                            |
|  working   3    |                                            |
|  idle      5    |                                            |
+-----------------+--------------------------------------------+
```

- **Left, top:** host → project → session tree with live `agent_state` badges and
  per-host connection markers (`●` connected, `…` connecting, `✗` error).
- **Left, bottom:** **Agents monitor** (D-05) — counts + a flat list sorted
  blocked-first; click jumps to the session and offers "open in terminal".
- **Right:** detail tabs — session inspect/metadata/actions, project/worktree
  info, and (D.5) Linear and GitHub browsers. Diff review (D.6) becomes a tab in
  v1.1.

---

## 6. Slices

### D.1 — Workspace shell + multi-host connect (v1, first)

- Launch → `host.discover` + localhost; connect-all concurrently (D-07).
- Per host: seed via `session.list` + `project.list`, then live patch from the
  event subscription. Per-host error markers; never block on a slow host.
- Live `agent_state` badges; the Agents monitor (D-05) is built here.
- **Done when:** ≥2 hosts listed, a state change is reflected live, a dead host
  shows an error marker without affecting the others.

### D.3 — Session + project + worktree management (v1)

- Session lifecycle: `session.new` (agent/project/branch/base_branch/input),
  `session.stop`, `session.inspect`; metadata view via `SessionInfo.metadata`,
  edit via `session.set_metadata`.
- Project: `project.list/add/show/rename/remove`; worktree create/inspect via the
  existing project + session worktree path.
- All driven through the SDK; no new protocol methods.

### D.4 — Prompt management (v1)

- **Resolve + preview + launch (read-only, D-13).** Resolve a project's prompts
  and actions from the **target host** via `project.prompt`/`project.action`
  (host-side, honoring host/repo layers), render `${var}` via `crates/prompt`
  (single-pass, unknown-var check — byte-identical to the scripts), preview, and
  launch via `session.new input=`. This works uniformly for the local and remote
  hosts.
- **No in-GUI template editing in v1** (D-13). The daemon owns the templates;
  authoring stays in the user's editor on each host.
- *Open item:* optionally add **local-only** editing of `~/.config/pohunek/prompts/`
  (local daemon only; remote stays read-only) — deferred pending decision A.1.

### D.5 — Provider integration: Linear + GitHub (v1)

- **Linear:** in-app GraphQL with the keyring token (D-11/D-12). Browse the
  operator's issues; **launch an agent on an issue** with a preset prompt, parity
  with `pohunek-launch-issue` (resolve action → fetch issue → render prompt →
  `session.new --branch <issue.branchName> --input <prompt>`).
- **GitHub:** via `gh`. Browse PRs/issues; launch parity with `pohunek-launch-pr`;
  surface PR checks/review status next to the live agent badge; open/view a PR.
- **Session ↔ work-item link** (the cross-surface link): the flat `link.*` keys of
  §7, written atomically through `SessionNewParams.metadata`.
- **Shared render (D-11):** the provider-context field mapping and `${var}`
  substitution move into `crates/prompt` (pure) and a `pohunek prompt render`
  subcommand. The GUI calls the crate fn; the rewritten scripts call the
  subcommand. The Python `pohunek_render_provider_prompt` in `lib.sh` is retired.
  This is what makes a GUI-launched prompt **byte-identical** to a script-launched
  one. No daemon method is added (client-side only).

### Attach action (v1, used by D.1/D.3/D.5)

Spawn `attach_command` with `{bin}`/`{host}`/`{id}` filled (D-03). Fire-and-forget;
the GUI does not own the child. Detaching/closing the terminal leaves the session
running on its host (daemon owns the PTY).

### D.6 — Diff review + comment-to-session loop (v1.1)

- Diff source: worktree-vs-base for a session/worktree; `gh pr diff` for a PR.
- Render by file/hunk (unified first; side-by-side later), inline comments
  anchored to `file:line` (record the side — old/new — for stability).
- Comments + review live **app-local until dispatch** (D-08). Dispatch renders the
  review into a preset prompt and launches `session.new --input` on the **same
  branch/worktree**; the `review→session` link is written into the new session's
  metadata. Optionally also post via `gh pr review` / `gh pr comment`.

---

## 7. Work-item link metadata schema (shipped)

Both the GUI and the scripts write this schema, stored in `SessionInfo.metadata`
(opaque to the chassis):

```
link.provider   = "linear" | "github"
link.kind       = "issue" | "pull_request"
link.id         = "<identifier or number>"   # e.g. "ENG-123" / "456"
link.url        = "<https://…>"              # optional
link.branch     = "<branch the work item maps to>"
```

(D.6 adds, on the dispatched session: `review.source` = `app-local review id`,
`review.dispatched_at` = RFC3339.)

The schema lives once, in `pohunek_prompt::link` (`crates/prompt/src/link.rs`);
`crates/gui-core` re-exports it rather than keeping its own copy. A link made in
the GUI and one made by a script are **byte-identical** given the same work
item, guaranteed by construction: both write exactly these keys via the same
atomic `session.new metadata` path (the CLI's repeatable `session new --meta
key=value` flag), and both derive `link.branch` through the shared
`branch_from_provider_json`. The **former required follow-up task** — the
scripts wrote nothing at launch — is done: `pohunek-launch-issue` / `-pr` build
the link with the client-side `pohunek prompt link` subcommand (provider JSON
on stdin, same convention as `pohunek prompt render`) and pass it into
`session new` as repeated `--meta` pairs.

---

## 8. Roadmap corrections

The roadmap's Track D section assumes an in-app terminal; correct it to match
this design:

1. **D-01 supersedes the VT renderer.** Drop `alacritty_terminal` /
   `libghostty-vt` and the egui/Iced terminal-widget framing. D.2 ("In-app
   terminal attach") is **redefined** to the attach-command delegation (D-03) and
   folded into the attach action — it is no longer a slice.
2. **GUI framework is decided: Iced** (not "egui or Iced, pick via spike").
3. The "Done when" clause "**attaches and round-trips terminal I/O**" changes to
   "**spawns the configured `attach_command` for the selected session**" — the
   GUI is not on the terminal I/O path.
4. The claim "the same store the sway scripts write" is now true; §7 is a
   shipped, shared schema and the scripts write it via `pohunek prompt link`.

---

## 9. Testing strategy

- **`gui-core` is headless-testable.** All transitions (connect, list seed, event
  patch, reconnect/backoff, link write, prompt render) are pure functions over
  state + incoming `Message`, tested without Iced.
- **≥2 loopback-TCP stand-in daemons** in CI (the roadmap "Done when"): the data
  layer lists both hosts' sessions, reflects a state change, writes a link that
  **persists across daemon restart** and is byte-identical to the scripts' link,
  and (v1.1) dispatches a review that starts exactly one session on the same
  branch with the review as the prompt.
- **Secret-leak assertion:** scan emitted `session.new` metadata + the event
  stream; fail if the Linear token key's value appears.
- **Prompt-render parity:** golden test that GUI rendering of a `.tmpl` equals
  `pohunek_render_provider_prompt` for the same context.

---

## 10. Sequencing

1. **Spike (1–2 days):** Iced app skeleton + tokio bridge; connect to localhost
   daemon; render `session.list`; one `agent_state` event live; spawn
   `attach_command`. Validates the async model end-to-end.
2. **D.1** workspace + multi-host + agents monitor.
3. **D.3** lifecycle + project/worktree.
4. **D.4** prompts.
5. **D.5** Linear + GitHub + link schema (§7) + scripts follow-up.
6. **v1 release.**
7. **D.6** diff review + comment-to-session loop (v1.1).

---

## 11. Open risks

- **Iced ecosystem maturity** for tables/trees/diff — verify widget availability
  at the spike; budget for a custom diff widget in D.6.
- **NetBird connect latency** vs the 2 s timeout — tune `connect_timeout_ms` on
  real mesh; ensure the UI stays responsive while hosts are still connecting.
- **Link parity drift** — the scripts and GUI must not diverge on §7 keys; the
  shared render (D-11) + the golden/byte-identical test are the guard.
- **Script migration off Python** (D-11) — rewriting `lib.sh` rendering to
  `pohunek prompt render` must keep existing launch behavior bit-for-bit; cover
  with a fixture-based golden test before deleting the Python.

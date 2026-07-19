# Pohunek — Roadmap

Consolidated, top-level roadmap for the whole project. This is the **index and
sequencing** source of truth; the per-phase design docs under
[`docs/phases/`](phases/) and [`docs/design/`](design/) remain the source of truth
for *what* and *why* inside each track.

Status reflects the **code on `main`**, verified against the tree (releases
v0.1.0 -> v0.4.3). Where a phase/plan doc's own status header lags the code, the
code wins and the lag is noted.

Last reconciled: 2026-06-27.

---

## 1. Completed foundations (shipped)

| Track | What it delivers | Where |
|---|---|---|
| **Phase 1 — Core local sessions** | Daemon owns PTYs/processes; CLI new/list/inspect/attach/detach/stop; state engine; resume via native session IDs; worktree-per-session; file-based stores. | [`phases/01`](phases/01-core-local-sessions.md), `crates/daemon`, `crates/cli` |
| **Milestone 5 — State engine** | Debounced `agent_state` signal (`working`/`blocked`/`idle`) with recorded source, in `session.inspect` + event stream. | [`...milestone-5.md`](superpowers/plans/2026-06-17-state-engine-milestone-5.md) |
| **Phase 2 — Remote over NetBird** | Tokenless discovery; `host list/discover/inspect`; full remote session lifecycle over the mesh; daemon binds NetBird-only. | [`phases/02`](phases/02-remote-netbird.md) |
| **Milestone 10** | `--json` everywhere + `cli_usage` typed error envelope. | branch `milestone-10` (merged) |
| **Projects** | Git-repo awareness; `project` commands; all 7 follow-ups (F1–F7) resolved. | [`design/projects*.md`](design/) |
| **Per-project actions + worktree hooks + agent profiles** | All slices incl. session/agent-state hooks (B2, `ed6b778`); env-clear + allowlist; host agent profiles. | [`design/per-project-actions-*`](design/) |
| **Phase 5 / Milestone 12 — rofi/sway launcher** | `session list --filter` (chassis); `session new --input` (atomic launch); `scripts/` (launch-issue/pr, rofi); optional banner rendered by `pohunek attach`; `pohunek setup` for sway/rofi. | [`phases/05`](phases/05-rofi-sway-launcher.md), `scripts/` |

> This roadmap supersedes older milestone notes. Use the recommended sequence
> below for next work.

---

## 2. Completed — Universal Assistant

Plan: [`design/universal-assistant-plan.md`](design/universal-assistant-plan.md).
**P0–P11 are implemented** in this tree.

Final finish-only items are closed:

- **P8 — hook write hard gate.** The assistant prompt and embedded knowledge
  bundle now require explicit per-file hook confirmation independent of `--yes`
  and define quarantine paths for non-interactive contexts.
- **P10 — behavior eval.** `cargo xtask eval` writes a concrete manual release
  gate package and validates captured transcripts for parser-valid `pohunek`
  commands plus required outcome terms.
- **P11 — human docs outputs.** `cargo xtask docs site` renders site/offline
  docs, and the release workflow packages `docs/offline/` plus the docs
  manifest into each tarball.

This track is closed. The forward path below starts with the public API / SDK
work.

---

## 3. Forward tracks

The big remaining direction is **client surfaces** on top of the existing chassis.
The chassis (daemon + control protocol) stays **provider-agnostic and
presentation-agnostic** and gains no new network surface. Three separate tracks,
in dependency order: **SDKs → native desktop app → (later) browser control
center**.

### Track S — Public API + SDKs *(foundational, split out)*

Promote the control protocol from an internal CLI wire format to a **documented,
versioned public API**, consumed only through SDKs (never hand-rolled wire code).
This is the shared base for every client below; it is broken out of the old
Phase 4 so the desktop app and the browser app build on the same contract.

- **S.1 — Rust SDK (`crates/client`) — complete.** Extract the
  transport-agnostic client that lived `pub(crate)` in `crates/cli/src/client.rs`:
  connect (Unix socket / NetBird TCP), request/response, event subscription, and
  the **attach duplex byte stream**, with its own error type. The CLI is now a
  consumer with no behavior change.
- **S.2 — Public API doc + version negotiation — complete.** The versioned
  public API is documented in [`docs/public-api.md`](public-api.md): methods,
  envelopes, error classes/codes, events, version negotiation, and the attach
  stream as public protocol surface.
- **S.3 — TS SDK (`web/sdk`) — complete.** `ts-rs`-generated types
  (`web/shared`) + a runtime client with pluggable transports (TCP for Node/Bun
  → daemon direct; WebSocket for browser → backend). CI **drift check** fails if
  generated TS types diverge from the Rust source. Track B inherits
  `web/backend` (`@pohunek/relay`) as its tested WebSocket transport core
  instead of starting from a spec.

**Stability:** no compatibility promise pre-1.0; SDK semver tracks the protocol
version; breaking changes allowed with a version bump until the promise is made.

*Done when:* minimal clients on the Rust SDK and TypeScript SDK do `daemon.health`
/ `session.list`, subscribe to an event, and round-trip an attach stream against
a daemon; the TypeScript SDK also verifies the same flow through the WebSocket
relay transport.

### Track D — Native Desktop Companion App *(primary GUI)*

> **Detailed design:** [`design/track-d-native-app.md`](design/track-d-native-app.md)
> is the source of truth for Track D and supersedes this summary where they differ.
> Key pivots since the original framing: **no embedded terminal** (attach is
> delegated to an external terminal), **GUI = Iced** (decided), **Linux only** v1,
> **v1 = D.1+D.3+D.4+D.5** (D.6 is v1.1).

A **pure-native Rust desktop app** (no webview, no JS) and the **primary GUI**
going forward. It is a **control plane** — it does *not* render a terminal. It is
a client of the chassis over the **Rust SDK (Track S)**, talking **directly to
each host's daemon over NetBird** — **no aggregator backend**, no central server.
It is the operator's cockpit for everyday agent work: sessions, hosts, agents,
projects, worktrees, prompts, Linear, and PRs in one window.

This supersedes the dropped libghostty native-GUI direction (Phase 3) and the
Phase-4 "Tauri later, optional" note: the desktop client is promoted to primary
and built pure-native.

**Tech (decided):** pure-native Rust GUI on **Iced** (Elm-style). **No terminal
renderer** — "attach" spawns the operator's terminal via a configurable
`attach_command` template running `pohunek attach` (the CLI already streams the
remote PTY over NetBird). **PTY ownership stays in the daemon**; the GUI uses only
the SDK `Client` + event `Subscription` (not the attach duplex stream). No TS/JS
layer.

**Provider integration lives in the app** (it is a real native process, so it can
shell out and call APIs directly — no backend needed): **Linear via in-app GraphQL
(keyring token), GitHub via `gh`**. It reuses the **same conventions** the sway
launcher scripts already use, so the two surfaces share one source of truth:
- prompt templates in `~/.config/pohunek/prompts/*.tmpl` (`${var}` substitution),
  rendered by a **shared implementation** (`crates/prompt` + `pohunek prompt
  render`) the GUI and the rewritten scripts both call;
- atomic launch via `session new --input`;
- work-item / PR links stored as **opaque `link.*` metadata** on the session in
  the daemon store (the chassis never interprets them) — the same keys the sway
  scripts write via the shared `pohunek_prompt::link` implementation and the
  client-side `pohunek prompt link` subcommand, so a link made in one surface
  shows in the other, byte-identical.

Provider credentials live **only** in the app (gh's own auth; a Linear token read
by name from the OS keyring) — never in daemon state, session metadata, or the
event log.

Suggested slices (each independently valuable):

- **D.1 — Workspace shell + multi-host connect.** Auto-discover (`host discover`)
  + localhost and auto-connect all reachable hosts concurrently (short timeout,
  partial results with per-host error markers), unified workspace with **live
  agent-state badges** off the event subscription (`agent_state`, `session_*`),
  plus an **agents monitor** (blocked-first).
- **D.2 — Attach delegation.** *(Redefined — no longer an in-app terminal.)* The
  "open in terminal" action spawns the configured `attach_command` for the
  selected session. The GUI is not on the terminal I/O path; closing the terminal
  leaves the session running on its host.
- **D.3 — Session + project + worktree management.** Full lifecycle (new / stop /
  inspect), project list/add/show/rename/forget, and worktree inspect (creation is
  a side effect of `session new --branch` — no standalone worktree method exists) —
  driven through the existing protocol.
- **D.4 — Prompt management.** Browse/edit the shared `~/.config/pohunek/prompts/`
  templates; launch a session with a rendered preset prompt (`session new --input`).
- **D.5 — Provider integration (Linear + GitHub).** Browse the operator's Linear
  issues and GitHub PRs/issues; **launch an agent on an item** with a preset prompt
  (parity with `pohunek-launch-issue` / `-pr`); link sessions; surface PR
  checks/review status next to the live state badge; open/view a PR.
- **D.6 — Diff review + comment-to-session loop (Kandev-style).** A unified diff
  surface for a **session's worktree, a worktree, or a PR**: render the diff
  (worktree diff vs the base branch for a session/worktree; `gh pr diff` for a PR),
  browse it by file/hunk, and add **inline comments anchored to `file:line`**.
  Collect the comments into a **review** and **dispatch it as a new session** — the
  review is rendered into a preset prompt (the shared `~/.config/pohunek/prompts/`
  convention) and launched atomically via `session new --input` on the **same
  branch/worktree**, so the agent picks up the review and acts on it. Optionally
  also post the review to the PR via `gh pr review` / `gh pr comment`. Comments
  live **app-local until dispatch** (to keep the chassis free of a new surface);
  on dispatch the **review→session link** is written into the new session's `link.*`
  metadata, so a dispatched review is visible across surfaces. *(v1.1.)*

*Done when:* against ≥2 loopback-TCP stand-in daemons (CI for the SDK/data layer)
the app lists both hosts' sessions, shows a state change, and **spawns the
configured `attach_command`** for a selected session (the GUI is not on the
terminal I/O path); launching on a fixture
issue starts exactly one session on the expected branch with the rendered prompt;
the link persists across daemon restart and is byte-identical to a sway-script
link; given a session/worktree/PR with changes the app renders the diff, accepts
inline comments, and dispatching the review starts exactly one new session on the
**same** branch with the comments delivered as the prompt and the review→session
link persisted; no provider token appears in any daemon log, metadata, or event.

### Track B — Browser Control Center *(later / optional)*

Phase 4 as designed ([`phases/04`](phases/04-browser-control-center.md)) — a
standalone **TS aggregator backend** (Bun) + **Svelte 5 SPA** (xterm.js) + optional
single-cert **mobile PWA**, with the same provider seam. Kept in the roadmap as a
**later, optional** surface for **mobile / from-any-device** access (the one thing
a native desktop app can't give you). It reuses **Track S** (TS SDK), inherits
`web/backend` (`@pohunek/relay`) as the WebSocket daemon transport core, and
keeps the **same opaque-link store + prompt-template conventions** as the desktop
app and the sway scripts — one source of truth, multiple clients.

Built **after** the desktop app proves the SDK and the provider seam. Nothing here
changes the daemon (no gateway, no embedded assets, no daemon-side auth).

---

## 4. Deferred / out of scope

- **App-level auth / RBAC** — deferred while the trust boundary is NetBird/WireGuard
  + filesystem permissions (single operator). Addable in the browser backend later
  without daemon/protocol changes.
- **In-tree provider adapters in the chassis** — never; providers stay shell-out
  (`gh`) / GraphQL (Linear) in the clients.
- **libghostty / GTK / Electron native GUI** — dropped (replaced by the pure-native
  Rust desktop app for desktop, and the browser app for mobile).
- **Filesystem scanning / auto-discovery of repos; cross-host project unification;
  GC of stale auto-projects; per-project policy beyond `default_base_branch`** —
  out of scope (from the Projects design).

---

## 5. Recommended sequence

1. **Track S.1** — extract the **Rust SDK** (`crates/client`); low-risk refactor,
   unblocks the desktop app.
2. **Track S.2** — document the public API + version negotiation.
3. **Track D** — build the **native desktop companion app** (Iced control plane)
   on the Rust SDK; see [`design/track-d-native-app.md`](design/track-d-native-app.md)
   and [`phases/06`](phases/06-native-app.md). **v1:** D.1 (workspace + multi-host)
   → D.3 (session/project/worktree) → D.4 (prompts) → D.5 (Linear + PRs); attach is
   delegated (D.2 folded in). **v1.1:** D.6 (diff review + comment-to-session loop).
4. **Track B (later/optional)** — when mobile / from-any-device access is wanted:
   build the aggregator backend on the completed S.3 TS SDK and inherited
   `web/backend` relay core → Svelte SPA → PWA → provider parity, reusing the
   desktop app's provider seam and the shared link store.

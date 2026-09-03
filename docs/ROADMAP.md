# Pohunek — Roadmap

Consolidated, top-level roadmap for the whole project. This is the **index and
sequencing** source of truth; the per-phase design docs under
[`docs/phases/`](phases/) and [`docs/design/`](design/) remain the source of truth
for *what* and *why* inside each track.

Status reflects the **code on `main`**. Where a phase/plan doc's own status
header lags the code, the code wins and the lag is noted. Accepted future work
is marked explicitly and must not be read as shipped functionality.

Last reconciled: 2026-09-03.

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
| **Phase 5 / Milestone 12 — rofi/sway launcher** | `session list --filter` (chassis); `session new --input` (atomic launch); `scripts/` (launch-issue/pr, rofi); transient attach menu and status banner; `pohunek setup` for sway/rofi. | [`phases/05`](phases/05-rofi-sway-launcher.md), `scripts/` |

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

The shipped forward work built client surfaces on top of the owner-first
chassis: **SDKs → native desktop app → mesh-local browser control center**. The
daemon remains provider-agnostic and presentation-agnostic.

The accepted next product track is an **optional team relay**. It adds a
host-initiated network path and multi-team authorization without replacing
standalone or direct NetBird operation. It is tracked separately below because
none of its runtime features are shipped yet.

### Track H — Hermes runtime and operator plugin

The first-class local Hermes Agent runtime is pinned to `0.20.0`. The runtime
launches in Pohunek-owned PTYs, resumes only an exact recorded native reference,
and reports native fork as unsupported data. Hermes remains limited to its local
interactive backend; it does not turn Pohunek into a Hermes gateway, database
reader, SSH bridge, or remote-provider client.

The operator-plugin milestone adds an explicitly selected Hermes profile or
custom owner-private home to the existing CLI integration surface. The release
CLI embeds the managed plugin and the skill generated from `docs/knowledge`.
Its policy is external to immutable plugin checksums and requires explicit
access mode and host allowlist. Read/manage/full tool availability is bounded;
`full` alone exposes stop/remove. Hook reporting is local and best effort, while
the daemon stays authoritative for the exact eight origin-session denials and
the three lifecycle-report exceptions. No public protocol bump is introduced:
the one-time M1 range-negotiation boundary already covers M2 and M3.

Operational rollout is ordinary after M1: install matching local binaries,
reconcile workers, run integration doctor for a selected profile, and validate a
canary before enabling remote or full policy. Binary downgrade after Hermes enum
or provider-keyed notification-policy persistence is unsupported; recover by
upgrading forward. See the [operator guide](knowledge/guides/hermes-operator.md),
[rollout runbook](runbooks/hermes-operator-plugin.md), and
[migration guide](migrations/hermes-operator-plugin.md).

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
  `web/backend` (`@pohunek/backend`) as its tested WebSocket transport core
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
> **v1 = D.1+D.3+D.4+D.5** (D.6 was v1.1; D.6 has since shipped, minus the
> optional `gh pr review` posting, which is deferred).

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
- **D.6 — Diff review + comment-to-session loop (Kandev-style). Shipped, minus
  gh-posting (see [`design/track-d-native-app.md`](design/track-d-native-app.md)
  §6).** A unified diff surface for a **session's worktree or a PR**: render the
  diff (worktree diff vs the base branch for a session, via the new daemon
  method `session.diff`; `gh pr diff` for a PR), browse it by file/hunk, and add
  **inline comments anchored to `file:line`**. Collect the comments into a
  **review** and **dispatch it as a new session** — the review is rendered into
  a preset prompt (`~/.config/pohunek/prompts/review.tmpl`) and launched
  atomically via `session new --input` with `cwd` set to the **same worktree**
  the review was of (git forbids a second worktree on an already-checked-out
  branch), an agent picker seeded with the source session's own agent profile
  and freely overridable, so the (possibly different) agent picks up the
  review and acts on it in place. **Posting the review to the PR via `gh pr
  review` / `gh pr comment` is deferred**, out of scope for this milestone.
  Comments live **app-local until dispatch** (to keep the chassis free of a new
  surface) as one JSON file per review, persisted immediately on every
  add/edit/delete; reopening a review for the same source resumes the
  most-recently-updated persisted draft for that exact source (a dispatched
  review is never resumed), starting fresh only when none exists. On dispatch
  the **review→session link**
  (`review.source`, `review.dispatched_at`) is written into the new session's
  metadata alongside the source session's `link.*` keys, so a dispatched review
  is visible across surfaces.

*Done when:* against ≥2 loopback-TCP stand-in daemons (CI for the SDK/data layer)
the app lists both hosts' sessions, shows a state change, and **spawns the
configured `attach_command`** for a selected session (the GUI is not on the
terminal I/O path); launching on a fixture
issue starts exactly one session on the expected branch with the rendered prompt;
the link persists across daemon restart and is byte-identical to a sway-script
link; given a session/PR with changes the app renders the diff, accepts
inline comments, and dispatching the review starts exactly one new session in the
**same** worktree with the comments delivered as the prompt and the review→session
link persisted; no provider token appears in any daemon log, metadata, or event.
**All met** as of the D.6 milestone, except the optional `gh pr review` posting,
which is deferred.

### Track B — Mesh-local Browser Control Center *(M1 complete; later plan superseded)*

Phase 4 as designed ([`phases/04`](phases/04-browser-control-center.md)),
reconciled by the
[Track B plan](design/track-b-web-control-center-plan-2026-07-22.md): a thin
**Bun backend** (`@pohunek/backend` — pure transparent tunnels, host discovery via
the local daemon, SPA serving) + browser-side aggregation in
**`@pohunek/client-core`** + a **Svelte 5 SPA** (xterm.js) + optional
single-cert **mobile PWA**, with the same provider seam. The browser speaks
the public protocol verbatim over those tunnels; the backend holds no
protocol state. It remains an optional surface for **mobile /
from-any-device** access (the one thing a native desktop app cannot give
you), reusing **Track S** (TS SDK).

**M1 is implemented:** Slices B + C, the notifications inbox, and the
in-browser terminal provide the multi-host sessions workspace and live session
lifecycle through one mesh-local backend origin. This is a shipped owner-path
client, not the accepted public team relay.

The old M2/M3 production-backend direction is superseded by the
[team-relay RFC](design/team-relay-control-plane-rfc.md). The Rust relay and its
team web/CLI surfaces are implemented by
[#71](https://github.com/zajca/pohunek/issues/71) and
[#86](https://github.com/zajca/pohunek/issues/86); later provider delivery is
tracked by [#73](https://github.com/zajca/pohunek/issues/73). Until #86 lands,
the existing Bun backend remains documented and supported as the shipped
mesh-local transparent browser transport.

### Track R — Optional Team Relay *(accepted; not implemented)*

Umbrella: [#56](https://github.com/zajca/pohunek/issues/56). Source of truth:
the [accepted team-relay RFC](design/team-relay-control-plane-rfc.md).

This track adds one trusted, public, PostgreSQL-backed Rust `pohunek-relayd`.
Hosts initiate an embedded userspace WireGuard tunnel and every control/attach
connection. The relay owns principals, service accounts, teams, roles, session
ACLs, routing, aggregation, audit, and quotas; `pohunekd` owns stable host state,
local `HostShare` ceilings, immutable session origin, PTYs, processes, and
worktrees. Local and direct-NetBird sessions never become relay-visible.

Standalone, direct NetBird, relay-only, and NetBird-plus-relay topologies remain
first-class. A host has at most one relay enrollment but may expose multiple
locally approved shares to multiple teams. The implementation order follows the
live blocker graph:

1. [#80](https://github.com/zajca/pohunek/issues/80) lands the RFC and aligns
   canonical documentation.
2. [#81](https://github.com/zajca/pohunek/issues/81) adds stable host identity,
   exact principal-or-team ownership, and local ownership transfer;
   [#85](https://github.com/zajca/pohunek/issues/85) builds the Rust relay,
   PostgreSQL, OIDC, principals, teams, groups, roles, and service accounts.
3. [#72](https://github.com/zajca/pohunek/issues/72), after completed
   [#69](https://github.com/zajca/pohunek/issues/69) plus #81 and #85, adds
   enrollment and host-initiated userspace WireGuard/TCP transport.
4. [#70](https://github.com/zajca/pohunek/issues/70), after #72 and #81, makes
   the coordinated protocol v4 host-link, `SessionOrigin`, and daemon relay
   guard cutover. There is no v3 relay compatibility shim.
5. [#82](https://github.com/zajca/pohunek/issues/82) and
   [#83](https://github.com/zajca/pohunek/issues/83), after #70 and #85, add
   locally approved `HostShare` policy and relay-side session authorization.
   [#84](https://github.com/zajca/pohunek/issues/84), after #70 and #82, adds
   subscription-first atomic snapshots and full resync without daemon replay.
6. [#71](https://github.com/zajca/pohunek/issues/71), after #70, #72, #82,
   #83, #84, and #85, completes relay host links, routing, aggregation, attach
   proxying, and the typed public API.
7. [#86](https://github.com/zajca/pohunek/issues/86), after #71, supplies the
   team CLI and Svelte web surfaces and removes the production Bun backend
   authority. [#87](https://github.com/zajca/pohunek/issues/87), after #71,
   #72, and #85, completes audit, quotas, deployment, backup/restore,
   observability, and incident hardening.

Post-relay extensions are [#73](https://github.com/zajca/pohunek/issues/73)
for provider webhooks and encrypted token storage, and
[#88](https://github.com/zajca/pohunek/issues/88) for real profile-backed
container and VM isolation. Neither is part of the first complete relay
release.

---

## 4. Deferred / out of scope

- **Application auth in the shipped mesh-local Bun backend** — intentionally
  absent under the owner-path NetBird/filesystem trust boundary. The future
  team relay does require OIDC, service accounts, RBAC, and session ACLs from
  its first complete release; [#85](https://github.com/zajca/pohunek/issues/85)
  and [#83](https://github.com/zajca/pohunek/issues/83) own that work.
- **In-tree provider adapters in the chassis** — never; providers stay shell-out
  (`gh`) / GraphQL (Linear) in the clients.
- **libghostty / GTK / Electron native GUI** — dropped (replaced by the pure-native
  Rust desktop app for desktop, and the browser app for mobile).
- **Filesystem scanning / auto-discovery of repos; cross-host project unification;
  GC of stale auto-projects; per-project policy beyond `default_base_branch`** —
  out of scope (from the Projects design).

---

## 5. Recommended sequence

Tracks S, D, and Browser M1 are shipped and remain usable throughout the relay
work. The next sequence is Track R exactly as ordered above: #80, then #81/#85,
#72, #70, #82/#83/#84, #71, and finally #86/#87. Existing owner-path work may
continue independently only when it does not create a second production relay
authority or pre-empt a locked RFC boundary.

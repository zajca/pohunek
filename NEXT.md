# NEXT STEP — Milestone 12: rofi / sway launcher (Phase 5, delivered whole)

This file describes, in detail, the immediate next step. It is a handoff for
whoever picks up the work (you, a subagent, or a fresh session).

This milestone delivers Phase 5 as scoped in
[`docs/phases/05-rofi-sway-launcher.md`](docs/phases/05-rofi-sway-launcher.md):
make `pohunek` a first-class part of a tiling-WM (sway) keyboard workflow with the
thinnest possible client — shell scripts + a rofi switcher + sway window
orchestration — plus the two small chassis changes they need. It is the low-risk
proof that "the chassis is an API," and can land before the browser control
center (Phase 4).

## Naming / prerequisite note

The project was renamed **`zagentmesh` → `pohunek`**. This handoff uses `pohunek`
throughout. The **codebase rename** (binary, `zagentmesh-*` crates, doc-comments,
older docs) is a separate companion task and should land **first or alongside**
this milestone so the new CLI surface (`session list --filter`, `session new
--input`) ships under the final name. Until the rename lands, substitute
`zagentmesh` for `pohunek` in the commands below.

## Why this is one milestone, not a pile of scripts

The filtering API alone is **a query nobody acts on**; launcher scripts with no
switcher **don't compose into a workflow**; a switcher with no window glue is **a
list you still have to wire up by hand**. The user value is the *keyboard loop* —
hit a key, see agents filtered by state, pick some, and have their terminals tiled
in front of you while the ones you were viewing close (detach, not stop). So
Milestone 12 ships that whole loop, plus launching a session **on** a Linear issue
/ GitHub PR with a preset prompt. Internally it is slices A–F, but "done" is the
end-to-end workflow.

Only **Slices A and B touch the engine** (Rust); C–F are scripts + sway IPC living
under `scripts/`. The chassis stays provider-agnostic and WM-agnostic.

## Goal of milestone 12

```bash
# Chassis (the only Rust changes):
pohunek session list --filter state=blocked --filter agent=claude --json
pohunek session list --filter state=running -q          # ids only, for scripts
pohunek session new --host build-box --agent claude --repo . --input "Fix #1234: …"

# Client (scripts + WM glue under scripts/):
pohunek-launch-issue LIN-1234        # Linear issue -> session on its branch + preset prompt
pohunek-launch-pr 4567               # GitHub PR  -> session on the PR branch + preset prompt
pohunek-rofi                         # rofi switcher: pick agents by state, tile their attach terminals
```

Keybind flow (sway): a key runs `pohunek-rofi` → it merges sessions across all
reachable hosts, filtered by state → you multi-select → attach terminals for the
selection open (each marked), and the previously-open attach windows that are no
longer selected **close = detach** (their sessions keep running on their hosts).
An optional one-row banner above each attach pane names host / session / agent /
live state, since sway runs without window decorations.

Each host stays authoritative; cross-host aggregation is **client-side** (the CLI
dials one daemon at a time, so the switcher enumerates hosts via `host discover`
and merges per-host `session list` — no central server).

---

## Definition of done (testable)

Grouped by slice; the milestone is **not done until #1–#10 hold together** and the
end-to-end keyboard loop (#8) works on a real sway session.

### Slice A — CLI filtering API (chassis; shared with Phase 4)

1. `session list` accepts repeatable `--filter key=value`, **AND** semantics,
   **exact** match (Docker-style) over `SessionInfo` fields: `state`, `activity`,
   `agent`, and `id`. An unknown key or bad value is a typed **usage error** (the
   milestone-10 `cli_usage` envelope), never a silent empty result.
2. A quiet/scripting mode (`-q`, ids-only) and the existing stable `--json` both
   honor the filters; filtering is applied per-daemon and is transport-agnostic
   (local Unix socket and `--host` alike).
   *Check:* unit tests on the filter predicate (single match / no-match / multiple
   ANDed filters / unknown key → error); `--json` and `-q` return the same set;
   filtering works identically local and over a loopback-TCP stand-in.

### Slice B — Atomic launch input (chassis)

3. `session new` gains `--input <text>` (and/or `--input-file`): the daemon
   delivers the given bytes to the freshly-spawned PTY as the session's first
   input, in the **same** round-trip as creation (no separate `session input`).
4. Under `--json` the path stays non-interactive (it already requires `--yes` for
   a remote `session new`); `--input` does not introduce a prompt.
   *Check:* a session started with `--input "…"` receives those bytes (assert via
   the agent echoing / a shell session); local and `--host` behave identically;
   `--json` output is unchanged in shape.

### Slice C — Launcher scripts (provider context + preset prompts)

5. `scripts/pohunek-launch-issue <linear-id>` and `scripts/pohunek-launch-pr
   <gh-pr>` derive context from the provider (title / body / branch), render a
   **preset prompt from a template**, and start a session via `session new
   --agent/--repo/--branch --input <rendered-prompt>`.
6. Prompt templates are **plain files** in the config dir
   (`~/.config/pohunek/prompts/*.tmpl`) with simple `${var}` substitution;
   per-script defaults (agent, host, repo) live in config. GitHub uses the
   official `gh`; Linear uses a **community CLI behind a thin script seam** (so it
   can be swapped for GraphQL/MCP). Provider credentials come from those tools'
   own auth and are **never inlined** into commands, logs, or session metadata.
   *Check:* given a fixture issue/PR, the script starts exactly one session with
   the expected agent/branch and the rendered prompt delivered as input; no token
   appears in argv, the event log, or session metadata.

### Slice D — rofi switcher (cross-host merge)

7. `scripts/pohunek-rofi` enumerates reachable hosts (`host discover --json`), runs
   `session list --host <h> --filter <…> --json` per host **concurrently** (short
   timeout; partial results with a per-host error marker on failure), merges into
   rows tagged with host + session id + agent + state, feeds `rofi -dmenu` with
   **multi-select**, and returns the chosen `host/session-id`s.
   *Check:* against ≥2 loopback-TCP stand-in daemons, the switcher lists both
   hosts' sessions under a state filter and returns a correct multi-selection; a
   down host yields a marked partial result, not a hang.

### Slice E — sway orchestration (open = attach, close = detach)

8. Selecting opens an attach terminal per chosen session
   (`$TERMINAL -e pohunek attach <host>/<id>`), each tagged with a unique **sway
   mark** (`pohunek:<host>/<id>`) via `swaymsg`. Attach windows for sessions **no
   longer** selected are closed; closing an attach window **detaches** (the attach
   client exits on window close / SIGHUP) and the session keeps running on its
   host.
   *Check (manual, on a real sway session):* after switching selections, only the
   chosen sessions have attach windows; the deselected sessions are **still
   listed as running** on their hosts (detach ≠ stop); re-selecting reattaches to
   the same live session. (The detach≠stop guarantee itself is already covered by
   the Phase 1/2 attach integration tests; this slice verifies the window glue
   triggers a detach, not a kill.)

### Slice F — Optional session banner

9. An attach terminal can show a one-line banner at the top naming
   host / session / agent / live agent-state via a **separate one-row sway pane**
   above the attach pane, fed from the event subscription
   (`agent_state`, `session_*`), refreshed on state change, surviving the agent's
   full-screen TUI.
   *Check:* the banner reflects a state transition (e.g. `running` → `blocked`)
   while the agent TUI is active and does not corrupt the TUI's rendering.

### Cross-cutting

10. `cargo build`, `cargo clippy --all-targets --workspace -- -D warnings`, and
    `cargo test --workspace` stay clean. Scripts have no secret in argv/logs; the
    chassis gained **no** provider or WM knowledge (only the read-side filter and
    the `--input` convenience).

---

## Decisions (already resolved — see the phase doc)

- **Filter API:** Docker-style **exact** `--filter key=value`, repeatable AND, over
  `state`/`activity`/`agent`/`id`; `-q` + `--json`; unknown key → usage error.
- **Atomic launch:** add `session new --input` (start + first prompt in one
  round-trip).
- **Linear access:** a **community CLI behind a script seam**; GitHub via `gh`.
- **Banner:** a **separate one-row sway pane** (robust against the full-screen TUI).
- **Window marking:** the user's **`$TERMINAL`** + a unique **sway mark** per attach
  window (terminal-agnostic).
- **Prompt templates:** plain files in the config dir with `${var}` substitution
  (no template-engine dependency).

## References / facts to build on

- **`SessionInfo` fields** (filter targets): `id`, `agent`, `cwd`, `pid`, `cols`,
  `rows`, `state`, `state_source`, `activity` (`crates/protocol/src/session.rs`).
- **`session new` args** today: agent, cwd, cols, rows, repo, branch, base_branch
  (`crates/cli/src/commands/session.rs` `NewArgs`) — add `--input` here.
- **`session input`** already exists (`SESSION_INPUT`); `--input` reuses the same
  daemon-side delivery, just at creation time.
- **`session list` is per-daemon**; there is no cross-host list — aggregation is
  the switcher's job, consistent with "no central server" (Phase 2).
- **detach ≠ stop**: the daemon owns the PTY; an attach is a separate connection,
  so closing it detaches and the session survives (Phase 1/2 guarantee).
- **`--json` everywhere + `cli_usage` error envelope** already exist (Milestone
  10) — the filter's bad-key error and the switcher's JSON parsing build on them.
- **sway IPC**: tag/find/close windows via `swaymsg` marks; the rofi parts,
  launcher scripts, and the filter API are WM-agnostic — only Slices E/F are
  sway-specific and can be ported to another WM later.

## Suggested build order

A → B first (the only Rust changes; A unblocks the switcher and Phase 4, B makes
launch atomic), then C (scripts), D (rofi), E (sway glue), F (banner). A and B are
fully CI-testable; D is testable against loopback daemons; E/F need a real sway
session and are verified manually.

## Out of scope (deferred to later milestones)

- The browser control center, the public-API/TS-type-generation contract, the
  daemon HTTP/WS gateway, and the mesh CA — all Phase 4
  (`docs/phases/04-browser-control-center.md`).
- In-tree provider adapters in the chassis (providers stay `gh` / Linear CLI in
  the scripts).
- Non-sway window managers for the orchestration slice.
- New session semantics — the Phase 1–2 lifecycle is reused unchanged.

# Phase 5: rofi / sway Launcher & Session Switching (+ CLI Filtering)

## Objective

Make `pohunek` a first-class part of a tiling-WM (sway) keyboard workflow with
the **thinnest possible client**: a set of shell scripts that drive the existing
CLI, plus a rofi switcher and sway window orchestration. Launch agent sessions
pre-loaded with provider context (a Linear issue / GitHub PR) and a preset
prompt; pick running agents by state in rofi and have their terminals tiled in
front of you, detaching whatever you were viewing — without stopping any session.

The **only chassis change** is a Docker-style **filtering API** on `session list`.
Everything else lives outside the engine as scripts + WM glue. This phase is the
low-risk proof that "the chassis is an API," and it can land **before** Phase 4.

## User Value

- One keybind → rofi → see agents filtered by state (e.g. only `blocked` /
  needs-input), multi-select, and their attach terminals appear tiled; the ones
  you were looking at close (detach), the sessions keep running.
- One command to start an agent **on** a Linear issue or GitHub PR with a preset
  prompt, branch, and worktree already wired up.
- An optional banner at the top of each session terminal naming the
  host/session/agent/state — because sway runs without window decorations.

## Design principles

- **Thin client on the chassis API.** rofi/sway/provider logic is scripts calling
  `pohunek`, `gh`, and Linear's tooling. The engine gains no provider or WM
  knowledge.
- **One chassis change only:** a filtering API on `session list` (Docker-style).
  It also benefits the Phase 4 browser client, so it belongs in the chassis and
  should land first.
- **Cross-host aggregation is the client's job.** `session list` is per-daemon
  (Phase 2: the CLI dials one host at a time). The switcher enumerates reachable
  hosts via `host discover` and merges `session list --host <h>` results
  client-side — no central server.
- **Closing an attach window detaches, never stops.** The daemon owns the PTY
  (Phases 1–2: detach ≠ stop). The switcher closes attach *windows*; the sessions
  survive. This must be an explicit, tested guarantee of the orchestration.

## Scope

- **CLI filtering API** (the chassis change): `session list --filter key=value`,
  repeatable, AND semantics, Docker-style, over the existing `SessionInfo` fields.
- **Launcher scripts** (provider-aware, outside the chassis): start a session on a
  Linear issue / GitHub PR with a preset prompt, branch, and worktree.
- **rofi switcher**: format a filtered, cross-host session list for `rofi -dmenu`,
  support multi-select, return chosen `host/session-id`s.
- **sway orchestration**: open marked attach terminals for the selection and close
  the previously-open attach windows that are no longer selected (detach, not
  stop).
- **Optional session banner**: `pohunek attach` can reserve the top row in the
  attach terminal for host/session/agent/live-state, for decoration-less WMs.

## Out of Scope

- Provider integrations inside the chassis (scripts use `gh` and Linear's
  MCP/GraphQL; credentials stay in those tools' own auth).
- Non-sway window managers for the orchestration slice (the CLI filter, launcher
  scripts, and rofi parts are WM-agnostic; only the window-open/close glue is
  sway-specific and can be ported later).
- The browser control center (Phase 4) and any GUI.
- New session semantics; the Phase 1–2 lifecycle is reused.

## Slices and Definition of Done (testable)

### Slice A — CLI filtering API (chassis)

1. `session list` accepts repeatable `--filter key=value` with AND semantics over
   `SessionInfo` fields: at least `state`, `activity`, `agent`, and `id`
   (exact match — see the Decisions section, "No substring/glob in v1"); document
   the exact key set and matching rules.
2. A quiet/scripting mode (`-q` / ids-only, Docker-style) and the existing
   stable `--json` both honor the filters; an unknown filter key or bad value is a
   typed **usage error** (the milestone-10 `cli_usage` envelope), not a silent
   empty result.
   *Check:* unit tests on the filter predicate (match, no-match, multiple ANDed
   filters, unknown key → error); `--json` and `-q` return the same filtered set;
   filtering is applied per-daemon and is transport-agnostic (local and `--host`).

### Slice B — Launcher scripts (provider context + preset prompts)

3. `scripts/pohunek-launch-issue <linear-id>` and `scripts/pohunek-launch-pr <gh-pr>` derive
   context from the provider (title/body/branch), render a **preset prompt from a
   template**, and start the session **with the first prompt atomically** via the
   new `session new --input <text>` (decided — one round-trip, no `new` → `input`
   race; see Decisions).
4. Prompt templates and per-script defaults (agent, host, repo) live in config
   files; provider credentials come from `gh`'s and Linear's own auth and are
   **never inlined** into commands, logs, or the session metadata.
   *Check:* given a fixture issue/PR, the script starts exactly one session with
   the expected agent/branch and the rendered prompt delivered as input; no token
   appears in the command line, the event log, or session metadata.

### Slice C — rofi switcher

5. A `pohunek-rofi` script enumerates reachable hosts (`host discover --json`), runs
   `session list --host <h> --filter <…> --json` per host, **merges client-side**
   into rows tagged with host + session id + agent + state, and feeds `rofi
   -dmenu` with **multi-select**; selection returns the chosen `host/session-id`s.
   *Check:* against ≥2 loopback-TCP stand-in daemons, the switcher lists both
   hosts' sessions under a state filter and returns a correct multi-selection.

### Slice D — sway orchestration (open/close = attach/detach)

6. Selecting in rofi opens an attach terminal per chosen session
   (`$TERMINAL -e pohunek attach <host>/<id>`), each **marked** (sway mark /
   `app_id`/title set by the launcher) so the switcher can find it later.
7. Attach windows for sessions **no longer** in the selection are closed; closing
   an attach window **detaches** (the attach client exits on window close/SIGHUP)
   and the session keeps running on its host.
   *Check:* after switching selections, only the chosen sessions have attach
   windows; the deselected sessions are still listed as running (detach ≠ stop) on
   their hosts; re-selecting reattaches to the same live session.

### Slice E — Optional session banner

8. An attach terminal can show a one-line banner at the top naming
   host / project / session name / session id / agent / live agent-state,
   refreshed on state change, surviving the agent's full-screen TUI.
   *Check:* the banner reflects a state transition (e.g. `running` → `blocked`)
   while the agent TUI is active, and does not corrupt the TUI's rendering.

## Architecture Impact

- The **only** engine change is the `session list` filter API (Slice A); it is a
  read-side query addition with no protocol or session-semantics change, and it is
  shared with the Phase 4 browser client.
- Everything else is scripts + sway IPC living under `scripts/` (or a companion
  repo). The chassis stays provider-agnostic and WM-agnostic.
- Cross-host aggregation stays client-side (per-daemon `session list`), preserving
  "no central server."
- The orchestration relies on the existing detach ≠ stop guarantee; the banner
  consumes the existing event stream / `session inspect`.

## Risks

- **Closing a window might stop instead of detach.** Mitigation: assert the attach
  client treats window close / SIGHUP as detach; cover with the Slice D check
  (session still listed after close).
- **Banner vs full-screen TUI.** A banner repaint that changes terminal modes
  fights a TUI that owns cursor addressing. Mitigation: make the banner an
  optional `pohunek attach` overlay, resize the daemon PTY to one fewer row,
  constrain the local session viewport to rows below the banner while attached,
  and repaint the banner after output, resize, event updates, and a short
  periodic interval.
- **Cross-host fan-out latency.** Probing/listing many hosts per rofi invocation
  can be slow. Mitigation: query hosts concurrently, honor a short timeout, and
  show partial results with a per-host error marker.
- **Linear tooling.** There is no official first-party Linear CLI, so the chosen
  community CLI is a third-party dependency (maintenance / breaking-change risk
  outside our control). Mitigation: keep Linear access behind a thin script seam
  so the tool can be swapped (for GraphQL or MCP) without touching the launchers.

## Decisions (resolved)

- **Filter keys & matching.** Docker-style **exact** `--filter key=value`,
  repeatable, AND semantics, over `state`, `activity`, `agent`, and `id`; an
  unknown key or bad value is a typed usage error. No substring/glob in v1.
- **Atomic launch.** Add `session new --input <text>`; the launcher scripts start
  the session with the first prompt in one round-trip (no `new` → `input` race).
- **Linear access.** A **community Linear CLI**, kept behind a thin script seam so
  it can be swapped (GraphQL/MCP) later; GitHub via the official `gh`.
- **Banner.** An **in-band attach overlay** reserves the first terminal row and is
  fed from `session.inspect` plus the event stream. The rofi/sway launcher does
  not spawn a second banner window.
- **Window marking.** Use the user's **`$TERMINAL`**; tag each attach window with a
  unique **sway mark** (`pohunek:<host>/<id>`) via `swaymsg` so the switcher finds
  and closes it. Terminal-agnostic.
- **Prompt templates.** Plain template files in the config dir
  (`~/.config/pohunek/prompts/*.tmpl`) with simple `${var}` substitution — no
  template-engine dependency.

## Success Criteria

- A keybind opens rofi showing agents filtered by state across all reachable
  hosts; multi-selecting tiles their terminals and detaches the rest, with every
  session still running.
- `pohunek-launch-issue` / `pohunek-launch-pr` start an agent on the right branch/worktree
  with a preset prompt, no credential ever inlined.
- `session list --filter` works identically local and remote, in `--json` and
  quiet mode, with typed errors for bad filters.
- An optional banner names the host/session/agent/state at the top of each session
  terminal under a decoration-less sway setup.

## Exit Criteria

- The `session list` filter API ships in the chassis with tests and stable
  `--json`/`-q` output, and is reused by Phase 4.
- The launcher scripts, rofi switcher, and sway orchestration deliver the
  pick-by-state → attach/detach workflow end to end against ≥2 hosts.
- Closing an attach window is a verified detach (sessions survive); the chassis
  gained no provider or WM knowledge.

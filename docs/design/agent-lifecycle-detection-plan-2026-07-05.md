# Bulletproof agent lifecycle detection plan (2026-07-05)

Status: proposed. Diagnosis complete; implementation not started.

Goal: the daemon must reliably detect, for every session it owns — and
optionally for agents running entirely outside pohunek — that:

- (a) an agent (claude/codex) **started** from a shell,
- (b) the agent **stopped** (clean exit, crash, `kill -9`, closed terminal),
- (c) the process **changed its working directory**,
- (d) the process **moved to another git worktree**,

with no scenario in which the daemon's picture of the world stays stale
forever.

> **Scope:** daemon (`detect/`, `session/`, `integration/`, new `procwatch/`
> module), hook assets, protocol (`SessionInfo` additions + events), CLI/GUI
> display, `docs/public-api.md`, knowledge bundle. Linux first; macOS behind a
> trait.

## D0. Diagnosis (done)

pohunek today has two detection systems, and neither one ever looks at the OS
process table:

1. **PTY-output activity detector** (`crates/daemon/src/detect/`) — parses
   bytes the PTY emits: OSC 2 (title) + OSC 9 (progress) in `detect/osc.rs`,
   VT100 screen scrape regions, and TOML manifest keyword matching
   (`detect/manifests/{shell,codex,claude}.toml`), debounced by the state
   machine in `detect/machine.rs` (`recheck_after=100ms`, `confirmations=3`,
   `startup_grace=3s`). `ProcessActivityScanner` (`detect/mod.rs:368`) is a
   byte-flow heuristic despite its name. This answers *Working/Idle/Blocked*,
   never *started/stopped*. It cannot tell `claude` launching from `cargo
   build` printing output.

2. **Hook-based lifecycle reporting** (`crates/daemon/src/integration/`) —
   the only lifecycle signal. `session_pty_env` (`session/hooks.rs:163`)
   injects `POHUNEK_ENV`, `POHUNEK_SOCKET_PATH`, `POHUNEK_SESSION_ID`,
   `POHUNEK_PROTOCOL_VERSION` into every session PTY. The SessionStart hook
   asset (`pohunek-agent-state.sh`) calls `session.report_agent` +
   `session.report_native_id`; the notify asset calls `notification.create`.

### Confirmed gaps (each verified in source)

| # | Gap | Evidence |
|---|-----|----------|
| G1 | **Nested agent stop is never detected.** `session.release_agent` exists (`session/mod.rs:1098`) but no shipped hook calls it; repo-wide the only caller is the API dispatcher (`api/handler/session.rs:225`). The notify hook's `stop` action only creates a `turn_completed` notification. `active_agent` clears only when the whole parent PTY exits (`session/mod.rs:1551-1556`). | grep: zero `release_agent` in `integration/assets/` |
| G2 | **No liveness or expiry on `active_agent`.** `ActiveAgentReport` (`session/mod.rs:320`) holds `source/agent/seq/activity_reported` — no PID, no timestamp. A SIGKILL'd agent leaves `active_agent` (and the swapped detector manifest) pinned forever. | struct inspection |
| G3 | **cwd is static.** `SessionInfo.cwd` is captured once at `session.new` (`session/mod.rs:815`) and only ever read afterwards (`hooks.rs:188`, `resume.rs:186`). No OSC 7 parsing exists (`detect/osc.rs` handles only OSC 2/9). | grep: zero `info.cwd` mutation, zero OSC 7 |
| G4 | **Worktree switch mid-session is invisible.** `worktree_path` binds at create/resume time via `WorktreeManager`; a `cd` into another worktree inside the shell changes nothing. | `session/mod.rs:279-378` |
| G5 | **Agents outside pohunek are invisible.** Hooks `exit 0` without `POHUNEK_ENV`/`POHUNEK_SESSION_ID`. A missing `POHUNEK_SESSION_ID` can also produce session-less notifications from inside a pohunek shell. | `state.sh:25-28`, `notify.sh:22-24` |
| G6 | **No process inspection anywhere.** No `/proc`, no `pidfd`, no `sysinfo` usage in the daemon (the only mention is an ESRCH comment in `pty/mod.rs:442`). | grep |

## D1. Prior art (researched 2026-07-05)

- **Herdr** (herdr.dev): process-name heuristics on the pane's foreground
  process **plus** terminal-output pattern matching from hot-reloadable TOML
  manifests, **plus** optional agent hooks as the highest-priority evidence.
  Survives kill -9 via pane/process state. pohunek's manifest detector is the
  same idea; what Herdr adds is the *process* leg of the tripod.
- **vibetunnel**: PTY wrapper; process exit observed at PTY level; Claude
  activity parsed from terminal-title mode. Same blind spot pohunek has for
  nested processes.
- **claude-squad / Conductor / Crystal**: worktree-per-session by
  construction — they never need to *detect* worktree changes because the
  isolation boundary prevents them. Not our model (pohunek sessions are free
  shells), but it confirms worktree mapping belongs to the session manager.
- **coder/agentapi**: terminal-snapshot diffing only; explicitly cannot see
  process exit or cwd. Weaker than what we need.
- **Native agent mechanisms**: Claude Code hooks (SessionStart/SessionEnd/
  Stop/Notification) and Codex notify/hooks carry `session_id`,
  `transcript_path`, `cwd` — rich but **push-based and lossy**: SessionEnd
  never fires on SIGKILL or terminal teardown. Transcript JSONL dirs
  (`~/.claude/projects/`, `~/.codex/sessions/`) are watchable with inotify
  and survive dirty exits.
- **OS level (Linux)**: `pidfd_open(2)` + epoll gives guaranteed exit
  notification (including SIGKILL) for any same-user PID, no privileges
  needed. `/proc/PID/cwd` readlink gives cwd for same-user processes.
  Descendant discovery via `/proc/PID/task/*/children` or a ppid walk.
  netlink proc connector and eBPF give exec events but need
  `CAP_NET_ADMIN`/`CAP_BPF` — **rejected as a core dependency**; the daemon
  must work unprivileged.
- **OS level (macOS)**: `libproc` (`proc_listchildpids`,
  `proc_pidinfo(PROC_PIDVNODEPATHINFO)` for cwd) + `kqueue`
  `EVFILT_PROC/NOTE_EXIT`. Endpoint Security needs a system extension —
  rejected for the same reason.

### Design principle: evidence tripod, process facts win

Bulletproof means no single lossy channel is load-bearing:

1. **Process facts** (new `procwatch`): what is *actually running* under the
   session's PTY child — authoritative for start/stop/cwd. Polling for
   discovery, event-driven (`pidfd`/`kqueue`) for exit.
2. **Hooks**: fastest and richest (native session id, transcript path) — but
   treated as *claims* that must be bound to a live PID and expire when the
   process facts stop backing them.
3. **PTY output** (existing detector + new OSC 7): activity states and
   instant cwd hints — never lifecycle-authoritative.

The reconciliation rule: **any state derived from a claim (hook, OSC) must be
verifiable against process facts and must age out when it is not.** That is
the property whose absence causes G1/G2.

## P1. `procwatch`: per-session process observer (Linux)

New module `crates/daemon/src/procwatch/` behind a trait so macOS can follow:

```rust
trait ProcessInspector: Send + Sync {
    fn descendants(&self, root: Pid) -> io::Result<Vec<ProcessFact>>; // pid, ppid, comm, cmdline
    fn cwd(&self, pid: Pid) -> io::Result<PathBuf>;
    fn exit_watch(&self, pid: Pid) -> io::Result<ExitWatch>; // pidfd + epoll on Linux
}
```

- One watcher task per running session, rooted at the PTY child PID the
  daemon already owns. Tick interval ~1s (config `procwatch_poll_ms`,
  default 1000, in the daemon config — no hardcoded literal), plus an
  immediate rescan on `report_agent` arrival and on detector
  Working-transition (cheap triggers that catch short-lived starts).
- **Agent identification is data-driven**: extend the existing detector
  manifests (`detect/manifests/*.toml`) with a `[process]` section —
  `comm` / `cmdline` regexes per agent kind (e.g. codex = `codex` binary,
  claude = `node`/`bun` with `claude` in argv). Same hot-swappable shape
  Herdr uses.
- On match: record `ObservedAgent { pid, agent_base, first_seen, cwd }` on
  the session entry and arm a `pidfd` exit watch. On exit event: clear it
  and kick reconciliation (P2) — this is the guaranteed stop signal for
  kill -9, crashes, and closed terminals.
- Fallback when `/proc/PID/task/*/children` is unavailable: full same-user
  `/proc` ppid walk (one pass, shared by all session watchers per tick).

Success criteria: integration test starts a fake agent (script whose comm
matches a test manifest) inside a shell session, asserts observed-agent
appears ≤2 ticks after spawn; `kill -9` the fake agent, assert the exit event
fires and the observation clears without waiting for a poll tick.

## P2. Reconciliation: `active_agent` bound to process facts

- `ActiveAgentReport` gains `pid: Option<Pid>` and `reported_at: Instant`.
- Binding: when a hook `report_agent` arrives, bind it to the matching
  `ObservedAgent` (hook gains a `pid` field — the asset sends the agent's own
  PID, see P3; otherwise bind by agent_base match on the observed set).
- **Auto-report**: an `ObservedAgent` with no hook claim still sets
  `active_agent`/`active_agent_base` (source `pohunek:procwatch`, no native
  session metadata) and switches the detector manifest — so detection works
  even with hooks uninstalled or broken.
- **Auto-release**: when the backing PID exits (pidfd event) or an unbound
  claim outlives `active_agent_claim_ttl` (config, default 30s) with no
  matching observed process, run the existing `release_agent` path
  (`session/mod.rs:1098`) — restoring the parent detector manifest. This
  closes G1+G2 even if every hook is missing.
- Ordering stays governed by `seq`/`report_is_current`
  (`session/mod.rs:1718`); procwatch events carry their own monotonic seq so
  a late hook release cannot clobber a newer auto-report.

Success criteria: unit tests over the reconciler with a mock
`ProcessInspector` covering: hook-then-exit, exit-then-late-hook, SIGKILL
release, claim-expiry with no process, nested agent restart (new PID) while
old claim pending.

## P3. Hook fast path: release + PID binding

- **Claude asset**: register a `SessionEnd` hook (and keep `Stop` for
  notifications) that calls `session.release_agent`; `SessionStart` payload
  adds the hook process's parent PID (`$PPID` from the hook = agent PID) so
  P2 can bind exactly.
- **Codex asset**: codex has no SessionEnd equivalent that survives dirty
  exit; wire whatever end-of-session hook exists (Stop-level) as best-effort
  release. Correctness does not depend on it — P2 auto-release is the
  backstop.
- Bump `POHUNEK_INTEGRATION_VERSION`; hook payload additions ripple into
  `api/handler/session.rs` request parsing.
- Keep the existing rule: hooks are *fast path only*. A hook release with a
  stale seq or mismatched PID is ignored (already partly guarded by
  `release_matches`, `session/mod.rs:1725`).

Success criteria: with hooks installed, release lands <1s after clean agent
exit (hook path); with hooks deleted, release still lands (procwatch path);
both asserted in one integration test matrix.

## P4. cwd + worktree change tracking

- **cwd**: each procwatch tick reads `/proc/<pid>/cwd` of the *focus
  process* — the active agent PID when set, else the PTY child (shell). On
  change: update `SessionInfo.cwd`, emit the session-updated event, and
  record `cwd_source` (`procwatch` | `osc7`).
- **OSC 7 fast path**: add OSC 7 (`file://host/path`) parsing to
  `detect/osc.rs` beside OSC 2/9; feed it as a cwd *hint* that procwatch
  verifies on its next tick (shells lie less than they misconfigure — a
  hint that /proc contradicts is dropped).
- **Worktree mapping**: on every cwd change, resolve the new cwd against
  `WorktreeManager`'s known worktrees and registered projects/repositories
  (prefix match on canonicalized paths; fall back to walking up to a `.git`
  file/dir). Update the session's `worktree_path`/project association fields
  and emit the same session-updated event. A session that leaves its
  worktree shows that truthfully instead of the stale binding (G4).
- Protocol ripple: `SessionInfo.cwd` becomes documented-as-dynamic;
  consider a dedicated `session.cwd_changed` payload only if GUI needs
  deltas — default is reusing the existing session-update event (pohunek has
  no back-compat constraints).
- GUI/CLI: `session list`/`inspect` and the GUI detail already render cwd —
  verify they re-render on the update event; show worktree drift (bound
  worktree vs current cwd) in the detail view.

Success criteria: integration test `cd`s inside a session shell, asserts
`SessionInfo.cwd` updates within 2 ticks and again instantly when the shell
emits OSC 7; test that `cd` into a second registered worktree updates the
worktree association and back.

## P5. External agents (outside pohunek) — observer mode, opt-in

Detect agents the user starts in a terminal pohunek does not own:

- inotify (Linux) watch on `~/.claude/projects/**` and
  `~/.codex/sessions/**` for JSONL create/write → candidate external
  sessions with native session id, transcript path, cwd (parsed from the
  first JSONL lines).
- Same-user `/proc` sweep (reusing the P1 shared walker) for agent-matching
  processes not under any pohunek session; join with transcript candidates;
  arm `pidfd` exit watches the same way.
- Surface as read-only `external: true` entries in `session.list` (new
  protocol surface) so the GUI can show "agents running outside pohunek";
  no PTY, no input, optional "adopt" later (out of scope here).
- Opt-in config flag (`observe_external_agents`), default off — it watches
  the user's home directory and that should be a deliberate choice.

Success criteria: with the flag on, starting claude in a plain terminal
shows an external entry within 2s and it disappears on `kill -9`.

## P6. macOS backend

Implement `ProcessInspector` with `libproc` (`proc_listchildpids`,
`PROC_PIDVNODEPATHINFO` for cwd) and `kqueue` `EVFILT_PROC/NOTE_EXIT` for
exit watches. Pure backend work behind the P1 trait; no reconciler changes.
Deferred until a macOS host is actually in play.

## P7. Docs and knowledge bundle (rippled through every phase, not last)

Per repo policy each phase updates in the same change:

- `docs/knowledge/concepts/sessions.md` — the active-agent paragraph
  currently claims releasing clears the fields without admitting nothing
  ships a release; rewrite around the tripod model once P2/P3 land.
- `docs/public-api.md` + knowledge bundle for: `report_agent` PID field,
  auto-release semantics, dynamic `cwd`, worktree drift fields, external
  session entries. Run `cargo xtask docs check` per phase.
- `docs/knowledge/assistant/source-map.md` for the new `procwatch/` module.

## Ordering and risk

P1 → P2 are the core and fix the worst live bug (stale `active_agent`,
G1/G2). P3 is small and mostly shell. P4 delivers the user-visible cwd/
worktree tracking and depends only on P1. P5 and P6 are independent
extensions. Riskiest piece is P2 ordering (hook seq vs procwatch seq) —
covered by the mock-inspector unit matrix before any wiring.

Gates per phase: workspace clippy `-D warnings`, full test suite, `cargo
xtask docs check`; known flaky tests (stale-socket, fake-gh, remote_tcp PTY)
re-run in isolation before judging failures.

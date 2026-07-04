# Plan Review: Cross-Host Agent Notifications

Reviewed plan: `2026-07-03-cross-host-agent-notifications.md`
Reviewed on: 2026-07-03
Method: verified against (1) the actual codebase and (2) current upstream docs for
Codex CLI and Claude Code hooks.

## Verdict

Architecturally feasible and consistent with how pohunek works today (broadcast
events, jsonl append store, hooks talking to the daemon over the Unix socket).
NOT ready for literal implementation: one external assumption is under-specified
(and, once corrected, actually works better than the plan implies), plus three
concrete design/scope gaps that would otherwise cause non-functioning behavior or
the exact notification noise the feature is meant to prevent.

## External facts verified (Codex / Claude hooks)

### Codex CLI now has a full hook system (not just `notify`)
- `PermissionRequest` fires when Codex is about to ask for approval -> maps to
  `approval_required`.
- `Stop` fires at turn/agent completion -> maps to `turn_completed`.
- `SessionStart` (already used by the existing `pohunek-agent-state.sh`).

Correction the plan must absorb: the legacy `notify` mechanism fires ONLY on
`agent-turn-complete`, never on approval (open issue openai/codex#11808). If the
Codex adapter were built on `notify`, `approval_required` from Codex would be
impossible. The plan says "hook adapter", which is compatible with the modern
`config.toml [hooks]` API, but never states this explicitly. Task 5 should say
plainly: Codex adapter registers as `PermissionRequest` + `Stop` hooks in
`config.toml [hooks]`, NOT via `notify`.

### Claude Code `Notification` is a family, not one event
`Notification` is disambiguated by a matcher; `Stop` is separate:
- `permission_prompt` -> `approval_required`
- `agent_needs_input` / `idle_prompt` -> `agent_blocked`
- `Stop` -> `turn_completed`

Task 5's blanket "Claude `Notification` input mapping to `approval_required`" is
too coarse. Mapping must be per-matcher, otherwise `idle_prompt` and
`permission_prompt` collapse into one kind.

### Version dependency (missing from the plan)
Both hook surfaces (Codex `PermissionRequest`, Claude `Notification` matchers) are
recent. The feature only works on sufficiently new builds of both agents. This
compatibility requirement should be documented.

## Codebase accuracy findings

Most references are accurate: crates exist, `pohunek-protocol`, `method`/`event`
constants, `SessionId` transparent newtype, `EventLog` writing `events.jsonl`,
`DaemonState`, broadcast-based subscribe, existing hook assets over
socket + python3. Inaccuracies:

1. `crates/daemon/src/api/server.rs` does not exist — subscribe lives in
   `crates/daemon/src/api/mod.rs` (`serve_connection`, `run_event_subscription`).
   Non-blocking; the plan already hedges this.
2. `Subscription::next_event()` does not exist — the plan correctly proposes to
   add it (Task 6). OK.
3. CLI `--all-hosts` / cross-host querying does not exist anywhere in the CLI.
   Fan-out lives only in `gui-core` (`lib.rs:2670`). Today every CLI invocation
   targets a single host. Task 7 treats `--all-hosts` as one bullet, but it is
   new CLI fan-out infrastructure from scratch. Re-scope.
4. `AgentActivity` has only `Working`/`Blocked`/`Idle` — no `failed`/`finished`.
   Termination is carried by `SessionState` + the `session_stopped` event. The
   projector must consume both agent_state and session lifecycle signals, not
   just "session event broadcasts" generically.

## Top functional risks

### A) Cross-producer duplication (most severe)
The same logical "agent is waiting for input" state is produced by TWO
independent sources: the screen-based projector (Task 4 -> `agent_blocked`, vt100
detection) and the provider hook (Task 5 -> `approval_required` from Claude
`permission_prompt` / Codex `PermissionRequest`). Dedup in the plan is only
WITHIN a producer (projector dedup in Task 4, `source_id` idempotency in Task 2).
There is no dedup BETWEEN the projector and the hook. Result: two notifications
for one blocked event — precisely the noise the feature exists to remove. Needs a
design decision (e.g. hook wins; suppress projector `agent_blocked` when a hook
`approval_required` arrived for the same session within a window).

### B) The `error` signal may not exist
Task 4 promises `error` "when a session fails". Failure is not carried by
`AgentActivity`; it is carried by `session_stopped`. It is NOT confirmed that
`session_stopped` distinguishes a clean exit from a crash (exit code / status).
If it does not, `error` cannot be derived from daemon-side state alone and must
come from a hook event (and neither Claude nor Codex has a clean "error" hook —
only an exit signalled through `Stop`). Verify the `session_stopped` payload
before committing to daemon-derived `error`.

### C) The installer must learn new hook event keys
The existing `integration/mod.rs` merges the pohunek hook into `SessionStart`
only. The new notification hook requires registering additional events: Claude
`Notification` + `Stop`, Codex `PermissionRequest` + `Stop`. Task 5 says "use the
existing integration asset install pattern; do not invent a separate installer",
but the current installer cannot register event keys other than SessionStart. The
merge logic must be extended — this is more than "add a file".

## What the plan got right

- Hook -> daemon delivery over the Unix socket with fire-and-forget `exit 0`
  when the daemon is down mirrors the existing `pohunek-agent-state.sh`
  (python3 + AF_UNIX). `POHUNEK_SOCKET_PATH` is inherited because the agent runs
  in a pohunek-launched PTY.
- jsonl append-only store with replay matches existing `store/mod.rs` and
  `events/mod.rs`.
- Removing/deduping the existing `push_blocked_effects` OS notification path
  (Task 8) — that path really exists (`gui-core/lib.rs:1917`) and is correctly
  identified.
- Not deriving `turn_completed` from PTY idleness and leaving it to hooks — the
  screen detector cannot produce `turn_completed`.

## Recommended changes before implementation

1. Task 5: Codex `PermissionRequest`/`Stop` via `config.toml [hooks]` (not
   `notify`); Claude mapping per-matcher (`permission_prompt`->approval,
   `agent_needs_input`/`idle_prompt`->blocked, `Stop`->turn_completed). Add the
   Codex/Claude version requirement.
2. Add explicit cross-producer deduplication across Tasks 4 + 5 (risk A).
3. Task 4: verify `session_stopped` carries failure/exit info; otherwise move
   `error` to hook-only.
4. Task 5: account for extending the installer with new hook event keys (risk C).
5. Task 7: re-scope `--all-hosts` — it is new CLI fan-out infrastructure, not a
   bullet.

## Sources

- Codex Hooks — https://developers.openai.com/codex/hooks
- Codex Advanced Configuration (notify) — https://developers.openai.com/codex/config-advanced
- openai/codex#11808 (notify only fires on turn completion) — https://github.com/openai/codex/issues/11808
- Claude Code Hooks reference — https://code.claude.com/docs/en/hooks.md

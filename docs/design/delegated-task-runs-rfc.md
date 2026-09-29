# RFC: Delegated Task Runs

- **Status:** Accepted 2026-09-29 (epic #182; implementation tracked by its
  sub-issues). Revised after design review, a Manage-Execute-Audit
  comparison, a code-grounded review on 2026-09-25 and four review rounds on
  2026-09-28.
- **Date:** 2026-09-24
- **Scope:** A task layer over ordinary sessions that lets an orchestrating agent
  delegate bounded coding work to another agent on any owned host and receive a
  settled, attributable, compact result in one blocking call. Includes the
  OpenCode 2.x agent as the first provider with structured turn evidence.
- **Audience:** maintainers of `pohunekd`, `pohunek-sessiond`, the protocol,
  the Rust/TypeScript clients, the CLI and the embedded agent skill

## 1. Summary

Pohunek already owns everything a delegation workflow needs underneath: durable
PTY sessions, worktree-per-session isolation, project resolution, diffs,
notifications, per-host agent profiles and multi-host targeting. What it does
not have is a way for *another agent* to hand over a task and get back a
trustworthy answer cheaply. Today an orchestrator has to send input, wait at
most 8 seconds at a time, re-read the screen, repeat until a predicate settles,
and then interpret terminal text. Every one of those steps is a paid model turn
for the orchestrator, and the outcome is still only a non-causal observation of
the current screen.

This RFC adds **tasks**: a logical record bound to one ordinary session, whose
prompts are **turns** with their own identity. A turn settles on evidence that
is causally attributed to that turn, and its **result** is a bounded, typed
document: outcome, the agent's final message, the diff against the task base,
configured check results, and — when the provider reports them — commands and
token/cost usage. `task.wait` blocks server-side until settlement or a
configured deadline, so a delegation costs the orchestrator one call to start
and one call to collect.

Nothing about the runtime model changes. A task's session is a normal PTY
session: it can be attached, observed, stopped, resumed and forked like any
other, and a human can take over at any point.

## 2. Motivation

### 2.1 Measured cost of polling-based delegation

A controlled benchmark on 2026-09-24 compared Claude Code working alone (arm A)
with Claude Code orchestrating an external delegation server that runs a cheap
model through OpenCode (arm B): 4 tasks x 2 arms x 3 repetitions, hidden
acceptance tests, identical permissions.

| | Arm A (alone) | Arm B (delegating) |
| --- | --- | --- |
| Quality | 12/12 | 12/12 |
| Total cost | $4.79 | $9.59 (worker model: $0.45) |
| Wall time | 9.6 min | 96 min |
| Orchestrator turns | 124 | 617 |
| Cached prompt tokens re-read | 3.4 M | 16.4 M |

The delegated model was nearly free. The loss came from the orchestrator: 255
status polls and 248 sleeps across 12 runs. Each is a full model request over
the whole cached context. Within arm B the orchestrator's cost grew by roughly
$0.01 per additional turn. The lesson for any delegation surface is direct:
**the number of orchestrator turns per task decides whether delegation pays,
not the price of the worker model.**

### 2.2 Pohunek's current orchestration surface

The embedded agent skill (`crates/cli/src/commands/agent_skill/SKILL.md`,
"Sending prompts and waiting") prescribes, for one prompt:

1. `session read` to capture runtime identity and terminal revision;
2. `session input --stdin`;
3. `session wait --after-terminal-watermark ...` (at most 8 s,
   `MAX_SESSION_WAIT_MS`, `crates/protocol/src/limits.rs:62`);
4. `session screen` to confirm the prompt was consumed;
5. repeated `session wait --activity idle --activity blocked --state ...`,
   each capped at 8 s, until one matches;
6. `session screen` / `session output` / `session diff` to learn what happened.

A five-minute agent turn therefore costs at least ~40 orchestrator turns of
waiting alone. The skill itself states the deeper problem: "no input-scoped
activity cursor exists that would make [a predicate] causal for one prompt". An
`idle` observation may belong to a repaint, to a previous prompt, or to a
different turn entirely.

### 2.3 What the delegation server gets right

The external server benchmarked above has properties worth adopting: explicit
run identity, attributable per-iteration results, before/after repository
snapshots with drift detection, compact byte-budgeted results, recorded
external acceptance, and token/cost metrics. Its weaknesses are also
instructive: it relies on a separate non-interactive process per run (no
durability, no attach, no human takeover) and it made the orchestrator poll.

Pohunek can provide the first set on top of its durable PTY model and remove
the polling entirely.

## 3. Relationship to the Existing Architecture

- **PTY/TUI-first is preserved.** A task never runs an agent outside a
  worker-owned PTY. Structured provider evidence (hooks, plugins, provider
  servers) is *observation*, exactly like the existing Claude/Codex hooks; it
  never replaces the terminal or re-renders it.
- **Sessions stay the unit of runtime.** A task references exactly one session
  for its lifetime. Session lifecycle, recovery, retention and attach are
  unchanged.
- **Worktree-per-session is the isolation primitive.** A task on a project
  defaults to a dedicated worktree on a generated branch `tasks/<task-id>`
  off the project's default base (the id makes collisions impossible;
  `branch` and `base` override it; the worktree follows ordinary retention).
  `session.new` without `branch` runs in place, so `task.start` never
  forwards an empty branch; in-place tasks require the explicit `in_place`
  flag, matching the skill's existing consent rule. Worktree paths are keyed by
  session id (`crates/daemon/src/worktree/mod.rs:230-245`) and Git refuses to
  check one branch out twice, so a new session never reaches another session's
  worktree by naming the same branch. Tasks that must share a tree use the
  explicit `worktree_of` handoff (section 8.7).
- **Project actions and worktree hooks are reused** for task launch recipes and
  for checks, including the name/path safety and in-repo trust rules of
  `docs/design/per-project-actions-and-worktree-hooks.md` (A.2.1, A.5, B.3).
- **The daemon remains the authority.** Clients, the CLI, the skill and any
  adapter (including the optional MCP adapter in section 13) only call the
  public protocol.
- **Orchestration composes on top.** Manager/auditor loops and any unattended
  delegation "dark factory" are clients of the public protocol (section 16);
  the daemon and the relay never run them or hold their task state.
- **Direct owner transport is unchanged.** Tasks target hosts exactly like
  sessions (`--host`, `<host>/<task-id>`). The future relay routes `task.*` like
  `session.*` under its own ACLs; nothing here depends on it.
- **Isolation claims stay honest.** Read-only investigation profiles (section
  11) restrict provider tools; they are not a hostile-workload sandbox. That
  boundary remains owned by #88.

## 4. Goals

1. One blocking protocol call waits for a turn to settle, bounded by daemon
   configuration rather than a fixed 8-second cap.
2. Turn settlement is **causal**: attributed to the specific input that opened
   the turn, never inferred from a screen state that predates it.
3. A settled turn yields a **typed, bounded result** that an orchestrator can
   act on without reading terminal text.
4. Results carry repository evidence: base, before/after state, diff summary,
   drift since the previous turn.
5. Configured checks run in the task worktree after settlement, with results in
   the task result, so the orchestrator does not need its own test turns.
6. Follow-up feedback continues the same provider conversation in the same
   session and worktree.
7. Blocked turns (approval, question) settle as `attention` with the question
   text, instead of hanging until a timeout.
8. Token and cost usage is recorded per turn when the provider reports it and
   is `unknown` otherwise — never estimated.
9. OpenCode 2.x becomes a first-class agent with structured turn evidence.
10. Orchestrators need at most two calls per turn in the common path: start or
    continue, then wait.

## 5. Non-goals

- A non-interactive runtime path. Tasks run in PTY sessions; there is no
  "headless run" mode.
- Automatic commit, push, merge, pull-request creation or worktree cleanup.
- Automatic approval of provider permission prompts on the owner's behalf.
- Model or profile routing, experiments and A/B assignment.
- A hostile-workload sandbox (owned by #88).
- Replacing `session.wait`, `session.screen` or `session.output`; they remain
  the low-level observation API.
- A daemon-side MCP server.
- A manager/auditor orchestration loop inside the daemon or the relay runtime.
  Orchestration is a client-side composition (section 16).

## 6. Terms

### 6.1 Task

A logical, daemon-owned record (`t-<ulid>`) that binds one session, its
project/worktree context, a base revision, an agent profile and an ordered list
of turns. A task is created by `task.start` and never re-targets another
session; every `task.start` creates its own session, so a session belongs to
at most one task. Its session may be recovered or resumed (new runtime generations);
the task follows the session.

A task has a coarse lifecycle state: `active` while its session is not in a
terminal lifecycle state, `ended` once the session is stopped, exited or
removed (including through `task.stop`). `ended` is final: resuming the
session later does not revive the task, and continuing the work is a new
`task.start` with `worktree_of` (section 8.7). Admission
controls that count concurrent tasks, such as relay delegation budgets, count
`active` tasks.

A settled turn — even `completed` with a published result and a recorded
review — does **not** end a task: its session keeps running until someone
stops it. `task.stop` on a task without an open turn is the explicit
"done with this task" step: it stops the session, marks the task `ended`,
and keeps the worktree, the task metadata and the result content (subject to
section 14 retention). Orchestrators that start one task per round must stop
each finished task, or they accumulate live sessions.

### 6.2 Turn

One prompt delivered to the task's session plus everything the agent does in
response, up to settlement. A turn has a daemon-assigned `turn_id`
(`<task-id>/<n>`), a **turn cursor** recorded at delivery, and a **consumption
milestone** recorded when the agent demonstrably takes the prompt up (section
8.1):

- `runtime_id`, `runtime_generation`;
- the terminal watermark and output offset immediately before the input was
  written;
- the provider turn reference, when the provider exposes one (section 9).

### 6.3 Settlement

The moment the daemon decides a turn has ended, based on **turn evidence**
observed strictly after the turn cursor. Settlement produces exactly one
**outcome**:

| Outcome | Meaning |
| --- | --- |
| `completed` | The agent finished the turn and returned to its ready state. |
| `attention` | The agent is blocked on an approval or a question addressed to the owner. |
| `failed` | The provider reported a turn failure, or the agent process exited with failure. |
| `exited` | The agent process exited successfully (for example the agent's own quit command) before the turn settled. |
| `lost` | The runtime generation was lost before settlement. |
| `stopped` | The session was stopped or removed by an explicit request. |
| `timed_out` | The turn exceeded the configured turn deadline (section 8.4). |
| `cancelled` | A queued turn was never delivered because the previous turn re-opened or the task was stopped (section 8.2). |

Settlement is revisable only in five defined ways, every transition is
recorded, and each revision increments the turn's `settlement_revision`
(starting at 1):

- an `attention` turn **resumes** after its pending attention is resolved —
  by `task.answer`, by a human answering directly in the terminal (section
  8.6), or by the provider resolving it itself (section 8.5): the same turn
  re-opens and its settlement is re-evaluated;
- a `timed_out` turn is **re-opened** by `task.extend` while its work has not
  visibly ended: the deadline moves within the ceiling and settlement is
  re-evaluated;
- a `completed` turn whose finality was **heuristic** (section 8.2) is
  **re-opened** when the same provider turn demonstrably continues after
  settlement. This revision is not requested by anyone; it corrects a
  settlement the daemon could not prove final. If the turn is still
  `finalizing`, its checks are cancelled and joined first (section 12) and
  the unpublished result is recorded as superseded with its checks
  `interrupted`;
- a `timed_out` turn **completes late** when correlated turn-end evidence
  for it (the same `prompt_id`/`turn_id`, or an OpenCode execution end for
  the bound execution) arrives after the deadline and before any later turn
  was delivered. The turn is revised to `completed` (or `failed`) with the
  usual finality rules, snapshots and checks, so the work appears in its
  `turn_delta` instead of being lost. No `task.extend` is needed, and the
  open-time ceiling does not block it: completion evidence closes a turn,
  it does not keep one open. Uncorrelated late evidence is never applied;
  it only marks the next result `drift_since_previous_turn`;
- an `attention` turn is **stopped** by `task.stop`: the pending attention
  is cancelled, the turn is revised to `stopped` as a new
  `settlement_revision`, the `attention` result is superseded, and any later
  `task.answer` fails `task_attention_stale`. Together with an answer and a
  terminal resolution this is the only way out of a settled `attention`, and
  it is atomic with the session stop.

A resumed or re-opened turn receives a fresh deadline window, but the total
open time of one turn across all its windows is bounded by
`tasks.turn_open_ceiling_ms`. Time spent settled as `attention` or
`timed_out` does not count as open time. Time between a heuristic
`completed` settlement and its re-open **does** count, because the agent was
in fact working throughout it. Once the ceiling is reached,
`task.extend` is refused with `task_turn_ceiling_reached` and the turn can
only be answered (if `attention`), stopped, or followed by a new turn once
the agent is ready. No sequence of answers and extensions keeps a turn open
indefinitely.

`failed`, `exited`, `lost`, `stopped` and `cancelled` are final; `completed` is final when
its finality is `provider_confirmed` and final unless re-opened when it is
`heuristic` (section 8.2). Continuing after `completed` or `failed` starts a
new turn; after `lost`, a new turn
follows explicit session recovery; after `exited` and `stopped` the task is
`ended` and further work is a new task (section 8.7). After `cancelled`
(a queued turn that was never delivered, section 8.2) it depends on the
reason: with `task_turn_reopened` the previous turn is open again, so the
caller waits on it and continues only after it settles; with
`task_stopped` or `session_ended` the task is `ended`.

### 6.4 Result

The bounded document produced at settlement (section 10). Its identity is
`result_id = (task_id, turn, settlement_revision)`; the document for one
`result_id` is immutable. A later revision of the same turn produces a new
result and marks the earlier one `superseded`; it never edits it.
`task.result` and `task.review` address results by this identity (section
13.1).

### 6.5 Check

A named, owner-configured command run in the task worktree after a turn
settles with `completed` (section 12).

## 7. Required Invariants

1. A turn settles only on evidence observed after its **consumption
   milestone** in the same runtime generation — never merely after the delivery
   cursor — or on evidence carrying the turn's provider reference. Evidence
   between delivery and consumption (startup repaints, a previous turn's late
   completion) never settles a turn. A runtime generation change before
   settlement settles the turn as `lost`, never `completed`.
2. At most one turn per task is open or awaiting an answer. `task.continue` on
   a task with an open turn fails with `task_turn_open`; on a task whose latest
   turn settled `attention` and was not answered it fails with
   `task_attention_open`; while the agent is still visibly working on the
   previous prompt it fails with `task_agent_busy`.
3. Input delivery is idempotent. `task.start` is idempotent per
   `(caller_scope, client_request_id)` (the daemon keeps a request-id → task
   index for as long as the task's **metadata** is retained,
   `tasks.metadata_retention`, section 14, since no `task_id` exists yet at
   first call); `task.continue`, `task.answer` and `task.extend` are
   idempotent per `(task_id, caller_scope, client_request_id)`. The
   `caller_scope` is the typed origin of the **connection** the request
   arrived on — local Unix socket owner or direct overlay owner — so keys from
   different origins never collide. This is new daemon work, not an existing
   property: today the Unix and overlay accept loops call the same
   `serve_connection` without any peer information
   (`crates/daemon/src/api/mod.rs:268`, `:399`, `:426`), relay RFC section
   12.3 types origin on sessions rather than connections, and neither
   `session.new` nor `session.input` carries a request id
   (`crates/protocol/src/session.rs`). Workstreams 1 and 3 add a typed
   connection origin at accept time, the request-id and fingerprint index,
   and its persistence.
   **Relay-path mutations do not use `client_request_id` at all**: they use
   the accepted relay RFC's operation tickets (`operation.ticket.issue` /
   `operation.begin`, relay RFC section 12.5), whose ticket binds the target
   (host, enrollment, share and revision, method class) and whose
   daemon-computed HMAC fingerprint covers the complete payload, whose
   expiry floor forbids execution after compaction, and whose lost-input
   outcome is `input_outcome_unknown`, never a replay. The relay dark
   factory RFC binds its admission records to those tickets. A retried call with the same
   key returns the existing turn instead of writing input twice; an answer
   is therefore never applied twice. The daemon stores a fingerprint of the
   complete request (method, target, every parameter, payload digest) with
   each key; a reused key with a different fingerprint fails with
   `task_request_conflict` and executes nothing. Idempotency holds across daemon and
   worker restarts through the delivery commit protocol of section 8.8; a
   delivery whose write cannot be proven either way is reported as
   `delivery_uncertain`, never silently repeated.
4. `task.wait` never writes input and never changes state. It may be retried
   freely after a transport failure.
5. The result's repository evidence describes the whole worktree against the
   task base. Pre-existing changes are reported, never attributed to the agent.
6. Metrics are either complete for every provider message in the turn or
   `unknown`. Partial sums are never reported as totals.
7. No secret enters daemon or worker logs, task events, notifications, relay
   persistence or audit. Provider-reported text — the final message,
   `commands`, attention text and choices, `claim_mismatches` paths — and
   check logs are **owner-private session content** under the same rule, ACL
   and retention as scrollback (section 14): they may contain whatever the
   agent or a command printed, including credentials, so they live only in
   owner-only files, are served only on paths that may serve scrollback
   (`task.result`, `task.check_log`; owner clients and, on the relay path,
   holders of `session.terminal.observe`), are never copied into events,
   notifications, projections or audit, and retire with the session.
   Prompts are persisted only as a **keyed fingerprint** (HMAC under a
   daemon-local key, so a stored value is no dictionary oracle for short or
   predictable prompts) plus that same scrollback; the plain canonicalized
   digest a hook reports is compared in memory and never persisted, logged
   or audited.
8. The origin-session guard applies: an agent running inside session S cannot
   continue, answer, extend, stop or review a task whose session is S, and
   cannot start a task whose working directory is S's working directory
   (an in-place task in S's cwd, or `worktree_of` naming S's task).
9. A task never mutates Git state beyond what the agent itself does in its
   worktree. The daemon's own Git operations for evidence are read-only
   against the repository and worktree and use the same `GIT_*`-sanitized
   invocation rules as worktree management; worktree snapshots (section 10)
   are written only into a daemon-private shadow repository.
10. Any input written to the task's session during an open turn that the
    task layer did not deliver — attach input or `session.input` from any
    client, human or agent — marks the turn `steered` (section 8.6). A
    steered turn settles by the same rules, but its result is never presented
    as purely agent-attributable.
11. At most one task **occupies** a worktree at a time. A task occupies its
    worktree from the moment any of its work may write to the tree until that
    work has provably stopped and its result is published:
    - while a baseline check run is in progress (section 12);
    - while a turn is open, including between delivery and consumption;
    - while an attention is pending (the agent is paused mid-work);
    - while a turn is `timed_out` and the agent has not been observed back in
      its ready state (the prompt may still be executing);
    - while a turn is settled but its result is still `finalizing` (checks
      running, section 12);
    - while a heuristically `completed` turn is inside its re-open watch
      window (section 8.2).

    Every operation that delivers input or re-opens a turn — `task.start`
    (including with `worktree_of`), `task.continue`, `task.answer`,
    `task.extend` — acquires occupancy through one check-and-set under the
    task store lock, and fails with `task_worktree_busy` when another task
    holds it. Occupancy ends only when **all** of these hold: no process the
    daemon started for the task (baseline or finalization checks) is still
    alive — `task.stop` and session removal cancel and join them first
    (section 12); no turn is open, no attention is pending and no result is
    finalizing; the heuristic re-open window of the latest turn has elapsed;
    and either the session has ended or the agent is observed ready. Session
    end alone never releases occupancy while daemon-owned task processes
    still run in the tree. Rounds on a shared worktree are sequential by daemon
    enforcement, not by convention. A heuristic re-open (section 6.3) that
    finds the worktree occupied by another task marks both tasks' affected
    results `integrity: suspect` instead of waiting, because the provider
    turn is already running.

    Occupancy is also a **write fence** on the other user tasks of the tree.
    While task A occupies a worktree, the daemon refuses task-layer delivery
    to every other task sharing it (`task_worktree_busy`) and refuses
    terminal input into their sessions: `session.input` fails with
    `worktree_busy` and attach is admitted read-only (observation without
    terminal control), so no human or client can make an idle agent in
    another session write into the tree A is working in. The daemon cannot
    stop a provider from resuming on its own inside a non-occupying session;
    such a resumption is observed as a working transition there and marks
    A's current or next result `integrity: suspect` and the resuming task's
    next result `drift_since_previous_turn`, exactly like the heuristic
    re-open case above. Orchestrators remove the residual risk by stopping
    finished tasks before the next round (section 16.2).

## 8. Turns and Waiting

### 8.1 Delivery

`task.start` creates the session through the same path as `session.new`
(project/branch/base resolution, worktree creation, hooks), then delivers the
first prompt as turn 1. `task.continue` delivers feedback as the next turn.

Delivery reuses the agent's input rules (bracketed paste, submit delay, blocked
refusal). The turn cursor is captured under the same lock that serializes input
writes, immediately before the first byte is written, so no repaint between
capture and write can be misattributed.

Prompts are read from the request body only; the CLI reads them from stdin.

**Delivery is not consumption.** A prompt is consumed only when the agent
demonstrably takes it up, in this order of preference:

1. a provider turn-start event referencing it (OpenCode
   `session.execution.started`, section 9.3);
2. a provider `UserPromptSubmit` hook whose prompt text matches the delivered
   prompt's digest (Claude and Codex, section 9.2). The match binds the turn
   to the provider's own identifier for that prompt — Claude `prompt_id`,
   Codex `turn_id` — which every later hook of the same turn carries. The
   digest match makes the milestone causal without storing the prompt. The
   hook's `prompt` field is not byte-identical to what was written into the
   PTY (bracketed-paste markers, line-ending conversion, a trailing newline
   from the submit key, provider-side trimming), so both sides are digested
   after a **per-provider canonicalization** — the exact rules (for example
   strip paste markers, convert CRLF to LF, remove one trailing newline) are
   pinned per provider version in the compatibility lock with goldens, and a
   provider upgrade that changes them fails the compatibility gate instead of
   silently degrading every turn to uncorrelated;
3. (detection) the prompt reaching the agent's input and the agent acting on
   it.

Until then the turn is open and unarmed — no evidence settles it (invariant
1). This matters at startup (turn 1 may be written before the TUI initializes)
and after `timed_out` (the previous prompt may still be executing); in both
cases repaints and leftover completion evidence precede consumption and cannot
settle the new turn. When consumption is observed only through detection
(form 3) — for detection-only agents, and for hook-based agents whose
prompt-submit hook is missing or degraded — the observation is itself
heuristic (a paste echo plus the agent leaving its ready state), so residual
misattribution remains possible whatever source later settles the turn.
`settled_by` (section 8.2) marks those turns as uncorrelated, and the skill
(section 13.3) tells orchestrators to weigh them accordingly.

### 8.2 Turn evidence

Each agent declares its **turn evidence sources** in its adapter, in priority
order. The daemon settles on the highest-priority source that produces a
decisive signal after the cursor, under one **arbitration rule**: a
lower-priority source may settle a turn only while every higher-priority
source the adapter declares is `evidence_degraded` (hook not installed,
plugin missing, provider server unreachable, journal gap). While a higher
source is healthy, a lower signal — a detection `idle` before the `Stop`
arrives, a hook before the OpenCode execution event — is recorded but does
not settle; the turn waits for the higher source or runs to `timed_out`.
Lower evidence therefore never publishes a weaker result ahead of the
authoritative one, and `settled_by` names the source that decided:

| Priority | Source | Examples |
| --- | --- | --- |
| 1 | Provider structured events | OpenCode server events (section 9.3) |
| 2 | Provider hooks/plugins reporting turn boundaries | Claude `Stop`/`StopFailure`, Codex `Stop`, Hermes plugin hooks |
| 3 | Detection transitions | `working` then `idle`/`blocked` after the cursor, per manifest |

Detection-only settlement (priority 3) must observe **both** a `working` state
and a subsequent settled state, both after the turn's consumption milestone.
An `idle` screen that was never preceded by `working` after consumption does
not settle a turn; if nothing changes before the turn deadline, the outcome is
`timed_out`. This closes the "predicate already matched" gap described in the
current skill and the two delivery-side gaps named in section 8.1.

Detection runs in the daemon, fed by the worker's output frames
(`crates/daemon/src/session/detector.rs`). Detection evidence is therefore
keyed by **output offset**, not by wall-clock time: a transition counts for a
turn only if the bytes that produced it lie beyond the turn's consumption
offset, and the same offset range never produces a transition twice. After a
daemon reconnect the worker re-sends retained output as `Replay` frames, which
the daemon today forwards to the detector like live output
(`crates/daemon/src/session/target.rs:944-947`), so the detector would see
old bytes with a fresh timestamp. Transitions derived from replayed bytes
are marked `reconstructed`: they may confirm a transition already recorded
for that offset, but they never create a first post-consumption `working`
and never settle a turn on their own. Hook and provider evidence arrives on
the worker socket, not in the output stream, so the worker stamps every
evidence record with the session's current `next_output_offset` at the
moment it receives the record (section 9.2). The consumption milestone from
a `UserPromptSubmit` therefore has an output offset, and after a reconnect
the daemon can decide exactly which replayed or live detection transitions
lie beyond it. A turn whose only evidence of an
outage interval is reconstructed settles by hook or provider evidence from
the worker journal (section 9.2) or runs to `timed_out` with
`evidence_degraded`.

Hook-based turn-end events (priority 2) carry the provider's identifier of
the prompt that opened the turn (Claude `prompt_id`, Codex `turn_id`,
section 9.2), but they are not always final. Two rules therefore apply:

- **Correlation.** A turn-end hook settles a turn only when its provider
  identifier equals the one bound at the turn's consumption milestone
  (section 8.1 form 2). A `Stop` carrying any other identifier is never
  applied to the open turn; if it matches a `timed_out` predecessor it
  completes that turn late (section 6.3), otherwise it is discarded. Such
  settlements are `provider_hooks_correlated`.
  When the milestone came from detection or the payload lacks the
  identifier (a provider version below the pinned minimum, a degraded
  hook), the hook settles only after the consumption milestone and the
  settlement is `provider_hooks_uncorrelated`.
- **Finality.** A `Stop` event is not proof that the provider turn ended.
  Claude runs all matching hooks in parallel and lets any `Stop` hook block
  the stop (exit code 2, a `block` decision, or `additionalContext`), after
  which Claude continues and the next `Stop` arrives with
  `stop_hook_active: true`; Codex exposes the same flag. Pohunek's own hook
  can report its `Stop` while another owner-installed hook is still
  running, and that hook may block after any fixed delay. A Claude `Stop`
  with non-empty `background_tasks` means the session is paused waiting for
  background work, not done. Neither Claude nor Codex emits an event after
  hook evaluation that confirms the turn really ended.

  Hook-based completion is therefore settled with `finality: heuristic`:
  it requires the matching `Stop`, no pending `background_tasks`, the
  agent's detection state back at ready, and no working transition, further
  same-identifier `Stop` or new prompt-submit event within
  `tasks.stop_settle_grace_ms` (daemon configuration, validated at
  startup). The grace window reduces premature settlement; it does not
  prevent it. What makes the design safe is the consequence rule: for
  `tasks.heuristic_reopen_window_ms` after settlement (at least the grace
  window, validated at startup), any working transition or same-identifier
  hook event re-opens the turn as a new `settlement_revision` (section 6.3),
  supersedes the published result, marks reviews of the old revision stale
  (section 13.1), and keeps worktree occupancy (invariant 11) so no other
  round starts on the tree during that window. The window also holds back
  the **same** task: the new turn's own working transition would otherwise
  be indistinguishable from the old turn continuing and would re-open it,
  violating invariant 2. A `task.continue` during the window is therefore
  **accepted and queued**, not refused, under these rules:

  - The call returns immediately with `{ queued: true, turn: n+1,
    deliver_after }`; it never blocks until the window closes, so a relay
    operation ticket is not held `in_progress` for the window
    (relay dark factory RFC section 8.3). The daemon persists the queued
    turn with its number, idempotency key, fingerprint and delivery id
    (section 8.8); turn numbers are assigned at queueing.
  - At most **one** turn may be queued per task. A second `task.continue`
    while one is queued fails with `task_turn_open`, exactly as while a turn
    is open (invariant 2).
  - A queued turn is `open` for every other purpose: `task.wait` on it waits
    as on an open turn, `task.answer` sees no attention on it, and
    `task.inspect` reports it with phase `queued` and its current
    `deliver_after`. `task.extend` on a queued turn is refused with
    `task_turn_queued`: it has no deadline to extend until it is delivered,
    and its deadline starts at delivery.
  - `deliver_after` is not a fixed time: delivery follows the window. If
    the window restarts (for example after a daemon outage, below), the
    queued turn waits for the restarted window and `task.inspect` reports
    the new time.
  - If the session ends or its runtime generation is lost before delivery,
    the queued turn settles `cancelled` with reason `session_ended` (it
    never wrote anything, so it is not `lost`); an ended session also ends
    the task.
  - When the window closes without a re-open, the daemon delivers the
    queued turn through the normal occupancy and delivery path.
  - If the old turn re-opens instead, the queued turn is cancelled without
    delivery and settles with outcome `cancelled` and reason
    `task_turn_reopened`; `task.wait` on it returns that result. `task.stop`
    during the window cancels a queued turn the same way (reason
    `task_stopped`), and so does the session ending (reason
    `session_ended`). A cancelled queued turn never wrote anything, so
    feedback is never written into a turn the orchestrator did not see. Re-open triggers are attributed to the old
  provider turn only: a same-identifier hook event, or a working transition
  while no new task-layer delivery has happened. Work that resumes after
  the window is steering-like activity without an open turn: it marks the
  task's next result `drift_since_previous_turn` and `integrity: suspect`.
  Both windows are measured by the **daemon**, on its monotonic clock, from
  the daemon's receipt of the evidence; only their start differs from the
  worker's receipt when evidence arrives late through the journal. The
  windows bound time; which detection transitions count inside them is
  still decided by output offset (above). They assume the daemon was
  observing throughout. If the daemon was disconnected from the worker at any point
  between the `Stop` and the end of the re-open window, detection for that
  interval is only `reconstructed` and could not have triggered a re-open.
  A heuristic completion whose grace or re-open window overlaps such an
  outage is therefore not settled from stale timing: the daemon restarts
  both windows at its own time of reconnect completion, and the result
  carries `evidence_degraded: true` with reason `window_overlapped_outage`
  so an orchestrator can see why finality was delayed.
  Heuristic completion is provisional until the window closes; `task.wait`
  with `until: "final"` (the default, section 8.3) waits through it, so an
  orchestrator never needs an extra call to learn whether a result
  survived.

  Provider-event completion (OpenCode `session.execution.succeeded` for the
  bound execution) is `finality: provider_confirmed`, because the provider
  itself reports the end of the execution; no re-open window applies.
  Subagent lifecycle events (`SubagentStop`) never settle a turn.

Every settlement records which source decided it (`settled_by`:
`provider_events`, `provider_hooks_correlated`, `provider_hooks_uncorrelated`
or `detection`), so an orchestrator can weigh an outcome accordingly.

### 8.3 `task.wait`

`task.wait { task_id, turn?, timeout_ms, until? }` blocks on a dedicated
connection until the named turn (default: the latest) reaches the requested
point, or until `timeout_ms` elapses. A turn passes through the phases `open`
(including pending attention), `finalizing` (settled, checks still running,
section 12), `published` and — for `finality: heuristic` results — `final`
once the re-open window has closed without a re-open (`provider_confirmed`
and non-`completed` results are final at publication). `until` is `"final"`
(the default) or `"published"`. An attention, a `timed_out` settlement or
any other outcome that needs the caller's action returns immediately in
either mode; waiting through a window only applies to a heuristic
`completed`. If the turn re-opens during the window, the wait simply keeps
going and returns the next revision. `task.wait` returns either a result
(section 10) or `{ reason: "timeout", turn, phase, progress }`, where
`progress` is a compact summary of the current phase (elapsed time, current
activity, provider phase when known, checks done and remaining while
finalizing, re-open window close time).

`timeout_ms` is bounded by the daemon configuration key
`tasks.max_wait_ms`, validated at startup. The shipped default configuration
sets it to a few minutes; a request above the ceiling fails validation rather
than being clamped silently. An omitted `timeout_ms` defaults to
`min(tasks.max_wait_ms, time until the current phase's deadline)`: the turn
deadline while `open`, the publication deadline (`tasks.finalize_max_ms`,
section 12) while `finalizing`, and the window close time while `published`
waiting for `final`.

The ceiling is deliberately modest. A turn that outlives it is waited out by
repeated `task.wait` calls, and Goal 10 counts **orchestrator tool calls, not
protocol waits**: the CLI `--wait` flag (section 13.2) is one process that
delivers the turn and then re-issues `task.wait` internally until the result
is final (or needs the caller's action), its own deadline passes, or the
turn times out. Reaching the turn deadline does not end a CLI wait while the
turn is `finalizing` or inside its re-open window; both have their own
bounded deadlines. A five-minute or thirty-minute turn therefore still costs
the orchestrator one call, for heuristic-finality agents (Claude, Codex) as
well.

Waiting uses the dedicated-connection model of `session.wait` and
`session.output`, so it does not block the control connection or other
clients, but it does **not** share their waiter slots. The session waiter
limits (`observation_global_waiters`, `observation_session_waiters`,
`crates/daemon/src/session/observation.rs:461-491`) are sized for 8-second
waits; multi-minute task waits would starve them. Task waits therefore have
their own pool, bounded by `tasks.max_waiters` and
`tasks.max_waiters_per_task`, and exhaustion fails with
`task_waiter_limit_reached`. The 8-second `session.wait` cap exists because
an abandoned connection holds its slot until the wait expires; task waiters
instead watch their dedicated socket for peer hangup (EOF/`POLLHUP`) and
release the slot as soon as the client goes away. Hangup is only prompt on
the local Unix socket; over TCP (the NetBird overlay) a vanished peer sends
nothing, and without traffic the kernel notices only after its retransmission
or keepalive timeouts. The control protocol is one request line and one
response line, and the clients decode the first line as the final response,
so the daemon sends no application-level heartbeat. Every overlay wait
connection instead sets per-socket TCP keepalive
(`tasks.wait_keepalive_idle_ms`, `tasks.wait_keepalive_interval_ms`,
`tasks.wait_keepalive_count`; supported on Linux and macOS), and a keepalive
failure or a peer EOF releases the slot. Slot lifetime is thus bounded by
the client's liveness plus the configured keepalive detection time, not by
`tasks.max_wait_ms`.

A wait holds no state beyond its slot: a client that disconnects simply
calls `task.wait` again (invariant 4). A daemon restart drops waiters;
settlement state is persisted, so the next `task.wait` returns immediately if
the turn settled meanwhile.

### 8.4 Turn deadline

`tasks.turn_deadline` (daemon configuration, overridable per agent profile and
per request up to the configured ceiling) bounds how long a turn may stay open
without settlement. On expiry the turn settles as `timed_out`; the session and
agent are **not** stopped. `task.extend` re-opens a `timed_out` turn whose work
has not visibly ended (section 6.3), within `tasks.turn_open_ceiling_ms`, so
the orchestrator can wait again;
otherwise it sends feedback once the agent is ready — invariant 2 refuses
delivery with `task_agent_busy` while the previous prompt is still executing —
or stops the session.

### 8.5 Attention

**Decision requested is not waiting.** A provider decision request (Claude
and Codex `PermissionRequest` hooks) fires before the provider decides how
to handle the request, and another hook, a permission rule or the permission
mode may allow or deny it without asking anyone. The daemon therefore
records a request first as a **pending decision** and settles `attention`
only once waiting is **confirmed**:

- by a provider signal that the request is now addressed to the user —
  OpenCode `permission.asked` and `form.created`; Claude `Notification` with
  matcher `permission_prompt` or `elicitation_dialog` carrying the same
  `prompt_id`;
- or, when the provider has no such signal (Codex), by a detection `blocked`
  transition after the pending decision with no tool progress within
  `tasks.attention_confirm_ms`.

A pending decision that resolves on its own — the tool runs (`PostToolUse`,
`session.tool.*`), OpenCode `permission.replied`, or the agent returns to
`working` — is discarded without ever becoming an attention. If an attention
was already settled and the provider resolves it itself (another hook, a
timeout inside the provider, a human in the terminal), the attention is
marked `resolved_elsewhere`, the turn resumes as a new settlement revision
(section 6.3), and any later answer to it is refused as stale.

**Attention identity.** Each confirmed attention has a daemon-assigned
`attention_id` (`<turn_id>/a<n>`, unique within the turn) and the turn's
current `settlement_revision`. One turn can pass through several attention
cycles; each gets a new id. The attention object carries kind (`approval`,
`question`), the provider's question or permission text, the choices the
provider offers, `attention_id`, `settlement_revision`, and the provider
reference (permission request id, form id, `prompt_id`/`turn_id`) used to
check that it is still current.

The daemon never answers an attention. The owner (or a caller authorized
under section 11.3) answers through `task.answer { task_id, turn,
attention_id, settlement_revision, answer, client_request_id,
allow_unverified_delivery? }`. Under the task store lock and the session's
input lock, the daemon first checks that the turn's current pending
attention is exactly that `attention_id` at that `settlement_revision`; a
mismatch fails with `task_attention_stale` and writes nothing. Those locks
cover only the daemon's view, not the provider's state: the provider can
resolve question A on its own and move to question B before the daemon has
processed the events. What happens next depends on how the answer reaches
the provider:

- **Addressed answers (`verification: provider_addressed`).** Where the
  provider accepts an answer addressed to a specific request — OpenCode's
  permission reply and form reply APIs, keyed by the provider's request or
  form id — the daemon sends the answer to that id. The provider itself
  rejects it if the request is no longer pending, and that rejection is
  returned as `task_attention_stale`. This is the only path on which a stale
  answer is **guaranteed** never to reach another question.
- **Keystroke answers (`verification: unverified`).** Claude, Codex and
  detection-only agents are answered by writing keys into the PTY. No
  provider-side check ties those keys to request A; if the provider has
  already moved to B, they answer B. The daemon cannot close this window, so
  it does not pretend to: a keystroke answer is refused with
  `task_answer_unverifiable` unless the caller sets
  `allow_unverified_delivery: true`. With the flag the daemon still performs
  the local check above, then writes the keys; the result and audit record
  `answer_verification: unverified`, and the turn's result is marked
  `steered`-equivalent for attribution (`answered_unverified: true`).
  Orchestrators and relay grants decide whether that weaker guarantee is
  acceptable; the default refuses it.

On success the same turn re-opens. `task.answer` is
idempotent per `(task_id, caller_scope, client_request_id)` (invariant 3).
`task.continue` on an unanswered `attention` turn is refused with
`task_attention_open` (invariant 2), so a question is never silently buried
under feedback.

A degraded attention (`text: null`, section 9.2) has no provider choices to
answer with. `task.answer` on it accepts only the generic `approve` and
`deny` answers, and only when the agent's detection manifest declares the
input sequence for each; otherwise it fails with `task_answer_unsupported`
and the attention is resolved by terminal takeover (section 8.6) or
`task.stop`. The daemon never guesses keystrokes for an approval it cannot
read.

### 8.6 Human takeover

A human may attach to the task's session at any time. Input the task layer
did not deliver — attach input or `session.input` from any client — during
an open turn marks it **steered** (invariant 10); settlement rules do not change,
but the result records the steering and is never presented as purely
agent-attributable. When a human answers a provider approval or question
directly in the terminal instead of through `task.answer`, the attention is
marked `resolved_elsewhere` and the resumed observation re-evaluates the turn
exactly as a `task.answer` resume does (section 6.3); later `task.answer`
calls for that attention fail with `task_attention_stale`, and only
`task.answer` records the answered choice as data.
`task.answer` stays policy-gated (section 11.3) regardless of takeover.

### 8.7 Shared worktrees (`worktree_of`)

`task.start { worktree_of: <task-id> }` starts a new task — a new session
with a fresh agent context — whose working directory is the worktree of
another task on the same host, instead of creating a worktree. The named task
may be `active` or `ended`; what matters is that its worktree still exists
and is held (below). An `ended` owner task costs no session and no
concurrency slot, so the objective's tree can outlive every agent between
rounds. This is the
only way two tasks share a tree; naming the same branch does not do it
(section 3).

- The new task records the **owner task** (the task that created the
  worktree, following `worktree_of` chains to their root) and inherits its
  `base`. Repository evidence stays relative to that base (invariant 5), and
  each turn additionally reports its own delta (section 10).
- No worktree create or remove hooks run: the worktree already exists and
  its binding stays with the owner task's session.
- **Worktree users and retention holds.** The daemon records every task that
  uses a worktree (the owner task and each `worktree_of` task) in the task
  store. A worktree carries a `worktree_shared` retention hold while either
  any user task is `active`, or an explicit **retain hold** is set: `task.start
  { retain_worktree: true }` on the owner task, or `task.retain_worktree
  { task_id }` later on any task of the chain while the worktree still
  exists, sets it on the owner task, and it survives the
  owner task's end until `task.release_worktree { task_id }` clears it or
  `tasks.worktree_hold_max_age` passes. While a retain hold is set, the
  owner task's **metadata** is exempt from `tasks.metadata_retention` (its
  content still follows session retention), so `worktree_of` never names a
  task whose record has expired; the exemption ends with the hold. Orchestrators set it on the first
  round so rounds can stop every task in between (section 16.2) and release
  it when the objective ends. While held, the retention sweep never removes
  the owner session or its worktree. The existing sweep already keeps
  worktrees with uncommitted changes, untracked files or commits no other
  ref contains (`crates/daemon/src/worktree/mod.rs:1218-1245`, applied from
  `crates/daemon/src/session/retention.rs:279`); the new hold extends that to
  clean, fully pushed trees that a later round still needs. An expired
  retain hold only returns the worktree to those ordinary rules; it never
  deletes work. Explicit removal of the
  owner session (`session rm`) while other user tasks are active is refused
  with `worktree_in_use`. Cascading removal is explicit and authorized
  per affected session:
  - `session.remove { stop_worktree_users: true, expected_worktree_users:
    [task_id...] }` must name the **exact** current set of active user
    tasks. Under the task store lock the daemon compares it with the
    recorded set and, on any difference, fails with
    `worktree_users_changed` and changes nothing. From that check until
    the removal finishes the worktree is marked `removal_pending`, and any
    `task.start { worktree_of }` into it fails with `task_worktree_busy`, so
    the set cannot grow underneath the cascade.
  - Authority must cover **every** stopped session, not only the owner.
    Local owner clients have it over all sessions. On the relay path the
    relay authorizes `session.lifecycle.control` on each named task's
    session and `session.remove` on the owner session before forwarding
    (relay dark factory RFC section 7.1), and the daemon additionally
    requires every named session to share the caller's `HostShareId`; a
    user task outside that share makes the cascade fail with
    `worktree_in_use` (the owner path must resolve it).
  - `worktree_in_use` reveals only what the caller may see: the relay
    filters the returned list to tasks on which the caller holds
    `task.metadata.read` and reports the rest as a count, with the same
    shape whether hidden tasks exist or not beyond that count (relay RFC
    section 14 identical filtering).
  - The cascade stops each user task through the `task.stop` path,
    including its cancel-and-join barrier for daemon-owned check processes
    (section 12), then removes the owner session and worktree, so a running
    auditor is stopped explicitly instead of losing its tree mid-turn.

  Stopping the owner session without removing it keeps the worktree and the
  hold.
- Invariant 11 serializes work across all tasks sharing a worktree:
  `task.start` with `worktree_of` is refused with `task_worktree_busy` while
  another task occupies the worktree, and the occupancy check and the new
  task's creation are one atomic step under the task store lock.
- `worktree_of` combines with `mode: "investigate"` (section 11.2): this is
  how an auditor verifies the executor's uncommitted state. It is refused
  with `task_worktree_mode_conflict` together with `in_place` or `branch`.
- **Read access never becomes write access.** An executor-mode
  `task.start { worktree_of }` must name a task that is itself not in
  investigate mode; naming an investigate-mode task fails with
  `task_worktree_via_investigate`. Otherwise a caller allowed only to
  observe a tree could start an investigate task there, become its creator,
  and then use it to place a writing executor into a tree it never
  controlled. An investigate-mode start may name any member of the chain.
- `worktree_of` names a task, never a path; the daemon resolves the path from
  its own binding, so a caller cannot point a task at an arbitrary
  directory. Sharing a tree requires authority over the **named** task,
  subject to the rule above for executor starts: any local owner client
  has it; on the relay path the named task's session must share the
  caller's `HostShareId` (relay RFC section 12.3), and the relay dark
  factory RFC defines which relay grants the caller needs on it.

### 8.8 Delivery commit protocol

Writing a task record and writing bytes to a PTY are two different durable
effects in two processes; no single atomic write covers both. Delivery
(`task.start` turn 1, `task.continue`, `task.answer` through keystrokes)
therefore follows a persisted protocol keyed by a stable **delivery id**
(`<turn_id>/d<n>`), assigned once per logical operation and reused on every
retry of the same idempotency key:

1. **prepared** — the daemon durably records the operation, its delivery id
   and the keyed prompt fingerprint in the task store before contacting the
   worker.
2. **dispatched** — the daemon sends the input to the worker tagged with the
   delivery id.
3. The worker durably records `delivery_id: writing` in its runtime journal
   (`crates/session-worker/src/journal.rs`) before writing the first byte,
   and `delivery_id: written` after the last byte, then acknowledges.
   Invariant 2 allows one outstanding delivery per task, so the journal keeps
   only the latest delivery record per task, which stays within the
   journal's size bound.
4. **delivered** — the daemon records the acknowledgement; the turn cursor
   captured under the input lock (section 8.1) is part of the acknowledgement.

Task delivery ids live in the worker journal, not in the lease-scoped input
deduplication namespace, which is cleared whenever the namespace changes
(`crates/session-worker/src/input.rs:320-328`); they survive worker reconnects
and lease changes for the life of the runtime generation.

Recovery after a daemon or worker restart, or a lost acknowledgement, asks
the worker for the delivery id's journal state and resolves:

| Worker journal state | Resolution |
| --- | --- |
| absent (never reached step 3), daemon still holds the payload in memory (lost worker acknowledgement, worker reconnect) | Not written. Dispatch again with the same delivery id. |
| absent, daemon restarted since `prepared` | Not written, but the daemon no longer has the prompt: it stores only a digest (invariant 7). The delivery moves to **`awaiting_resubmission`**. It is never dispatched from stored state. |
| `written` | Written. Record `delivered`; never write again. |
| `writing` (crash mid-write) | **Uncertain**: part of the prompt may have reached the agent. The turn settles `failed` with `reason: delivery_uncertain`; nothing is re-sent, and the orchestrator decides after inspecting the session. |
| runtime generation changed | The previous PTY is gone. An unacknowledged delivery settles the turn `lost`; nothing is re-sent into the new generation. |

**Addressed answers.** An OpenCode answer (section 8.5) is a provider API
call rather than a PTY write, but it is the same kind of external side
effect and follows the same protocol with the journal states `sending`
(recorded before the request is issued, with the `attention_id` and the
provider request or form id) and `sent` (recorded with the provider's
response). Recovery: `absent` — not sent, issue the request; `sent` —
record `delivered` from the journaled response; `sending` — the worker asks
the provider for the request's state: already replied resolves to
`delivered` (the provider also rejects a duplicate reply for an id it has
answered, which resolves the same way), still pending re-issues the request,
gone resolves to `task_attention_stale`. `task.answer` therefore keeps
invariant 3 on both answer paths.

**Resubmission.** A delivery in `awaiting_resubmission` resumes only when
the client retries the same operation with the same idempotency key (local
path) or the same operation ticket (relay path, relay RFC section 12.5) **and
the full original payload**. The daemon checks the payload against the
stored keyed fingerprint (local path) or the ticket's HMAC fingerprint (relay path); a
mismatch fails with `task_payload_mismatch` and dispatches nothing. A read-
only lookup (`task.inspect` by request key, or `operation.result.get` on the
relay path) reports `awaiting_resubmission` so the client knows it must
resend rather than wait. The turn stays open and unarmed meanwhile; if no
valid resubmission arrives within `tasks.resubmit_window_ms`, the turn
settles `failed` with `reason: delivery_abandoned`, which ends the
operation for good — a later resubmission is refused as stale, never
executed. Automatic reconciliation therefore only ever **observes**
outcomes; re-sending a prompt always requires the client to present it
again.

A retried call with the same idempotency key follows the same resolution
instead of creating a new delivery. Tests cover a crash before dispatch,
after `writing`, after `written` before the acknowledgement, and across a
lease change.

On the relay path resubmission is an **amendment of the operation-ticket
state machine** of relay RFC section 12.5, which otherwise lets a repeated
`operation.begin` with the same ticket and fingerprint only return the
recorded result or `in_progress`: a ticket whose operation is
`awaiting_resubmission` accepts exactly one further `begin` with a matching
fingerprint, CASes from that state to `begun` and executes the delivery;
every later `begin` for the ticket returns the recorded result as before,
and an expired or abandoned ticket never re-enters `begun`. The relay dark
factory RFC lists this amendment (its section 3) and the accepted relay RFC
gains it together with a one-shot resubmission case in its ticket tests.

## 9. Provider Turn Evidence

### 9.1 Adapter contract

`AgentAdapter` gains a `turn_evidence()` declaration listing the sources the
agent supports and how each maps to settlement, attention, final message,
commands and metrics. A source the host cannot currently use (hook not
installed, plugin missing, provider server unreachable) is reported in
`task.inspect` and in the result as `evidence_degraded`, and settlement falls
back to the next source.

Each provider's evidence shapes are pinned in `compat/<agent>/` with sanitized
goldens, following the Hermes compatibility lock model, and are verified by an
`xtask` gate.

### 9.2 Existing agents

The managed integration already installs lifecycle hooks for Claude and
Codex (event constants at `crates/daemon/src/integration/mod.rs:88-104`,
registration in `inspect_claude_registration` at `:939-1009` and
`inspect_codex_registration` at `:1011-1055`). They reach the daemon on two
different paths today:

- the **notify** hooks (`Stop`, `StopFailure`, `Notification`,
  `PermissionRequest`) connect straight to the daemon socket and are
  fire-and-forget: on any socket failure they exit silently and the event is
  lost (`crates/daemon/src/integration/assets/claude/pohunek-agent-notify.sh`,
  header and `send_request`);
- the **state** hooks (`SessionStart`, subagent start/stop) prefer the
  session's **worker** socket (`POHUNEK_WORKER_SOCKET_PATH`,
  `crates/daemon/src/integration/assets/claude/pohunek-agent-state.sh:58`),
  which is up whenever the
  agent is.

Task evidence cannot use the notify path: while the daemon is restarting,
a `Stop` would vanish and the turn would end `timed_out` although it
completed. Task evidence therefore travels on the **state-hook path to the
worker**, which is a new use of that channel, not a reuse of the notify
path:

1. The managed task hooks (`UserPromptSubmit`, `Stop`, `StopFailure`,
   `PermissionRequest`, `Notification`) send a bounded, field-allowlisted
   evidence record to the worker socket, with the provider identity claim
   rules of the state hooks (`MAX_IDENTITY_CLAIM_TTL_SECS`). The
   prompt-submit hook hashes the canonicalized prompt locally (section 8.1)
   and sends only the digest and the provider identifier, so prompt text
   never crosses the hook channel.
2. The worker stamps each record with the current `next_output_offset`
   (section 8.2), appends it to a bounded **evidence journal** with a
   per-generation sequence number, and forwards it to the daemon. The daemon
   acknowledges a sequence number only after it has durably recorded the
   evidence record and every settlement change it caused (consumption
   milestone, pending decision, settlement revision) in the task store, so an
   acknowledgement is a commit point and a crash before it leaves the record
   in the worker journal for replay; replayed records are idempotent by
   `(runtime_generation, sequence)`, and a record the daemon already applied
   is acknowledged again without effect. The journal can briefly hold owner-private
   content such as `last_assistant_message`; it lives in the worker's
   owner-only runtime directory under the same posture as scrollback, never
   in logs, and each record is deleted from the journal once acknowledged. On reconnect the daemon asks for records after
   its last acknowledged sequence, so evidence emitted during a daemon outage
   is replayed exactly once, in order. If the journal overflowed during the
   outage, the replay reports the gap and affected turns are
   `evidence_degraded`.
3. The existing notify hooks keep feeding notifications to the daemon
   directly; the task kinds are deduplicated against them (section 13.1).
   Only the worker path is turn evidence.
4. Provider-server events from a secondary child (OpenCode, section 9.3)
   enter the **same** sequenced, offset-stamped journal, so replay after a
   daemon outage applies to them exactly as to hook records.

New managed hooks are `UserPromptSubmit` for Claude and Codex (consumption,
section 8.1) and `PermissionRequest` for Claude (attention). The Claude and
Codex hook configuration is installed per user, so these hooks run for
**every** Claude or Codex session on the host, not only task sessions. Outside
a pohunek session they exit immediately (no `POHUNEK_SESSION_ID`); inside a
non-task pohunek session the worker drops the record. The residual cost is
one hash of each prompt in pohunek sessions, which this RFC accepts
explicitly.

Verified provider payload facts this design relies on (2026-09-25; each is
pinned in the compatibility lock with its source):

- **Claude Code** (hooks reference, `code.claude.com/docs/en/hooks`; installed
  2.1.280): every hook input carries `prompt_id`, a UUID for the user prompt
  being processed (Claude Code 2.1.196 or later), so `UserPromptSubmit`,
  `Stop`, `PermissionRequest` and `Notification` of one turn share it.
  `UserPromptSubmit` carries `prompt`. `Stop` carries `stop_hook_active`,
  `last_assistant_message`, `background_tasks` and `session_crons`; a `Stop`
  hook can block the stop, and Claude ends the turn after 8 consecutive
  blocks. The documentation warns that the transcript is written
  asynchronously and may lack the current turn's final messages at hook time,
  and directs hooks to `last_assistant_message` instead. `PermissionRequest`
  carries `tool_name`, `tool_input` and optional `permission_suggestions`.
  `Notification` matchers include `permission_prompt` and
  `elicitation_dialog`.
- **Codex** (`codex-rs/hooks/src/schema.rs`; installed 0.156.0 registers
  `UserPromptSubmit`, `PermissionRequest` and `Stop`): every turn-scoped hook
  input carries `turn_id`. `UserPromptSubmit` carries `prompt`; `Stop` carries
  `stop_hook_active` and `last_assistant_message`.

The minimum provider versions for correlated settlement are the Claude and
Codex versions pinned in the lock; below them, settlement is
`provider_hooks_uncorrelated` (section 8.2).

| Agent | Consumption | Settlement | Attention | Final message | Commands | Metrics |
| --- | --- | --- | --- | --- | --- | --- |
| Claude Code | `UserPromptSubmit` (new managed hook), digest-matched, binds `prompt_id` | Managed `Stop` / `StopFailure` hooks with the bound `prompt_id` (section 8.2) | `PermissionRequest` (new managed hook) opens a pending decision (tool name, bounded tool input summary, suggestions); `Notification` `permission_prompt` / `elicitation_dialog` with the same `prompt_id` confirms waiting (section 8.5) | `Stop` payload's `last_assistant_message`, bounded, stored owner-private | Tool events in the transcript, read after the stop grace window | Usage fields in the transcript, read after the stop grace window, including subagent transcripts (see below) |
| Codex | `UserPromptSubmit` (new managed hook), digest-matched, binds `turn_id` | Managed `Stop` hook with the bound `turn_id` | Managed `PermissionRequest` hook opens a pending decision; waiting confirmed by detection (section 8.5) | `Stop` payload's `last_assistant_message`, bounded | Not reported: `unknown` | Not reported: `unknown` |
| Hermes | Plugin lifecycle hooks | Plugin lifecycle hooks | Plugin-reported when available | Plugin-reported, bounded | Plugin-reported when available | `unknown` unless the pinned runtime reports it |
| Shell | Not supported for tasks | — | — | — | — | — |

Because the Claude transcript can lag the hook, anything read from it
(commands, metrics) is read after the stop grace window and is `unknown` if
the transcript does not yet contain the turn's final assistant message.

Attention evidence is a separate question from settlement evidence. Where the
provider reports approvals or questions (OpenCode `permission.asked` and
`form.created`; Codex and Claude `PermissionRequest`; Claude permission and
elicitation notifications), the turn settles `attention` with typed text
and, where the payload offers them, choices — but only once waiting is
confirmed; a `PermissionRequest` alone is a pending decision that another
hook or rule may still resolve (section 8.5). Where it does not, a detection `blocked`
transition after the consumption milestone settles `attention` with
`text: null` and `evidence_degraded: true` (kind `approval`) — never with
scraped screen text; section 8.5 defines how such an attention can be
answered. A blocked prompt with no such transition runs to `timed_out` as
before.

Claude usage completeness (invariant 6) covers subagents: usage from
subagent transcripts started during the turn is summed into the turn's
metrics; if any subagent transcript is unreadable or unbounded, the turn's
metrics are `unknown`.

The facts above are pinned with sanitized goldens in `compat/<agent>/`
(Codex already has `compat/codex/subagent-hooks.json` captured from
`schema.rs`; Claude and OpenCode locks are new) and re-verified on every
provider pin bump. Where a field is missing at runtime, the column degrades to
`unknown` rather than being guessed.

### 9.3 OpenCode (new agent)

OpenCode 2.x is client/server: its TUI talks to a server over HTTP with a typed
event stream. That makes it the first agent whose turns can be settled on
provider events instead of screen detection. The following OpenCode 2.x
behaviours were verified on 2026-09-25 against the installed 2.0.14 binary
(its CLI, the JavaScript bundled in it, and isolated live runs of
`opencode serve`), and the design depends on them. Each one is pinned in
`compat/opencode/` with its evidence and re-verified by the compatibility
gate on every OpenCode pin bump. Facts marked *(unverified)* come from the
earlier design pass and must be confirmed by that lock before
implementation:

- **Shared background service.** The TUI, `run` and `api` accept
  `--standalone` ("run with a private server instead of the background
  service") and `--server <url>` ("connect to a server URL instead of the
  background service"); the two cannot be combined. Without either they
  attach to a shared `opencode service`, whose work runs outside the pohunek
  worker with the service's environment, cwd and configuration, and survives
  session stop. Pohunek must never launch OpenCode in that mode.
- **Server listen address.** `opencode serve` takes `--hostname`, `--port`,
  `--cors`, `--service` and `--stdio`. It binds TCP only: a filesystem path
  or `unix:` URL as `--hostname` fails with `ServeError`, although the
  server's internals know a Unix address type. `--port 0` binds an ephemeral
  port and the server prints `server listening on http://<host>:<port>` on
  stdout.
- **Server authentication.** The server reads `OPENCODE_SERVER_PASSWORD`
  (or `OPENCODE_PASSWORD`) and then requires HTTP Basic authentication on
  its `/api/*` routes (`401` with `www-authenticate: Basic` otherwise); the
  static web UI routes stay open but expose no session data. Clients
  (`--server`) read the password from `OPENCODE_PASSWORD`. When no password
  is supplied, 2.0.14 **generates a random one and prints it to stdout**; a
  supplied password is not printed. (Upstream documentation for other
  versions describes an unauthenticated server with a warning instead, which
  is why this is a pinned, version-specific fact.)
- **Project directory from `PWD`.** `run` resolves its root as
  `root ?? process.env.PWD ?? process.cwd()`, and the TUI changes directory
  to `process.env.PWD ?? process.cwd()`. The PTY child inherits the worker's
  environment (`crates/session-worker/src/pty.rs:388-407`), so the adapter
  must pass the directory explicitly and set `PWD` to the session cwd.
- **Native configuration only.** `OPENCODE_CONFIG_CONTENT` is read. Permission
  rules have the shape `{action, resource, effect}` with effect `allow`,
  `deny` or `ask`. *(unverified)* Agents are defined under `agents` with
  ordered `permissions` rules (last match wins; default base `* allow`); the
  legacy `agent` key is decoded with the 1.x schema and silently drops
  `permissions` and `system`; `OPENCODE_CONFIG_CONTENT` is loaded last.
- **Credentials live in `opencode.db`.** An isolated data directory contains
  only a fresh `opencode.db` and inherits no logins. *(unverified)* `auth.json`
  is only imported once by a legacy migration.
- **Resume and fork.** The TUI accepts `--session <id>` to resume; it has
  **no** `--fork` flag (only `run` and `mini` do, and they require
  `--continue` or `--session`). Forking for the TUI therefore goes through
  the server API (`/api/session/{sessionID}/fork`) followed by
  `--session <new-id>`.
- **No activity in the terminal title.** The TUI sets only `OpenCode` or
  `OC | <title>`, so title-based detection (used for Claude) is unavailable.
- **Typed events.** The server defines `session.execution.started`,
  `session.execution.succeeded`, `session.execution.failed`,
  `session.execution.interrupted`, `session.idle`, `session.status`,
  `permission.asked` / `permission.replied` (reply `once`, `always` or
  `reject`), `form.created` / `form.replied` / `form.cancelled` (questions
  addressed to the user — there is no `question.asked` event),
  `session.step.ended` carrying `cost` and `tokens`, and
  `session.tool.called` / `session.tool.success` / `session.tool.failed`.
  *(unverified)* Which tool events carry shell commands and exit codes.

Runtime design:

1. The worker starts a **per-session** `opencode serve --hostname 127.0.0.1
   --port 0` child as a second supervised child in its **own process group**
   (the PTY child is a new POSIX session leader under `portable-pty`, so a
   worker-spawned sibling cannot join its group), with the session cwd,
   `PWD`, and the profile's `OPENCODE_CONFIG_CONTENT`. The server executes
   tools with the owner's authority and a loopback port is reachable by every
   local user, so authentication is mandatory: the worker generates a
   per-session random password, passes it to the server as
   `OPENCODE_SERVER_PASSWORD` and to the TUI as `OPENCODE_PASSWORD`, and never
   records it. The worker always supplies the password, because without one
   2.0.14 generates and prints its own. The worker reads the listen URL from
   the server's stdout and otherwise discards that stream; it is never
   written to logs, scrollback or evidence. `--port 0` avoids per-session
   port allocation races. The worker verifies before starting the TUI that
   an unauthenticated `/api` request is refused; if it is not (an OpenCode
   version without authentication), the adapter refuses to launch rather
   than exposing an unauthenticated tool-executing port. A Unix socket would
   be preferable but is not available through the 2.0.14 CLI; the adapter
   adopts it when a pinned version exposes it.
2. The PTY runs the OpenCode TUI attached to that server (`--server <url>`,
   plus the project directory). Attach, screen and output work unchanged.
3. The worker subscribes to the server's event stream and forwards a bounded,
   typed subset to the daemon as turn evidence through the same sequenced,
   offset-stamped evidence journal as hook records (section 9.2), so events
   emitted during a daemon outage are replayed after reconnect. Raw event
   payloads are not stored; only the fields named above.
4. Native session identity comes from the server, so no hook or plugin is
   needed for resume, fork or settlement. Resume is TUI `--session <id>`;
   fork is a server API call followed by `--session <new-id>` (the TUI has no
   `--fork`).
5. Final message and per-message usage are read from the server's session API
   after settlement, filtered to the assistant messages observed during the
   turn.
6. A detection manifest still exists for owner-facing activity when the event
   stream is unavailable, with `evidence_degraded` reported.
7. Credentials are provisioned explicitly. An isolated per-session data
   directory does not inherit provider logins, so the task profile declares a
   *reference* to a secret (host keyring entry or owner-only file under the
   host configuration) and the worker injects the resolved value only into
   the server child's environment. Secret values never enter task records,
   events, logs, `Debug` output, or compatibility goldens — the existing
   provider-credential posture. The value remains readable by same-UID
   processes through the child's environment; this RFC claims nothing beyond
   the host Unix-account boundary (#88). A profile whose secret reference
   cannot be resolved fails at launch with a typed error instead of starting a
   server that cannot authenticate.
8. The server and the TUI form **one runtime generation**. If either child
   exits, the worker ends the generation and stops the other child; recovery
   starts both again, the TUI resumed with `--session <id>` against the new
   server URL. Restarting the server alone is never attempted: `--port 0`
   yields a different port on every start and the TUI cannot follow a
   replaced `--server` endpoint. Item 6 covers only an event stream that
   drops while the server process is alive. The worker records the server's
   process-group id beside the PTY child's (`crates/session-worker/src/pty.rs`
   already tracks and `killpg`s the PTY group), and stop, cleanup and
   generation end signal and join **both** groups on Linux and macOS.

**Decided runtime shape:** the per-session server and the TUI are two
children of one worker. This avoids a plugin installation step, gives typed
evidence and request-addressed answers (section 8.5) without a hook channel,
and was verified against 2.0.14 (authentication, ephemeral port, fork and
reply APIs). It requires the worker protocol to model a secondary child with
the same stop, cleanup and generation guarantees as the PTY child
(workstream 2). The plugin design is recorded under Alternatives (section
18).

## 10. Task Result

A settled turn's result is one document, bounded by `tasks.result_max_bytes`:

```json
{
  "task_id": "t-01...",
  "turn": 2,
  "settlement_revision": 1,
  "superseded": false,
  "outcome": "completed",
  "settled_by": "provider_events",
  "finality": "provider_confirmed",
  "reopen_window_closes_at": null,
  "evidence_degraded": false,
  "steered": false,
  "elapsed_ms": 84211,
  "final_message": { "text": "...", "truncated": false },
  "attention": null,
  "repository": {
    "base": "3fd25bf",
    "owner_task_id": "t-01...",
    "head_before": "3fd25bf",
    "head_after": "3fd25bf",
    "dirty_before": true,
    "dirty_after": true,
    "integrity": "clean",
    "diff": { "files": ["runner/runner.go", "runner/runner_test.go", "runner/config.go"], "added": 58, "removed": 5 },
    "turn_delta": { "files": ["runner/runner.go", "runner/runner_test.go"], "added": 41, "removed": 3, "complete": true },
    "snapshot_start": "snap-...",
    "snapshot_end": "snap-...",
    "worktree_fingerprint": "sha256:...",
    "drift_since_previous_turn": false,
    "modified_during_checks": false
  },
  "checks": [
    { "name": "unit", "outcome": "passed", "exit_status": 0, "duration_ms": 9120, "attempts": 1, "log_ref": "..." }
  ],
  "commands": [ { "command": "go test ./runner", "exit_status": 0 } ],
  "metrics": { "input_tokens": 43245, "cached_input_tokens": 1280, "output_tokens": 413, "cost": { "amount": "0.0067", "currency": "USD" } },
  "session": { "session_id": "s-01...", "runtime_id": "runtime-1", "runtime_generation": "1" }
}
```

Rules:

- `final_message` is the provider's last assistant message for the turn when a
  turn-evidence source supplies it; otherwise `null`, never scraped from the
  screen.
- Field provenance is explicit: `final_message`, `commands` and `metrics` are
  provider **claims** the orchestrator must weigh as such; `repository` and
  `checks` are daemon-verified **facts** (invariants 5, 6, 9). A result never
  upgrades a claim into a fact.
- `commands` and `metrics` are `null` when unknown, `[]`/zeros only when the
  provider positively reports none. `metrics.cost` carries the amount as a
  decimal string and the currency exactly as the provider reports it; if
  the provider does not state a currency, `cost` is `null` rather than
  assumed.
- **Bounded lists.** Every list in the result has a configured cap and a
  deterministic truncation: `repository.diff.files` and `turn_delta.files`
  keep the first `tasks.result_max_files` paths in byte order;
  `commands` keeps the first `tasks.result_max_commands` in provider order;
  `claim_mismatches` keeps the first `tasks.result_max_mismatches`;
  attention `choices` keep the first `tasks.attention_max_choices`;
  `checks` is bounded by `tasks.max_checks_per_turn` at request time and is
  never truncated. A truncated list carries `truncated: true` and its total
  count; the full file lists stay reachable through `session diff --turn`
  and full check output through `task.check_log`. The caps are validated at
  startup to fit within `tasks.result_max_bytes` together with the string
  limits, so publication never blocks occupancy on an oversized result.
  List truncation never changes `integrity`; only snapshot truncation does
  (`turn_delta.complete`).
- `settlement_revision`, `superseded`, `finality` and
  `reopen_window_closes_at` identify and qualify this result (sections 6.3,
  6.4, 8.2). `worktree_fingerprint` is the fingerprint at publication; a
  review binds to it (section 13.1).
- `diff` summarizes the whole worktree against `base` (invariant 5);
  `turn_delta` summarizes what changed between this turn's start snapshot
  and its settlement snapshot, which is what the turn's agent (plus any
  steering) did. On a shared worktree (section 8.7) `diff` includes earlier
  rounds' work and `turn_delta` isolates this round. `turn_delta.complete` is
  `false` when either snapshot was truncated (below); the delta is then a
  lower bound and `integrity` is at most `suspect`.
- The full diff is not inlined. The result carries only the summary;
  historical per-turn patches come from the snapshots below through
  `session diff` with a new `turn` selector. Without the selector,
  `session diff` keeps reading the current worktree
  (`crates/daemon/src/session/diff.rs:119`).
- **Worktree snapshots.** A fingerprint of names and digests cannot
  reproduce a file that a later round overwrote or deleted, so historical
  diffs need content. At turn start and at settlement the daemon records a
  snapshot of the worktree in a **daemon-private shadow repository** under
  the daemon state directory (one per worktree, owner-only), with its own
  index, so snapshotting never writes to the project's `.git`, refs, index
  or working tree (invariant 9). Capture may read the project's object store
  through Git alternates for speed, but **durability never depends on
  them**: objects borrowed through alternates are not protected by the
  shadow repository's refs, and a history rewrite followed by garbage
  collection in the project repository can delete them (the hazard Git's
  documentation describes for `--shared` repositories). Before a result is
  published, the daemon therefore copies into the shadow repository's own
  object store every object the published per-turn patch needs: the snapshot
  trees and every blob whose content differs between the turn's start and
  end snapshots, on both sides. Blobs identical in both snapshots are never
  read by a per-turn diff and may stay borrowed. The guarantee is scoped
  accordingly: the **per-turn patch** (`turn_delta` and `session diff` with
  `turn`) stays reproducible after any change to the project repository; the
  whole-worktree `diff` against `base` is recorded as a summary at
  publication and is not promised as a historical patch. Snapshots
  honor the worktree's ignore rules (ignored files are not captured and are
  reported as a count, never content). New content per snapshot is capped
  by `tasks.snapshot_max_bytes` and per-file by
  `tasks.snapshot_max_file_bytes`; over-cap files are recorded by path and
  digest only and mark the snapshot `truncated`. Line counts in `diff` and
  `turn_delta` are exact only between untruncated snapshots. Snapshots are
  task **content** for retention (section 14): they retire with the task's
  session, and a `session diff` `turn` selector on a retired snapshot fails
  with `task_snapshot_retired`.
- `log_ref` values are opaque references resolvable through `task.check_log`
  with byte offsets, reusing the bounded paging model of `session output`.
- `integrity` summarizes trust in the repository evidence: `clean` when the
  verified facts agree, `suspect` when drift, steering, an incomplete
  `turn_delta`, a modification during checks, or `evidence_degraded`
  undermines attribution, or when a provider claim disagrees with a verified
  fact, and `violation` only when an investigate-mode turn modified the tree.
  Claim/fact disagreements are listed in `claim_mismatches` with the two
  concrete comparisons the daemon performs: file paths reported by provider
  tool events (edits, writes) that are absent from `turn_delta`, or present
  in `turn_delta` without any provider tool event or steering to explain
  them; and a provider-reported command exit status that contradicts a check
  running the identical argv. No other claim comparison is implied.
  `integrity` is a summary dimension over the specific fields, not a
  replacement for them.
- `settled_by` is one of the values defined in section 8.2; `outcome` is one
  of the values in section 6.3.
- Drift is computed by comparing a bounded worktree fingerprint at the start of
  a turn with the fingerprint recorded at the previous settlement: HEAD,
  branch, tracked diff, and untracked names with content digests capped by
  `tasks.fingerprint_max_entries` and `tasks.fingerprint_max_bytes` (overflow
  marks the fingerprint `suspect` rather than hashing unboundedly). The
  fingerprint is the cheap equality check; the snapshots above carry the
  content. Drift never
  blocks a turn; it is reported so the orchestrator knows someone else changed
  the tree.

## 11. Profiles, Permissions and Investigations

### 11.1 Task-capable profiles

A per-host agent profile (Part C of the project actions design) becomes
task-capable by declaring a `task` table: turn deadline override, optional
investigation mode, and — for OpenCode — the native configuration overlay
(model, steps, permission rules). Profiles that do not declare it can still be
used for tasks; they inherit the host defaults.

### 11.2 Investigation mode

`task.start { mode: "investigate" }` requests a turn in which the agent must not
modify the worktree. The profile states how that is enforced, and the result
reports it as `enforcement`:

| Enforcement | Meaning |
| --- | --- |
| `provider_rules` | Provider tool permissions deny writes and shell (e.g. OpenCode `permissions`: `* deny`, `read`/`glob`/`grep` allow, `external_directory` deny). |
| `provider_rules+readonly_mount` | Additionally, the profile wraps the agent in a read-only filesystem view (e.g. bubblewrap) declared by the operator. |

The daemon verifies after settlement that the worktree fingerprint is unchanged
and reports `modified: true` (`integrity: violation`, section 10) otherwise,
regardless of enforcement. It never claims isolation beyond the declared
enforcement. A profile that cannot enforce investigation mode (for example a
Claude profile without a writable-tool deny configuration) refuses
`mode: "investigate"` at `task.start` with `task_investigate_unsupported`
rather than silently downgrading. Investigation mode on a shared worktree
(`worktree_of`, section 8.7) checks the same fingerprint: an auditor that
modified the executor's tree is a `violation`, never a silent edit.

### 11.3 Permission prompts

Tasks do not auto-approve. A profile may pre-grant provider permissions through
the provider's own configuration (for OpenCode, native `permissions` rules), so
routine tool use does not block. Anything that still asks settles as
`attention`. `task.answer` is available to any caller that may call
`task.continue`; an owner-level policy (`tasks.answer_policy`) can restrict it
to the owner's interactive clients. `tasks.answer_policy` governs callers the
daemon can identify — the local owner's clients. Relay-path answer authority is
deliberately not expressed here: the relay dark factory RFC defines it as a
per-`HostShare` capability (`allow_delegated_answers`) intersected with an
explicit relay grant, so the daemon never inspects relay principals.

## 12. Checks

Checks are named commands defined like project actions:

- host-level: `~/.config/pohunek/checks/<name>.toml` or the project's host
  configuration;
- in-repo: `.pohunek/checks/<name>.toml`, **disabled by default**.

In-repo checks need a gate that does not exist today. The per-project
actions design deliberately has **no trust gate** for in-repo definitions and
hooks (`docs/design/per-project-actions-and-worktree-hooks.md`, "Security &
trust"), and the current runner executes repository hooks without one; this
RFC does not change that accepted posture for hooks. Checks differ in one
way that justifies their own gate: a delegating caller, possibly a relay
principal, chooses when they run, so a repository-defined check would let
the repository's content decide what an orchestrator can make the host
execute. The gate is:

- `checks.allow_in_repo` in the project's **host** configuration (the
  owner-only project configuration the daemon already loads, never a file in
  the repository), default `false`, validated at load as a boolean;
- scope: one registered project; it never applies to other projects or to
  hooks;
- when `false`, in-repo check definitions are ignored, `task.inspect` lists
  them as `disabled_in_repo`, and naming one fails with
  `task_check_not_permitted`;
- when `true`, in-repo checks may declare their own argv, working directory,
  timeout and environment allowlist — arbitrary execution as the daemon user,
  the same blast radius as in-repo hooks. That is precisely why the gate
  exists: the A.5 safe subset (names only, never argv) governs templates and
  actions and cannot express a check. On the relay path they are further
  limited to the share's permitted-check list below.

**Where a definition is read from, and which one wins.** A check result is
only a daemon-verified fact if the agent under test could not rewrite the
check itself. Therefore:

- **Host definitions always win.** On a name collision the host definition
  runs and the in-repo one is ignored (reported in `task.inspect` as
  `shadowed_by_host`). On the relay path, the share's permitted-check list
  is matched against the definition that actually resolves: a plain entry
  (`unit`) permits only a host definition, and `repo:unit` permits only a
  `repo_base` definition. A shadowed name therefore resolves to the host
  definition and needs the plain entry; `repo:unit` alone does not permit
  it (relay dark factory RFC section 7.1).
- **In-repo definitions are read from the task's base commit** — for a
  `worktree_of` task, the owner task's base — so a check added on the branch
  after that base is not runnable until a later task starts from a base that
  contains it; this is the cost of never reading definitions from a tree
  the agent writes. They are never read from
  the worktree (`git show <base>:.pohunek/checks/<name>.toml` through the
  sanitized read-only Git invocation, invariant 9). An agent that edits
  `.pohunek/checks/unit.toml` during a turn changes nothing about which
  argv runs; the edit shows up in `turn_delta` like any other file.
- Each check entry in the result carries `definition_source` (`host` or
  `repo_base`). Any `repo_base` check caps the result's `integrity` at
  `suspect`: its definition came from the repository under test.
- For every source, a check executes **in the agent-modified worktree**, so
  scripts, build files and tests it invokes may have been changed by the
  agent. A check result is a verified fact about what that command did in
  that tree, not about the trustworthiness of the tree; `turn_delta` shows
  whether files a check depends on changed.

A check declares argv (no shell interpolation unless the argv invokes a shell),
working directory relative to the worktree, timeout, and environment
allowlist. Execution follows the worktree-hook discipline (B.3) for process
groups, timeouts and typed failure. Output capture is **new work**: B.3
discards hook output to `/dev/null`, whereas checks write bounded output to
owner-private log files under the daemon state directory, capped by
`tasks.check_log_max_bytes`.

`task.start` and `task.continue` accept `checks: [name...]`; the profile or
project may declare defaults. Checks run sequentially after a `completed`
settlement and before the result is published. Settlement and publication are
distinct moments: the turn is in the persisted phase **`finalizing`** (section
8.3) until every check has an outcome, `task.result` returns
`task_result_pending`, no `task_result_published` event is emitted, and
`task.wait` keeps waiting within its `timeout_ms`. The published result
therefore never contains a partially-filled `checks` list. `outcome` is
`passed`, `failed`, `timed_out`, `start_failed`, `interrupted` or `skipped`;
`passed` means exit status 0 and nothing more.

**Finalization contract.**

- The daemon owns check processes and launches each in two phases, so no
  crash window leaves an unknown process writing to the tree: (1) it persists
  a **launch intent** (check name, attempt, a random launch token) in the
  task store; (2) it spawns the check in its own process group with the token
  in its environment and the child stopped before `exec` (`SIGSTOP` raised in
  the pre-exec hook), persists the identity it can now read (pid,
  process-group id, process start time, token), and only then continues it
  with `SIGCONT`. Recovery treats an intent without identity as a check that
  never ran: it looks for a stopped same-UID process carrying the token,
  kills it when found, and records the attempt `interrupted`. Process
  identity is always compared with the start time, so a reused pid is never
  signalled.
- Finalization has its own deadline, `tasks.finalize_max_ms`, independent of
  the turn deadline (which only bounds the agent's part of the turn). On
  expiry, the running check is killed (`timed_out`), the remaining ones are
  `skipped`, and the result publishes.
- A daemon restart during finalization kills any surviving recorded check
  process groups (they belong to the dead daemon), then resumes
  finalization: an interrupted check is re-run once if the worktree
  fingerprint still equals the one taken before the first check; otherwise
  it is recorded `interrupted` without a re-run and the result is
  `integrity: suspect`. Checks that already have an outcome are not re-run.
  Each check reports `attempts`.
- The finalization deadline continues across a restart (it is persisted as
  an absolute time), so a crash loop cannot keep a result unpublished.
- **Cancel-and-join barrier.** Check processes are the daemon's, not the
  session worker's, so ending the session does not end them. `task.stop`,
  session stop or removal of a task's session, and the cascade of section
  8.7 first cancel the task's baseline or finalization checks: they send
  the termination signal to each recorded process group, wait up to
  `tasks.check_kill_grace_ms`, send `SIGKILL`, and **wait until every
  recorded process group has exited** (reaped and verified gone). Only then
  do they record the checks as `interrupted`, publish the result, release
  occupancy (invariant 11) and, for removal, touch the worktree. A process
  group that cannot be confirmed gone blocks removal of the worktree and
  keeps occupancy, reported as `check_cleanup_stuck` in `task.inspect`,
  rather than removing a tree still in use.
- Occupancy (invariant 11) holds until publication.

Checks run in the worktree while the agent is still alive in its session, so
the tree can change underneath them (steering, another client). The daemon
fingerprints the worktree before the first check and after the last; a
difference sets `modified_during_checks: true` and `integrity: suspect`
(section 10). Input delivered by the task layer is refused with
`task_result_pending` while checks run.

Check names are an authorization surface: `checks: [name...]` asks the host
to execute owner-configured commands. Local owner clients may name any check
enabled for the project. On the relay path, the `HostShare` policy lists the
check names a share may request (relay dark factory RFC); any other name
fails with `task_check_not_permitted` before the turn is delivered.

An optional **baseline** run (`checks_baseline: true` on `task.start`) executes
the same checks on the task base before turn 1, so results can distinguish
pre-existing failures from regressions. Turn 1 is delivered only after the
baseline finishes; its duration is reported separately and does not count
against the turn deadline. The baseline occupies the worktree (invariant 11),
is bounded by `tasks.finalize_max_ms`, and follows the same restart rules. With `worktree_of` (section 8.7) the baseline runs
on the current state of the shared worktree, not on the owner task's base,
and the result says so (`baseline_on: "worktree"` instead of `"base"`).

## 13. Protocol, CLI, SDK and Adapters

### 13.1 Protocol methods

| Method | Purpose |
| --- | --- |
| `task.start` | Create the session (project/branch/base/profile/mode, or `worktree_of`), deliver turn 1. Idempotent per `(caller_scope, client_request_id)` on owner paths and per operation ticket on the relay path (invariant 3). |
| `task.continue` | Deliver feedback as the next turn. |
| `task.answer` | Answer the pending attention named by `(turn, attention_id, settlement_revision)`; stale answers are refused (section 8.5). Idempotent per `(task_id, caller_scope, client_request_id)` (invariant 3). |
| `task.wait` | Block until the turn's result is final (default) or published (`until`), or the timeout elapses (dedicated connection, task waiter pool, section 8.3). |
| `task.result` | Read a published result by `(turn, settlement_revision?)` without waiting; the revision defaults to the latest and every response states its `result_id` and whether it is superseded. |
| `task.inspect` | Task record, lifecycle state, evidence sources and degradation, worktree users, and one page of turns (see paging below). Also resolves a task by `(caller_scope, client_request_id)` for delivery reconciliation. |
| `task.list` | One page of tasks on the host, filterable by project, lifecycle state, outcome, owner task and run id (see paging below). |
| `task.extend` | Extend an open turn's deadline, or re-open a `timed_out` turn whose work has not visibly ended, within `tasks.turn_open_ceiling_ms` (section 6.3). |
| `task.stop` | Cancel and join the task's check processes (section 12), stop its session and settle any open turn — or a settled `attention` turn, as a new revision (section 6.3) — as `stopped`; on a task without an open turn it is the explicit end of a finished task (section 6.1). Optional preconditions `if_latest_turn` and `require_idle: true` make the stop atomic with a check that the latest turn is exactly that one and has no open turn, pending attention or finalizing result; otherwise it fails with `task_stop_precondition_failed` and changes nothing. On a task that is already `ended`, `task.stop` succeeds as a no-op and returns the recorded end (`already_ended: true`) without evaluating preconditions, so a retried cleanup never fails on its own earlier success. The worktree, metadata and result content survive for `task.result` and `task.review`. Idempotent. |
| `task.review` | Record an external verdict (`accepted`, `changes_requested`, `rejected`) bound to a `result_id` and the `worktree_fingerprint` the reviewer verified, with optional notes and check references (see review binding below). Idempotent per `(task_id, caller_scope, client_request_id)`. |
| `task.check_log` | Page a check log by reference and byte offset. |
| `task.retain_worktree` | Set the retain hold on the worktree's owner task after the fact, while the worktree exists (section 8.7). Idempotent. |
| `task.release_worktree` | Clear the retain hold set by `retain_worktree` on an owner task (section 8.7); the worktree returns to ordinary retention rules. Idempotent. |

`task.start` accepts an optional `run_id`: a client-chosen ULID or UUID
(validated to that syntax, so it can carry no free text or secret and is
distinct from any transport correlation id) stored on the task record, returned
by `task.inspect`, filterable in `task.list`, and never an authorization
input. Orchestrators use it to group the tasks of one objective so a
restarted orchestrator can find them again (section 16).

**Review binding.** A verdict names the `result_id` it judges and the
`worktree_fingerprint` the reviewer verified (normally the one in the result,
or the one an auditor's investigate task observed). The daemon refuses a
review whose `result_id` does not exist (`task_result_unknown`) and records,
never refuses, one whose fingerprint differs from the result's: such a
verdict is stored with `fingerprint_matches: false`. A stored verdict becomes
**stale** — kept, flagged, never deleted — when its result is superseded by a
later settlement revision or when the worktree's current fingerprint no
longer equals the reviewed one. `task.inspect`, `task.result` and the
`task_reviewed` event report each verdict with `result_id`, fingerprint,
`stale` and the reason. Several verdicts per result are allowed (for example
a human after an auditor); each is its own record with its own reviewer
attribution.

**Paging.** `task.list` and the turn list in `task.inspect` are paged. Order
is stable: tasks by `(created_at, task_id)` descending, turns by turn number
descending. A request carries `limit` (at most `tasks.page_max_items`) and an
opaque `cursor`; a response carries at most `limit` items, stays within
`tasks.page_max_bytes` (below the protocol frame limit; fewer items are
returned if needed) and a `next_cursor`. The cursor encodes the last returned
sort key, not an offset: items created after the first page never appear in
later pages of that walk (a new walk sees them), items removed by retention
between pages are simply absent, and nothing is returned twice. A cursor
older than `tasks.cursor_ttl_ms` or from another daemon epoch fails with
`task_cursor_expired`. Reconciliation after a restart therefore walks
bounded pages instead of one unbounded response.

Typed errors introduced by this RFC:

| Code | Raised by | Meaning |
| --- | --- | --- |
| `task_turn_open` | `task.continue`, `task.answer` | The latest turn is still open or already resumed. |
| `task_attention_open` | `task.continue` | The latest turn settled `attention` and was not answered. |
| `task_agent_busy` | `task.continue` | The agent is still visibly working on an earlier prompt. |
| `task_worktree_busy` | `task.start`, `task.continue`, `task.answer`, `task.extend` | Another task occupies the worktree (invariant 11). |
| `worktree_busy` | `session.input`, attach with terminal control | The session's worktree is occupied by another task; only observation is admitted (invariant 11 write fence). |
| `task_worktree_unavailable` | `task.start`, `task.continue` | The shared worktree no longer exists. |
| `task_worktree_mode_conflict` | `task.start` | `worktree_of` combined with `in_place` or `branch`. |
| `task_session_unavailable` | `task.continue`, `task.answer` | The task is `ended` or its runtime is not live. |
| `task_turn_queued` | `task.extend` | The turn is queued behind a re-open window and has no deadline yet (section 8.2). |
| `task_worktree_via_investigate` | `task.start` | An executor `worktree_of` start named an investigate-mode task (section 8.7). |
| `task_turn_ceiling_reached` | `task.extend` | The turn's total open time reached `tasks.turn_open_ceiling_ms`. |
| `task_answer_unsupported` | `task.answer` | A degraded attention whose manifest declares no approve/deny input (section 8.5). |
| `task_answer_unverifiable` | `task.answer` | A keystroke answer without `allow_unverified_delivery` (section 8.5). |
| (reason) `task_turn_reopened` / `task_stopped` / `session_ended` | result of a queued turn | A queued turn settled `cancelled` without delivery because the previous turn re-opened, the task was stopped, or the session ended (section 8.2). Returned by `task.wait`/`task.result`, not as a call error. |
| `task_payload_mismatch` | retried `task.start`, `task.continue`, `task.answer` | The resubmitted payload does not match the stored digest or ticket fingerprint (section 8.8). |
| `task_stop_precondition_failed` | `task.stop` | `if_latest_turn` / `require_idle` did not hold. |
| `worktree_users_changed` | `session.remove` | `expected_worktree_users` differs from the current set (section 8.7). |
| `task_attention_stale` | `task.answer` | The named attention is not the current pending one at that revision, or the provider already resolved it (section 8.5). |
| `task_result_unknown` | `task.review`, `task.result` | No result with that `result_id`. |
| `task_snapshot_retired` | `session.diff` with `turn` | The turn's snapshots were retired with the session content (section 10). |
| `task_cursor_expired` | `task.list`, `task.inspect` | The paging cursor is too old or from another daemon epoch. |
| `worktree_in_use` | `session.remove` | Other active tasks use the session's worktree (section 8.7). |
| `task_result_pending` | `task.result`, `task.continue` | The turn settled but its checks have not finished. |
| `task_check_not_permitted` | `task.start`, `task.continue` | A requested check is not enabled or not permitted for the caller's origin. |
| `task_store_full` | `task.start`, `task.continue` | A task store cap (`tasks.store_max_bytes`, `tasks.max_turns_per_task`, `tasks.max_tasks_retained`) would be exceeded (section 14). |
| `task_waiter_limit_reached` | `task.wait` | The task waiter pool is full. |
| `task_request_conflict` | all idempotent methods | A reused request key with different parameters. |
| `task_investigate_unsupported` | `task.start` | The profile cannot enforce investigation mode (section 11.2). |

Events on `subscribe`: `task_turn_opened`, `task_result_published` (emitted at publication, after finalization, not at settlement), `task_result_final` (when a heuristic result's re-open window closes),
`task_turn_attention`, `task_reviewed`. Notifications gain `task_settled` and
`task_attention` kinds with the existing policy and retention machinery. For
a task session, the hook-driven `turn_completed` and `approval_required`
notifications for the same provider event are deduplicated into the task
kinds through the existing dedupe key, so one turn never notifies twice.

Every method follows the existing protocol conventions: typed errors with class,
code and recovery hint; decimal-string counters; generated TypeScript types; and
ripples through `client`, `daemon`, `cli`, `gui-core` and `web`.

### 13.2 CLI

```sh
printf '%s' "$PROMPT" | pohunek task run --agent opencode-deepseek \
  --project pohunek --branch zajca/fix-x --checks unit,clippy --wait --json
pohunek task continue t-01... --stdin --wait --json
pohunek task answer t-01... --turn 2 --attention t-01.../2/a1 --revision 1 approve --json
pohunek task wait t-01... --timeout 10m [--until published] --json
pohunek task extend t-01... --by 10m --json
pohunek task show t-01... [--turns-cursor ...] --json      # task.inspect
pohunek task result t-01... --turn 2 [--revision 1] --json
pohunek task list [--correlation-id ...] [--cursor ...] --json
pohunek task check-log t-01... <log-ref> [--offset N] --json
pohunek task review t-01... --result t-01.../2@1 --fingerprint sha256:... accepted --json
pohunek task stop t-01... [--if-latest-turn 2 --require-idle] --json
pohunek task retain-worktree t-01... --json
pohunek task release-worktree t-01... --json
```

Every protocol method in section 13.1 has a subcommand. `--wait` makes `run`
and `continue` one process that delivers the turn and then waits until the
result is final (section 8.3), so an orchestrator needs **one tool call per
turn**. The JSON envelope is the existing versioned `{cli_version, protocol,
ok|err}` document. Per AGENTS.md, the command group ships with shell
completion, the README command reference, `docs/public-api.md`, and the
`docs/knowledge/` bundle updated in the same change.

### 13.3 Agent skill

The embedded skill gains a "Delegating a task" section that replaces the
multi-step send-and-wait recipe for delegation with `task run --wait` and
`task continue --wait`, and states how to act on each outcome. The existing
session-level recipe stays for interactive steering.

The skill also gains a "Delegating long-horizon work" section prescribing the
composition of section 16: a bounded number of task rounds per objective (a
round budget is orchestrator policy, never a daemon default), one audit pass
per round (`mode: "investigate"` task or the round's `checks`), a
`task.review` verdict recorded before the next round, and fresh-context rounds
on a shared worktree (`worktree_of`, section 8.7) rather than one
ever-growing conversation, all tagged with one `run_id` per
objective. It tells orchestrators to judge each round by `turn_delta` and to
treat `provider_hooks_uncorrelated` and `detection` settlements as weaker
evidence, to treat `finality: heuristic` results as provisional until their
re-open window closes, to bind every review to the result's `result_id`, and
to `task.stop` each finished executor and auditor task once its verdict is
recorded (section 16.2).

### 13.4 Optional MCP adapter

`pohunek mcp` is a stdio MCP server inside the CLI binary that exposes
`task_run`, `task_continue`, `task_wait`, `task_show`, `task_answer` and
`task_review` as tools. It is a thin client of the public protocol: no state,
no daemon-side MCP, same host targeting and origin guard. Tool calls block like
`--wait`, bounded by the same daemon ceiling and by the MCP client's own tool
timeout, which the adapter reports in its tool descriptions.

This does not conflict with the universal assistant's "no MCP knowledge server"
decision, which concerns serving documentation, not delegating work. Whether
to ship the adapter in the same milestone is an open question (section 19).

## 14. Storage, Retention and Secrets

- Task data does **not** go into the unified metadata store. That store is
  one JSON-lines file rewritten whole on every mutation and capped at 16 MiB
  (`crates/daemon/src/store/mod.rs:1-11`, `MAX_METADATA_STORE_BYTES`);
  results multiplied by turns, tasks and `tasks.metadata_retention` would
  fill it and slow every session mutation. Tasks use a **per-task
  directory** under the daemon state directory (owner-only): one file per
  task record, turn record, result revision, review verdict and delivery
  record, each written with the per-record atomic persistence of
  `crates/daemon/src/host_state/persistence.rs` (temp file, fsync, rename,
  bounded record size, `tasks.record_max_bytes`). The unified store holds no
  task data; the task index (by id, `run_id`, request key, worktree)
  is rebuilt at startup by a bounded directory scan and kept in memory. The
  store is bounded as a whole: `tasks.store_max_bytes` caps the per-task
  directories together, `tasks.max_turns_per_task` caps turns per task and
  `tasks.max_tasks_retained` caps retained task directories; `task.start` and
  `task.continue` are refused with `task_store_full` when a cap would be
  exceeded, retention sweeps the oldest retired content first within the
  caps, a full disk fails the mutation typed and never truncates a record,
  and the startup scan is bounded by the same caps.
  Mutations that span records, the session store and external effects
  follow a persisted **task operation journal** shaped like the relay RFC's
  ticket journal (section 12.5) and the create intent of #192: `task.start`
  records `intent` (request key, fingerprint, parameters) before anything
  else, `session_created` after the session store commit (with the session
  id), `bound` once the task record and its occupancy, user and hold entries
  are written, and `delivered` per section 8.8; `task.stop`, cascade
  removal and hold changes record their own intent and completion. Recovery
  replays the journal: an `intent` without `session_created` is re-driven
  only by a client retry with the same key; a `session_created` without
  `bound` is completed (the session becomes the task's) or, when the task
  record cannot be written, the session is stopped and removed through the
  ordinary cleanup stages and the operation ends `failed`; a `bound` without
  `delivered` is the section 8.8 case. Occupancy, users and holds are
  derived from `bound` records at startup, never trusted from a partial
  write. Crash-injection tests cover every journal boundary.
- Turn records hold cursors, settlement metadata, result documents, check
  references and review verdicts. Prompt text is not stored in task records;
  a keyed fingerprint identifies it.
- Final messages and check logs are owner-private files under the daemon state
  directory, bounded by `tasks.final_message_max_bytes` and
  `tasks.check_log_max_bytes`. The result document's provider-reported text
  fields (`commands`, attention text and choices, `claim_mismatches`) are
  content under the same rule (invariant 7): the metadata copy that outlives
  the session (below) carries them as `retired`, never their text.
- Task content follows session retention: removing a session removes its
  final messages, check logs, result documents and worktree snapshots
  (section 10). `session.retention.sweep` covers both, and never removes a
  session whose worktree carries a `worktree_shared` hold (section 8.7).
- Task **metadata** outlives the session for `tasks.metadata_retention`
  (daemon configuration, validated at startup): task and turn ids, lifecycle
  state, outcomes, `settled_by`, `integrity`, repository summaries, check
  outcomes (without logs), `run_id`, owner task, timings and review
  verdicts. This is what lets an orchestrator reconstruct its verified
  picture after its own crash (section 16) even when sessions were already
  swept; `task.result` for such a turn returns the metadata with content
  fields marked `retired`.
- Provider events forwarded by the worker are field-allowlisted; unknown fields
  are dropped, never persisted.

## 15. Failure Modes

| Situation | Behaviour |
| --- | --- |
| Client disconnects during `task.wait` | No effect on the turn; call `task.wait` again. |
| Daemon restart during an open turn | Worker keeps running and keeps journaling hook evidence arriving on its socket (section 9.2); the daemon reconnects and requests evidence after its last acknowledged sequence, so a `Stop` emitted during the outage still settles the turn causally. Detection evidence for the outage interval is only `reconstructed` from replayed output and never settles on its own (section 8.2). A journal overflow reports a gap and marks affected turns `evidence_degraded`. |
| Runtime generation lost | Open turn settles `lost`; the task can continue after explicit recovery, starting a new turn. |
| Agent exits cleanly mid-turn | Open turn settles `exited`; the task becomes `ended`. |
| Late `Stop` from an earlier prompt | Never applied to the open turn: its `prompt_id`/`turn_id` differs from the one bound to it (section 8.2). If it matches a `timed_out` predecessor, that turn completes late (section 6.3); otherwise it is discarded. Without identifiers it is discarded if it precedes the new turn's consumption milestone, otherwise the settlement is marked `provider_hooks_uncorrelated`. |
| Another `Stop` hook blocks and the agent continues | Work resumes inside `tasks.stop_settle_grace_ms`; the turn stays open. |
| Task waiter pool full | `task.wait` fails with `task_waiter_limit_reached`; session waits are unaffected. |
| Client abandons a long `task.wait` | On the Unix socket peer hangup releases the slot at once; over TCP the per-socket keepalive releases it within the configured detection time (section 8.3). |
| Daemon down across a heuristic completion's windows | Both windows restart at reconnect; the result is `evidence_degraded` with reason `window_overlapped_outage` (section 8.2). |
| Second task started on a busy shared worktree | Refused with `task_worktree_busy` (invariant 11). |
| Provider stalls | Turn deadline settles `timed_out`; agent is untouched. |
| Duplicate `task.start` after a timeout | Same `client_request_id` returns the existing task and turn. |
| Worktree modified externally between turns | Next result reports `drift_since_previous_turn: true`. |
| Crash during delivery | Resolved by the delivery commit protocol (section 8.8): re-dispatch while the daemon still holds the payload, `awaiting_resubmission` after a daemon restart, record when written, `failed` with `delivery_uncertain` when the write was interrupted, `delivery_abandoned` when no resubmission arrives in time. |
| Provider moves from question A to B before an answer to A is written | Addressed answers (OpenCode) are rejected by the provider as stale; keystroke answers are refused unless `allow_unverified_delivery` is set, and are then recorded as unverified (section 8.5). |
| Task stopped while its checks run | Checks are cancelled and joined before occupancy is released or the worktree is touched (section 12). |
| Project history rewritten and garbage-collected | Published per-turn patches remain reproducible from objects owned by the shadow repository (section 10). |
| Daemon restart during finalization | Recorded check process groups are killed; interrupted checks re-run once if the worktree fingerprint is unchanged, otherwise `interrupted`; the persisted finalization deadline still applies (section 12). |
| Heuristic `completed` followed by more provider work | Inside the re-open window: new settlement revision, old result superseded, its reviews stale. After the window: next result reports drift and `integrity: suspect` (section 8.2). |
| Late answer to an attention that was already resolved or replaced | `task_attention_stale`; nothing written (section 8.5). |
| `PermissionRequest` resolved by another hook or rule | Pending decision discarded; no attention is settled (section 8.5). |
| Owner session of a shared worktree removed | Refused with `worktree_in_use` while user tasks are active; with `stop_worktree_users` they are stopped first (section 8.7). |
| Human or another client writes into the task session mid-turn | Turn marked `steered`; settlement unchanged; result records it (section 8.6). |
| OpenCode server child dies while the TUI lives | The TUI is bound to the dead server's ephemeral `--server` URL and has no reconnect contract, so the worker ends the runtime generation exactly as for a PTY-child exit: the TUI is stopped, an open turn settles `lost`, and explicit recovery starts a new generation with a fresh server and a TUI resumed on the stored OpenCode session (`--session <id>`, section 9.3 item 8). Evidence never silently degrades to detection while a TUI points at a dead server. |
| Check hangs | Check timeout, process-group kill, `timed_out` outcome; the turn result still publishes. |

## 16. Composition: Manager/Auditor Loops and the Relay Dark Factory

The daemon is the chassis: it owns sessions, turns, causal settlement and
bounded evidence. Everything about *deciding what work to do next* is
deliberately above it. This section fixes the intended composition so no
implementer grows an orchestration brain inside the daemon or the relay.

### 16.1 The manager/auditor pattern

The proven shape for long-horizon work is a Manage-Execute-Audit loop (as
systematized by Ma et al., "LongHorizon-Harness: Advancing Long-Horizon
Agents for Real-World Tasks", arXiv:2608.01964, 2026): a manager holds the
task state outside any execution context, a fresh-context executor performs
one bounded subtask, and a read-only auditor verifies the environment before
that state is trusted. On top of this RFC every role maps to an existing
primitive:

| Loop role | Pohunek primitive |
| --- | --- |
| Manager (persistent task state) | An orchestrating agent in its own session; its task state lives in its own memory or files, never in the daemon; its tasks share a `run_id` |
| Subtask contract | `task.start`/`task.continue` prompt with `checks` and `mode` |
| Executor (fresh context) | A task on the objective's shared worktree: the first round creates it, later rounds use `task.start { worktree_of }` (section 8.7), one round per task; the in-place flag is the explicit-consent fallback |
| Auditor (read-only verification) | A `mode: "investigate"` task with `worktree_of` the executor's task, so it inspects the executor's uncommitted state (sections 8.7, 11.2), and/or the round's `checks` |
| Audit report | The result's verified fields (section 10, including `turn_delta`) plus a `task.review` verdict |
| Ask route | `attention` outcome and `task.answer` (sections 8.5, 11.3) |

Two properties the substrate already guarantees carry the loop's safety: an
executor's claims (`final_message`, `commands`, `metrics`) never count as
evidence by themselves (section 10 provenance, invariants 5, 6), and the
auditor's read-only posture is enforced and reported, never assumed (section
11.2).

### 16.2 Fresh-context composition

A long-horizon objective is decomposed into bounded rounds rather than one
ever-growing conversation. The canonical round form is the **shared
worktree**: the first round's task creates the worktree, and each later round
— executor or auditor — starts a fresh task in that same worktree with
`worktree_of` (section 8.7), so rounds build on each other's uncommitted
state while each executor keeps a fresh context. Running rounds against the
project checkout itself (in-place mode, explicit consent) is the fallback,
with the documented caveat that concurrent in-place sessions share one cwd
and race. Rounds sharing one worktree are sequential by daemon enforcement
(invariant 11); parallel executors need separate worktrees and an explicit
merge, which is orchestrator policy, not daemon behavior. Each round carries
only the compact state the manager chooses to pass; `turn_delta` attributes
each round's own changes and drift detection (section 10) attributes
external ones; `task.continue` remains for short steering within a round.
The rationale is empirical: fresh-context execution with audited state
raises the failure floor on multi-step work, while long conversations rot
and compound errors.
**Round cleanup.** A finished round is not a finished task: `completed`
leaves the executor's and auditor's sessions running (section 6.1). After
recording a round's verdict, each finished task is stopped by a client that
has lifecycle authority over it — the executor by the manager that created
it, the auditor task by the auditor that created it — using `task.stop` with
`if_latest_turn` and `require_idle` (section 13.1), so a stop can never hit a
turn that started after the client decided. The first round's executor is
also the worktree's owner task and is stopped after its round like every
other executor: the manager sets `retain_worktree: true` on that first
`task.start`, so the tree carries a retain hold that outlives every task
(section 8.7) and no session has to stay alive between rounds. The manager
calls `task.release_worktree` only when the objective is done and the
worktree's result has been merged or discarded by explicit orchestrator
policy — stopping a session never removes its worktree, and an unreleased
hold expires after `tasks.worktree_hold_max_age` into the ordinary retention
rules.

Recovery cannot infer "this round is finished" from task state alone: a
still-valid verdict on the latest result says nothing about a turn opened
since, and an auditor task never carries a verdict of its own (the verdict
is recorded on the executor's result). Clients therefore keep a durable
**round record** before cleaning up — round id, executor and auditor task
ids, the reviewed `result_id` and each task's latest turn at close — and a
restarted client stops exactly the tasks listed in closed round records,
with those turns as `if_latest_turn` preconditions. A precondition failure
means work continued after the round closed; the client leaves that task
running and re-plans instead of stopping it. Without this step a sequential
run accumulates live sessions until it exhausts any concurrency limit, and a
new period does not release them.

The daemon does not enforce round budgets — a round budget (N rounds or a cost
ceiling per objective) is orchestrator policy, and the skill states it as such
(section 13.3).

### 16.3 The dark factory

With manager, executor and auditor all above the substrate, the loop can run
unattended — a dark factory: the daemon is the chassis, tasks are the
production line, and humans enter only at two explicit points, `attention`
escalations (section 8.5) and `task.review` verdicts. The team relay (the
accepted relay RFC) is the natural team-facing home for such a factory without
changing anything in this design:

- relay **service accounts** run manager and auditor agents under relay
  identity, not personal credentials (relay RFC section 13.4);
- `task.*` routes through relay ACLs exactly like `session.*` (section 3), so
  team policy decides who may delegate to which host;
- relay **audit records** cover every `task.*` call as an attributable action;
  relay **delegation budgets** (relay dark factory RFC) bound the factory's
  delegation volume the way `tasks.turn_open_ceiling_ms` bounds a single
  turn, while relay transport quotas keep bounding resources;
- `attention` becomes the human-in-the-loop gate: notifications reach the
  owning principal, and answer authority on the relay path resolves through
  the relay dark factory RFC's per-share capability plus an explicit relay
  `task.answer` grant.

Honesty constraints carry over unchanged: relay ACLs are authorization, not
workload isolation (that boundary stays with #88), and nothing here moves PTY,
worktree or evidence authority off the host daemon.

### 16.4 What stays out

There is no manager, auditor, planner or task-state store in the daemon or in
the relay runtime. If a future milestone adds host-side orchestration, it must
be a client of the public protocol like any other (non-goal, section 5).

## 17. Implementation Workstreams and Definition of Done

The scope is intentionally large; it is delivered as one issue per workstream
under this RFC's epic #182 (#219 protocol, #220 worker, #221–#226 daemon
slices, #227–#228 adapters, #229 CLI and skill, #230 SDKs and clients, #231
MCP adapter, #232 validation), each with its own DoD, rather than a single
landing.
The order below is dependency order, and workstream 7 (the MCP adapter) is
deliberately last — it may move to a follow-up issue per open question 1
without blocking the rest.

1. **Protocol:** task types, methods, events, errors, limits as configuration;
   request ids and request fingerprints on every mutating task method (new:
   `session.new` and `session.input` carry none today); Rust and generated
   TypeScript; wire-shape tests.
2. **Worker protocol:** turn cursor capture under the input lock; consumption
   milestone capture; delivery ids with `writing`/`written` journal records
   and a delivery-state query (section 8.8); a hook-evidence endpoint on the
   worker socket, the sequenced evidence journal, forwarding frames with
   acknowledgement and resume-after-sequence on reconnect (section 9.2);
   secondary child supervision for the OpenCode server (section 9.3).
3. **Daemon:** a typed connection origin recorded at accept time and
   passed into `serve_connection` for the Unix and overlay listeners
   (`crates/daemon/src/api/mod.rs:268`, `:399`, `:426`), the persisted
   request-key and fingerprint index; the per-task directory store on
   `host_state/persistence.rs`-style per-record files (section 14); evidence
   ingestion from the worker journal and offset-keyed detection evidence
   with `reconstructed` replay transitions (section 8.2); task store with
   lifecycle state and `run_id`,
   settlement engine with consumption milestones, evidence priority, hook
   correlation, the stop grace window and heuristic re-open with settlement
   revisions, pending-decision vs confirmed attention with attention ids and
   atomic stale-answer refusal, the delivery commit protocol and its
   recovery, a separate task waiter pool with peer-hangup release,
   `worktree_of` handoff (including into ended owner tasks) with worktree
   occupancy covering every writing phase, worktree users, retain holds and
   the `worktree_shared` retention hold, queued `task.continue` during
   re-open windows, `task.wait` `until: "final"` with per-socket keepalive, late
   completion of `timed_out` turns, bounded
   drift fingerprints and shadow-repository snapshots for `turn_delta` and
   per-turn diffs, the `finalizing` phase with persisted check processes,
   restart recovery and the finalization deadline, the `checks.allow_in_repo`
   gate, host-over-repo precedence and base-commit reads for in-repo check
   definitions, owner-private check log capture, check-name authorization,
   result identity and review binding,
   paged `task.list`/`task.inspect`, notification dedupe, content and
   metadata retention.
4. **Adapters:** OpenCode adapter (launch, authenticated loopback server
   child on an ephemeral port with stdout discarded after the listen line,
   fork through the server API, event mapping, capabilities, manifest,
   compatibility lock and goldens); Claude and Codex `UserPromptSubmit`
   digest hooks with pinned prompt canonicalization, a Claude
   `PermissionRequest` hook, and task-evidence hooks for `Stop`/`StopFailure`/
   `Notification`/`PermissionRequest` that report to the worker socket
   (section 9.2) alongside the existing notify hooks, mapped to turn
   evidence by `prompt_id`/`turn_id`; Claude and Codex compatibility locks pinning the
   verified payload fields; Hermes plugin evidence.
5. **CLI and skill:** `task` command group with `--wait`, JSON envelopes,
   completion; skill section; knowledge bundle update.
6. **SDKs and clients:** Rust client and TS SDK methods; `gui-core` and web
   task views (outcome, attention, review) without embedding a terminal.
7. **MCP adapter** (if accepted in scope).
8. **Validation:**
   - unit and integration tests for every outcome, including causal settlement
     (an already-idle screen must not settle a new turn; pre-consumption
     evidence must not settle a turn; `task.continue` after `timed_out` must
     not settle on the previous turn's late completion; a `Stop` whose
     `prompt_id`/`turn_id` differs from the bound one is discarded; a `Stop`
     followed by resumed work or a `stop_hook_active` continuation inside
     the grace window does not settle; a Claude `Stop` with pending
     `background_tasks` does not settle; `SubagentStop` never settles),
     OpenCode server refusal of unauthenticated `/api` requests and the
     adapter's refusal to launch when it is not refused; heuristic
     finality: a parallel owner `Stop` hook that blocks **after** the grace
     window re-opens the turn as a new revision, supersedes the result,
     stales its reviews and keeps the worktree occupied, and work resuming
     after the re-open window marks the next result suspect; attention: a
     `PermissionRequest` auto-resolved by another hook never becomes an
     attention, a delayed answer to attention 1 is refused once attention 2
     is pending, an attention resolved in the terminal refuses later answers;
     occupancy: `worktree_of` refused during `timed_out` with the agent still
     working, during `finalizing`, during a baseline, and `task.extend`
     refused while another task occupies the tree; delivery commit: crashes
     before dispatch, after `writing`, after `written` before the
     acknowledgement, and across a lease change; finalization: daemon restart
     mid-check with and without worktree changes, finalization deadline
     across restarts, `task.wait` and CLI `--wait` spanning the turn deadline
     into finalization; snapshots: per-turn diff reproduced after a later
     round overwrote and deleted files, and after a history rewrite plus
     `git gc --prune=now` in the project repository, truncation,
     retirement; answers: an addressed answer to a request the provider
     already resolved is rejected, a keystroke answer is refused without
     `allow_unverified_delivery`; re-open window: `task.continue` during it
     returns `{ queued: true }` at once, is delivered when the window
     closes, or settles `cancelled` (`task_turn_reopened`) when the previous
     turn re-opens and (`task_stopped`) on `task.stop`; a second
     `task.continue` while one is queued fails with `task_turn_open`;
     `task.wait` on a queued turn waits as on an open one; `task.extend` on a
     queued turn fails with `task_turn_queued`; a queued turn settles
     `cancelled` with `session_ended` when the session ends before delivery;
     an executor `worktree_of` naming an investigate-mode task fails with
     `task_worktree_via_investigate` while an investigate start naming it
     succeeds; the daemon down
     across a heuristic completion's windows restarts them at reconnect with
     `window_overlapped_outage`; evidence records carry the output offset
     used to order replayed detection; and the next
     turn's working transition never re-opens the previous turn;
     `task.wait` with the default `until: "final"` returns once for a
     heuristic result that survives its window and returns the new revision
     when it does not; late completion: a correlated `Stop` after
     `timed_out` revises the turn to `completed` with its `turn_delta`;
     re-open during `finalizing` cancels and joins the checks and supersedes
     the unpublished result; daemon outage: a `Stop` emitted while the daemon
     is down is journaled by the worker and settles the turn after
     reconnect, replayed output never creates a first post-consumption
     `working`, and a journal overflow marks the turn `evidence_degraded`;
     a daemon crash after receiving but before acknowledging an evidence
     record replays and applies it exactly once; an OpenCode server child
     exit ends the generation and settles the open turn `lost`;
     addressed-answer recovery before, during and after the provider
     request; result lists over their caps publish with `truncated` and
     totals and never block occupancy; provider-reported text never appears
     in events, notifications or metadata; write fence: `session.input` and
     a controlling attach into a non-occupying user task's session are
     refused while another task occupies the tree, and a provider resuming
     there marks results suspect; check launch: a crash between spawn and
     identity commit leaves a stopped child that recovery kills by token;
     task operation journal crash-injection at every boundary; evidence
     arbitration: a detection `idle` never settles a hook agent while its
     hooks are healthy; `attention` → `stopped` through `task.stop` as a new
     revision; `task_store_full` on every cap; the generated `tasks/<id>`
     branch and refusal of an empty branch; keyed prompt fingerprints never
     equal to a plain digest in any persisted record;
     check definitions: a host definition shadows an in-repo one of the same
     name, an in-repo definition edited in the worktree during the turn is
     not what runs, and `repo_base` checks cap integrity at `suspect`;
     retain hold: `worktree_of` into an `ended` owner task with a retained
     worktree, `task.retain_worktree` after start naming a non-owner chain
     member and holding a clean, fully pushed tree, release, and hold expiry
     never deleting unsaved work; digest
     canonicalization goldens per provider; overlay wait slot released by a
     keepalive failure; `task.stop` on an ended task with stale preconditions
     succeeds as a no-op; stop barrier: `task.stop` and cascade removal during a
     running check wait for the process group to exit before occupancy is
     released or the tree removed, and a stuck group blocks removal;
     cascade removal: `worktree_users_changed` on a stale list, concurrent
     `worktree_of` refused while `removal_pending`; resubmission: restart
     after `prepared` yields `awaiting_resubmission`, a matching
     resubmission dispatches, a mismatching one is refused, expiry yields
     `delivery_abandoned`; `task.stop` preconditions; review
     binding: stale on supersede and on fingerprint change; paging: stable
     order, no duplicates under concurrent inserts and retention, byte
     limit, cursor expiry; worktree hold: sweep keeps a clean shared
     worktree, `session.remove` refusal and `stop_worktree_users`; in-repo
     check gate default-off; idempotent
     delivery including `task.answer` and cross-origin key isolation,
     deadline re-open and the open-time ceiling, attention round-trip
     including degraded attention, steering by attach and by `session.input`,
     `worktree_of` (auditor sees executor state, busy-worktree refusal,
     origin-guard refusal), `turn_delta`, drift, modification during checks,
     check-name refusal, waiter pool exhaustion and hangup release, metadata
     retention after session removal, and metrics completeness including
     subagents;
   - model-free provider goldens for OpenCode, Claude, Codex and Hermes
     evidence;
   - a delegation benchmark matching section 2.1 (same tasks, hidden tests,
     three repetitions) run against `task run --wait`, with two low-cost
     worker profiles served through OpenCode 2.x (the exact models are
     recorded on the epic issue at run time), plus the arm A baseline
     (orchestrator working alone) re-run in the same conditions, recording
     cost, wall time, orchestrator tool calls and worker turns per task per
     arm. The benchmark is an operator-run activity: it spends real
     provider credits with the operator's own credentials, its evidence is
     recorded on the epic issue, and it is never a CI gate — CI covers only
     the model-free goldens and settlement tests above.

Definition of done: all gates in `AGENTS.md` pass; the benchmark shows at most
two orchestrator tool calls per turn on the common path and records cost, wall
time, orchestrator tool calls and worker turns per task per arm (a cheap worker
must not hide round inflation); the delegating arm's total cost is reported
against arm A with the same acceptance quality, and the epic issue records
whether delegation paid off (a result where it does not is recorded, not
hidden); every settlement in the benchmark is attributed (`settled_by`
present, no detection-only settlement for OpenCode, and the share of
`provider_hooks_uncorrelated` settlements reported for Claude and Codex).

## 18. Alternatives Considered

- **Keep polling, raise the `session.wait` cap.** Removes some turns but keeps
  settlement non-causal and results screen-based. Rejected.
- **Non-interactive provider runs (`opencode run`, `codex exec`) per task.**
  What the benchmarked external server does. It gives clean structured output
  but discards durability, attach, human takeover, and the PTY-first
  constraint. Rejected.
- **Daemon-side MCP server.** Makes the daemon depend on one client protocol
  and duplicates host targeting and authorization. Rejected in favour of the
  CLI-side adapter.
- **OpenCode TUI with `--standalone` plus a reporting plugin.** Avoids a
  listening port, but needs installation and upgrades per profile, a
  reporting channel from inside the provider, and plugin-mediated
  request-addressed answers; none of it was verified. Rejected in favour of
  the two-child design of section 9.3.

## 19. Open Questions

1. Whether the MCP adapter ships in the same milestone as the task layer.
2. Default and ceiling values for `tasks.max_wait_ms`, `tasks.turn_deadline`,
   `tasks.turn_open_ceiling_ms`, `tasks.stop_settle_grace_ms`,
   `tasks.heuristic_reopen_window_ms`, `tasks.attention_confirm_ms`,
   `tasks.finalize_max_ms`, `tasks.snapshot_max_bytes`,
   `tasks.snapshot_max_file_bytes`, `tasks.page_max_items`,
   `tasks.page_max_bytes`, `tasks.cursor_ttl_ms`, `tasks.check_kill_grace_ms`,
   `tasks.resubmit_window_ms`, `tasks.wait_keepalive_idle_ms`,
   `tasks.wait_keepalive_interval_ms`, `tasks.wait_keepalive_count`,
   `tasks.store_max_bytes`, `tasks.max_turns_per_task`,
   `tasks.max_tasks_retained`,
   `tasks.worktree_hold_max_age`, `tasks.record_max_bytes`,
   `tasks.check_log_max_bytes`, `tasks.result_max_files`,
   `tasks.result_max_commands`, `tasks.result_max_mismatches`,
   `tasks.attention_max_choices`, `tasks.max_checks_per_turn`,
   `tasks.max_waiters` and
   `tasks.metadata_retention`, and whether per-profile overrides may exceed
   the host ceiling. All are required configuration validated at startup.
3. Whether investigation mode should refuse to start when a profile declares
   only `provider_rules` enforcement for an untrusted project.

Resolved elsewhere: relay-path `task.answer` authority and relay delegation
budget scopes are defined by the relay dark factory RFC (sections 7.3 and
8.1); review verdict retention is `tasks.metadata_retention` (section 14).
Codex exposes `UserPromptSubmit` with `turn_id` (section 9.2), so Codex
settlement is correlated like Claude's.

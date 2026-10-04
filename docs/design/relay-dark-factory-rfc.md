# RFC: Relay Dark Factory — Unattended Manager/Auditor Delegation

- **Status:** Accepted 2026-09-29 (epic #185; implementation tracked by its
  sub-issues). Revised after a code-grounded review on 2026-09-25 and four
  review rounds on 2026-09-28. Amended 2026-10-04 (#510): usage-limit
  attentions and multi-target waits in the factory loop.
- **Date:** 2026-09-24
- **Scope:** The relay-side authorization, budget, audit, projection and
  escalation changes needed to run unattended manager/auditor delegation loops
  ("dark factory") over the team relay, plus the client contract such a factory
  must follow. Extends the delegated task runs RFC and amends named contracts
  of the accepted team relay RFC (section 3); revises neither's hard
  constraints.
- **Audience:** maintainers of `pohunek-relayd`, `pohunekd`, the protocol, the
  Rust/TypeScript clients, and the embedded agent skill

## 1. Summary

The delegated task runs RFC defines `task.*`: bounded delegation turns over
ordinary durable sessions, causal settlement, typed results, and (section 16)
the Manage-Execute-Audit composition in which a manager orchestrates tasks, a
fresh-context executor performs them, and a read-only auditor verifies the
environment. The team relay RFC defines identity, teams, `HostShare` ACLs,
service accounts, audit and quotas for routing that same work across hosts.

What neither document defines is the surface an **unattended** factory needs:
which relay permissions govern `task.*`, how a service account acts as a
manager or auditor, how runaway delegation is bounded before admission, how
approval questions reach a human, and what happens to open work when any piece
fails. This RFC defines those. The result is a dark factory — the daemon stays
the chassis, the relay stays the control plane, and the factory is a client
that composes both:

```text
manager (client)  --task.start/task.continue-->  relay ACL + budget admission
    |                                                  |
    |                                          host link (v4)
    v                                                  v
task state (client-side)  <--task.wait/result--  daemon task layer  -->  PTY worker
    ^                                                  |
    |                                          settled, attributable
auditor (client, investigate-mode task or checks)      |      result
    |                                                  v
task.review verdict  ----------------------->  relay audit + catalog
```

Humans enter at exactly two points: `attention` escalations (answering or
approving) and `task.review` verdicts. A machine answers an `attention`
through `task.answer` only where a team administrator explicitly granted it
that right and the host owner opted the share in (section 7.3). Typing an
approval into the terminal needs terminal control, which a service account
receives only through an equally explicit grant (section 7.1). Everything else runs lights-out,
bounded by budgets that fail closed and never stop work already running.

## 2. Motivation

### 2.1 What the composition already gives

The delegated task runs RFC makes delegation cheap (two calls per turn),
causal (consumption-scoped settlement), and evidential (claims vs. verified
facts, checks, drift, integrity). Its section 16 maps the Manage-Execute-Audit
loop onto existing primitives and notes the relay as the natural team-facing
home. That is sufficient for an interactive orchestrator.

### 2.2 What is missing for lights-out operation

An unattended factory exercises every relay subsystem at once, and the
accepted relay RFC has no answers for:

- **Authorization**: session ACL classes (relay RFC section 14) say nothing
  about `task.*`. Who may delegate? Who may answer an approval as a machine?
- **Budgets**: relay quotas (section 18) bound transport resources, not
  delegation volume. A manager loop with a bug can start thousands of tasks;
  nothing stops it before admission.
- **Attribution**: a factory acts through service accounts for days. Audit
  records (section 17) must make every delegation step reconstructible without
  storing prompt or result content.
- **Escalation**: `attention` settles a turn, but nobody has defined how a
  question from a headless worker reaches the owning human and under whose
  authority it is answered.
- **Recovery**: a factory manager holds task state in its head (context). What
  survives its crash, a relay outage, or a host loss — and who cleans up?

Each gap is small; together they decide whether unattended delegation is a
supported operating mode or an accident.

## 3. Relationship to the Existing Designs

- **The accepted relay RFC keeps its boundaries; this RFC amends named
  contracts inside them.** Component boundaries (section 7), the trust model
  (section 8), identity (section 13) and the sync algorithm (section 16.2)
  are unchanged. Accepting this RFC does amend these accepted contracts, and
  each amendment is owed to the relay RFC text and its issue map (section
  23) and is tracked by #233; until #233 lands, the relay RFC is stale on
  exactly these points and this document is the newer decision:
  - `HostShare` policy (relay RFC section 12.2) gains `task.*` operation
    classes, the `allow_delegated_answers` capability and a permitted-check
    list (section 7.1);
  - relay permission classes (relay RFC section 14) gain the `task.*`
    classes and the `session.task.control` session class of section 7.1,
    and service-account creators no longer receive
    `session.terminal.control` by default (section 7.1);
  - `HostShare` policy additionally gains `allow_unverified_answers`
    (section 7.3); both answer capabilities are **new policy content**, not
    entries of the existing operation-class list;
  - the session projection row (relay RFC section 16) gains bounded task
    fields (section 10), which re-opens the snapshot budgets of relay RFC
    section 18.1 for re-measurement;
  - the data classification table (relay RFC section 17.2) gains the rows of
    section 9;
  - catalog retirement (relay RFC section 17.3) is deferred for a session
    row projecting `retain_hold: true` (section 7.1);
  - the v4 host link gains an idempotent `operation.cancel { request_id }`
    frame for routed waits (task RFC section 8.3) and the relay methods
    `factory.run.open`, `factory.run.add_member`, `factory.run.reconcile`
    and `factory.run.failover` (section 12.2);
  - the operation-ticket state machine (relay RFC section 12.5) gains a
    repeatable `awaiting_resubmission` → `begun` transition for a
    matching-fingerprint `begin`, bounded by the host's resubmission window
    and attempt cap and never after `writing`, expiry or abandonment (task
    RFC section 8.8), with ticket tests for a first and a second crash.
- **The delegated task runs RFC is the task substrate.** All settlement,
  causality, result and attention semantics come from there; this RFC never
  redefines them. Section 16 of that RFC is the composition this factory
  implements.
- **PTY/TUI-first is untouched.** Factory executors are ordinary task
  sessions; a human can attach and take over at any time (task RFC section 8.6).
- **Direct owner operation stays first-class.** Everything here is relay-path
  only. Local Unix and NetBird owner operation gains no factory dependency,
  and host owner paths always outrank the factory (relay RFC section 12.3).
- **Isolation claims stay honest.** Factory roles are authorization controls,
  not workload isolation. Direct-host execution remains at the host
  Unix-account trust boundary; [#88](https://github.com/zajca/pohunek/issues/88)
  owns the later container/VM boundary.

## 4. Goals

1. Typed relay authorization for every `task.*` method, composable into
   manager and auditor roles for service accounts.
2. Delegation budgets that fail closed at admission and never stop running
   work.
3. Full attribution of unattended delegation through audit records that carry
   no prompt, result, or terminal content.
4. A defined escalation path from `attention` to a human, and a defined
   `task.answer` authority that no principal can grant itself: a machine
   answers through `task.answer` only with an explicit, audited
   team-administrator grant on a share whose host owner opted in, and never
   through the terminal unless it holds an equally explicit terminal-control
   grant (section 7.1).
5. Bounded task projections in relay state sync so factory clients can list
   and route without reading content.
6. A client contract (loop, recovery, stop conditions) that keeps orchestration
   out of the daemon and the relay runtime.
7. Failure semantics for every unattended-mode failure, with running host work
   never collateral damage.

## 5. Non-goals

- A manager, auditor, planner or task-state store inside `pohunek-relayd` or
  `pohunekd` (task RFC section 16.4).
- Persisting prompts, results, check logs, final messages or terminal content
  at the relay (relay RFC section 17.2).
- Automatic answering or approval of `attention` prompts by timer, by
  default, or by any implicit policy. The relay itself never answers. A
  service account answers through `task.answer` only as an explicitly
  granted principal (section 7.3), and each answer is its own attributable,
  audited action.
- Preventing a **human** holder of `session.terminal.control` from
  approving prompts by typing into the terminal.
- Scheduling, queuing, or load-balancing decisions beyond admission control;
  the factory chooses its own work.
- Workload isolation claims beyond the honest boundary of
  [#88](https://github.com/zajca/pohunek/issues/88).
- Replacing the interactive orchestrator experience; humans delegating by hand
  keep using `task.*` directly.

## 6. Terms

### 6.1 Factory

A client-side Manage-Execute-Audit loop (task RFC section 16) run by one or
more **factory principals** — relay service accounts acting as manager and
auditor — over `task.*` routed by the relay. "Dark" means unattended: no human
in the loop except `attention` and `task.review`, and `attention` answered by
a machine only under an explicit grant (section 7.3).

### 6.2 Run

One factory objective: a bounded sequence of task rounds (fresh-context tasks
on a shared worktree via `worktree_of`, or turns within a task) toward one
goal, with a budget and stop conditions. A run is a client-side record; the
relay and the daemon know only tasks and the run's opaque `run_id`
(task RFC section 13.1), which groups them without making either an
orchestration authority.

### 6.3 Manager principal / auditor principal

Service accounts (relay RFC section 13.4) holding the `FactoryManager` or
`FactoryAuditor` role templates (section 7.2). A manager may execute and
review; an auditor may read evidence, start investigate-mode tasks, and
review. Both are ordinary relay principals: revocable, expiring, audited.

### 6.4 Budget

A per-period admission limit on delegation volume (section 8). Distinct from
relay transport quotas (relay RFC section 18), which continue to bound
resources independently.

### 6.5 Escalation

The delivery of an `attention` notification to a human principal, and that
human's subsequent `task.answer` (or decision to leave the turn unanswered).

## 7. Authorization Surface

### 7.1 Task permission classes

Mirroring the session classes of relay RFC section 14, `task.*` methods map to
six permission classes. A task class is never sufficient on its own: every
call on an existing task also requires the listed session class on that
task's session, because task data is session data in another shape (a final
message or command list reveals what terminal observation would).

This RFC adds one session class to relay RFC section 14:
**`session.task.control`** — deliver task-layer input to a task session
(`task.continue`, `task.extend`, `task.answer`). It is deliberately narrower
than `session.terminal.control`: the daemon writes only a task prompt or a
validated answer to a named attention, refuses prompt delivery while an
attention is pending (task RFC invariant 2), and never lets the caller write
arbitrary bytes. Task-layer delivery is therefore not terminal input and does
not require terminal control.

| Class | Covers | Also requires on the task's session |
| --- | --- | --- |
| `task.metadata.read` | `task.inspect`, `task.list` | `session.metadata.read` |
| `task.execute` | `task.start`, `task.continue`, `task.extend` | `session.task.control` (for `task.continue`/`task.extend`; the creator receives it by default) |
| `task.investigate` | `task.start` with `mode: "investigate"` only | — (on the new task); see `worktree_of` below |
| `task.evidence.read` | `task.wait`, `task.result`, `task.check_log`, `session.diff` with `turn` | `session.terminal.observe` |
| `task.answer` | `task.answer` | `session.task.control` |
| `task.review` | `task.review` | `session.metadata.read` |

`task.stop` stops a session, so it requires `session.lifecycle.control` on
the task's session and either `task.execute` or `task.investigate` (the
latter only for investigate-mode tasks). A principal with `task.execute` but
without `session.lifecycle.control` on a session may delegate into it but not
stop it. `task.retain_worktree` and `task.release_worktree` require
`session.lifecycle.control` on the owner task's session: the call may name
any member of the `worktree_of` chain (task RFC section 8.7), the relay
resolves the owner task from the named member through the task projection
(section 10) and authorizes on that owner session. This is the one call of
the `worktree_of` family checked on the chain root rather than on the named
task, because the hold is a property of the owner task, not of the member.

**Service-account creator defaults.** Relay RFC section 14 gives a session's
creator full relay-side session permissions. This RFC amends that for
service accounts: a service account that creates a session receives every
creator permission **except `session.terminal.control`** (and therefore no
attach and no `session.input`). Terminal control, which can type an approval
into the agent, reaches a service account only through an explicit grant
treated like a machine `task.answer` grant: named account, share-scoped,
mandatory expiry, created by a team administrator as an audited action, with
the setup flow stating that it confers approval authority. Human creators
keep the full default.

`task.start { worktree_of: <task> }` (task RFC section 8.7) places a new
agent in another task's tree, so it additionally requires, on the session of
the task **the caller named** — not on the root owner task the daemon
resolves the chain to: `session.lifecycle.control` for an executor start
(the authority to decide what runs in that tree), and
`session.terminal.observe` for an investigate-mode start. Every task in a
`worktree_of` chain shares one tree, so authority over a member of the chain
is authority over that tree — with one exception that keeps read access from
becoming write access: an executor start must name a member that is **not**
in investigate mode, so lifecycle control over an investigate task (which
any caller with observe rights can create) never authorizes a writing
executor. The daemon enforces this independently of the relay
(`task_worktree_via_investigate`, task RFC section 8.7). In the reference loop the auditor names the executor task it audits,
on which it already holds `session.terminal.observe` (section 7.2), and the
manager names its previous round's executor or the owner task, both of which
it created. The investigate
case is authorization, not isolation: investigation enforcement is provider
rules plus an optional read-only mount, and a modification is reported as a
`violation` (task RFC section 11.2), not prevented by the relay.

`task.list` returns only tasks whose sessions pass the same filter, and
results are identical in shape for "absent" and "not permitted", matching
relay RFC section 14's identical-filtering rule. For a task whose session has
ended or was removed on the host, the relay authorizes against the ACL of the
retained catalog entry (relay RFC section 17.3); once that entry retires,
relay-path access to the task's metadata ends even if the host still retains
it under `tasks.metadata_retention`. One exception keeps holds releasable:
the owner task's session row projects `retain_hold: true` while a retain
hold is set (section 10), and catalog retirement of that row is deferred
for as long as the hold exists (an amendment to relay RFC section 17.3
listed in section 3), so `task.release_worktree` can always be authorized
over the relay before the host's `tasks.worktree_hold_max_age` lapses.

`task.*` classes grant nothing over session sharing: changing a task
session's ACL still requires `session.share.manage`, and removing it
`session.remove` (relay RFC section 14).

The relay enforces these classes on every routed call, **authorization
before budget**: a call the caller is not authorized for is refused with the
ordinary authorization error and never consumes budget or creates an
admission record; only an authorized call is then checked against the
budgets of section 8; the daemon enforces
its own `HostShare` operation-class intersection (relay RFC section 12.2)
with `task.*` added to the allowed operation classes a share may permit. A
share that does not permit task operations refuses them regardless of relay
role. The share policy also lists the check names relay-path callers may
request (task RFC section 12); any other name is refused by the daemon with
`task_check_not_permitted`. Entries are matched against the definition that
actually resolves (host definitions shadow repository ones, task RFC section
12). A plain entry (`unit`) permits only a **host** definition of that name;
if the host defines none, the name is refused rather than resolved to a
repository definition. A `repo:<name>` entry permits only a `repo_base`
definition; when a host definition shadows the name, `repo:<name>` does not
permit it and the plain entry is required. A host owner therefore opts in to
repository-defined commands by name, and never permits a host command by
accident.

`session.remove { stop_worktree_users }` (task RFC section 8.7) stops other
principals' tasks indirectly, so the relay authorizes it as the sum of its
effects: `session.remove` on the owner session **and**
`session.lifecycle.control` on the session of every task in
`expected_worktree_users`. If the caller lacks either for any of them, the
relay refuses the whole call with `worktree_in_use` before forwarding, and
the error lists only the tasks the caller holds `task.metadata.read` on,
plus a count of the others.

Relay-path `task.*` mutations are relay mutations in the sense of the
accepted relay RFC and therefore use its **operation tickets**
(`operation.ticket.issue` / `operation.begin`, relay RFC section 12.5), not
caller-chosen keys forwarded to the host (task RFC invariant 3). A client
supplies its own idempotency key to the relay; the relay keys it by
`(team, principal, key)` and binds it to exactly one ticket, which the
daemon issued for one host, share revision and method class. Two principals,
or two targets, can never share a ticket.

### 7.2 Built-in role templates

| Role | Classes | Intent |
| --- | --- | --- |
| `FactoryManager` | `task.metadata.read`, `task.execute`, `task.evidence.read`, `task.review` | Orchestrates runs; never answers prompts. |
| `FactoryAuditor` | `task.metadata.read`, `task.investigate`, `task.evidence.read`, `task.review` | Read-only verification: the auditor role of the Manage-Execute-Audit loop (task RFC section 16.1) as an ACL. It can run investigate-mode tasks but never an executor. |
| `FactoryOperator` | `FactoryManager` plus `task.answer` | Interactive convenience for humans. The template itself is human-only: the grant machinery refuses to assign `FactoryOperator` to a service account. |

The session-class requirements of section 7.1 still apply to each role; a
role template grants task classes, and the session grants come from session
creation (the creator's defaults, without terminal control for service
accounts), explicit session grants, or team `Owner`/`Admin` authority
(relay RFC section 14). An auditor service account auditing tasks another
principal created therefore also needs `session.terminal.observe` **and**
`session.metadata.read` on those sessions (for `task.inspect`, `task.list`
and the mandatory `task.review`), granted explicitly and narrowed to the factory's host shares or
projects (relay RFC section 13.5 grant scoping); the role template does not
imply it. Team `Owner`/`Admin` keep the session authority relay RFC section
14 gives them. Custom roles may compose the same classes, with one exception
the authorizer enforces by **grant provenance**: for a service account,
`task.answer` and `session.terminal.control` are honoured only from a direct
grant to that account, scoped to a named `HostShareId`, with an unexpired
expiry. Built-in roles, custom roles, group grants, team-wide grants and
wildcard scopes never confer either to a service account, whatever classes
they list; tests cover each of those paths.

**Machine answers.** A service account can hold `task.answer` only through a
dedicated custom grant that names the service account, is scoped to specific
host shares (never team-wide), carries a mandatory expiry, and is created by
a team administrator as an audited action. No role template carries it for
service accounts, and a principal can never grant it to itself (invariant 6).
The grant is exercisable only on shares whose `allow_delegated_answers`
capability the host owner approved (section 7.3); the default denies both.
Every answer by a service account is audited as that account's own action,
distinct from the factory's turns, and is visible to the escalation targets
of section 11.

### 7.3 Answer authority

`task.answer` on the relay path requires **all three** of:

1. the relay `task.answer` class on the acting principal — the relay decides
   *who* may answer (human, service account, group) from its own identity
   model;
2. the `HostShare` policy permitting delegated answers on the target host — a
   per-share capability (`allow_delegated_answers`) approved by the local
   host operator when the share is accepted. It is **new share policy
   content** added by this RFC (section 3), stored beside the operation
   classes of relay RFC section 12.2 rather than as one of them. The daemon
   decides *whether this
   share* may be answered over the relay at all; it never learns the relay
   principal's class, consistent with "the daemon does not inspect user,
   group, role, or session ACL claims";
3. the origin-session guard (task RFC invariant 8).

No single layer can widen the answer right: the relay cannot force an answer
onto a share whose owner did not opt in, and a host owner's opt-in authorizes
no principal the relay has not granted. The relay never answers, prompts never
auto-resolve, and a revoked grant cancels affected in-flight answer
attempts promptly under relay RFC section 13.3 cancellation semantics.

Two consequences follow from the daemon never learning the principal, and
host owners must be told both when approving the capability:

- `allow_delegated_answers` opts the share in for **every** relay principal
  the relay grants `task.answer`, human or service account. A host owner who
  wants answers on the relay path but never by a machine has no daemon-side
  way to express that; they rely on the team administrators' grants. The
  share-approval UI states this in plain words.
- Without the capability, nobody answers over the relay path **through
  `task.answer`**, including humans. Attention on such a share is resolved
  through the owner path, or through a relay attach by a principal holding
  `session.terminal.control` (task RFC section 8.6). Section 11 routes
  escalations accordingly.

**Machine approval is closed on both paths.** The rules above govern
`task.answer`. The other way to approve a prompt is typing into the
terminal, which requires `session.terminal.control`. Because service
accounts no longer receive terminal control as creators (section 7.1) and
task-layer delivery needs only `session.task.control`, a `FactoryManager` or
`FactoryAuditor` service account has **no** path to approve a provider
prompt unless a team administrator granted it either a machine `task.answer`
grant or a terminal-control grant — both named, share-scoped, expiring and
audited. The three-layer `task.answer` authority is therefore the effective
control for factory machines, not a decorative one. Humans holding terminal
control can still answer in the terminal; such answers are audited as
terminal control (relay RFC section 17.1) and recorded by the task layer as
a terminal resolution (`resolved_elsewhere`, task RFC section 8.5) on a
`steered` turn (task RFC invariant 10).

**Unverified answers on the relay path.** For Claude, Codex and other
keystroke-answered agents, `task.answer` is refused unless the request sets
`allow_unverified_delivery` (task RFC section 8.5), because the daemon cannot
prove the keys reach the intended question. On the relay path the flag is a
separately authorized capability, not a free parameter:

- the share policy must carry `allow_unverified_answers` in addition to
  `allow_delegated_answers` (host owner opt-in, default deny);
- a human principal needs nothing further; a service account's `task.answer`
  grant must explicitly include `unverified` (default: provider-addressed
  answers only);
- the flag is part of the request fingerprint (section 8.2), and the audit
  record states the answer's `answer_verification` (section 9).

Without that capability a factory can answer OpenCode prompts (addressed
answers) but not Claude or Codex prompts, whose attentions then escalate to
humans (section 11). Operators choose that trade-off explicitly per share
and per grant.

`task.answer` requests carry the task RFC's attention identity (`turn`,
`attention_id`, `settlement_revision`); the relay forwards them unchanged and
the daemon refuses stale answers (task RFC section 8.5).

The daemon-side `tasks.answer_policy` of task RFC section 11.3 governs only
callers the daemon can identify — the local owner's clients — and is unchanged
by this RFC.

## 8. Budgets and Quotas

### 8.1 Budget model

Delegation budgets are configured per team, with optional narrower scopes
per service account and per host share. A call is admitted only if **every**
applicable scope (team, the acting service account if any, the target share)
has headroom, and one admission consumes from all of them in the same
transaction. Three hard counters and one advisory:

| Counter | Unit | Admission effect |
| --- | --- | --- |
| `max_active_tasks` | concurrent tasks in lifecycle state `active` (task RFC section 6.1) | `task.start` refused when full. |
| `task_starts_per_period` | `task.start` per period | Refused when exhausted. |
| `turns_per_period` | turn 1 of every `task.start` + `task.continue` + `task.extend` + service-account `task.answer` per period | Refused when exhausted. |
| `cost_ceiling_per_period` | advisory; summed from reported `metrics.cost` | Reported, never enforced on `unknown` metrics (task RFC invariant 6). |

`task.extend` is counted because each extension buys another deadline window
(task RFC section 6.3); the daemon's `tasks.turn_open_ceiling_ms` bounds a
single turn, and this counter bounds how often a factory buys time across
turns. A provider continuing a turn by itself after a usage limit (task RFC
section 8.5) is not an admission and consumes no counter: no principal
opened anything, and the re-opened turn is the same turn.

`max_active_tasks` is computed from durable admission records (section 8.3),
not from the projection alone, because a start the host refused, or one whose
response was lost, never appears in the projection at all.

Periods are fixed, non-overlapping windows aligned to a configured start time
(for example, 24-hour windows starting at 00:00 UTC). Rolling windows are not
used: fixed windows give the typed refusal an exact reset time.
All values are **PROPOSED UNMEASURED** until the unattended benchmark
(section 15) records real factory consumption, in the same spirit as relay RFC
section 18.1.

### 8.2 Fail-closed semantics

- Counters consume at **admission, not success**: a `task.start` refused by
  the host (share policy, profile validation) still consumes its start counter
  and one turn of `turns_per_period`, so retry storms cannot farm free
  attempts. There is no refund path for periodic counters. The concurrency
  slot of `max_active_tasks` is different: it is capacity, not consumption,
  and is released by the admission record's lifecycle (section 8.3).
- Admission is per **logical operation**: one admission record, one
  operation ticket, one **request fingerprint**. The relay computes the
  fingerprint over the canonical request — method, target host and share,
  `task_id` (for non-start methods), turn and attention identity (for
  `task.answer`), every parameter except `run_generation`, and the payload
  digest — and stores it on the admission record; `run_generation` is fence
  input, not request identity. A retry with the same client key **and the same
  fingerprint** consumes nothing, is never refused by exhaustion, and is
  resumed through the same ticket, so a lost response cannot make one
  logical start cost twice or be refused after it is already running. The
  same key with a **different** fingerprint is refused at the relay with
  `task_request_conflict` before anything is forwarded; it never receives
  the retry exemption and never reaches the host as a "new" operation. The
  daemon independently rejects a changed payload under the same ticket
  (`ticket_payload_mismatch`, relay RFC section 12.5). A new key is a new
  operation and is budgeted as such, even if its parameters equal an
  earlier failed attempt. `task.start` consumes one start **and**
  one turn (turn 1); `task.continue`, `task.extend` and a service account's
  `task.answer` consume one turn each. A human's `task.answer` is never
  budgeted: exhaustion must not leave a question that a person is ready to
  answer stuck.
- Exhaustion refuses **admission only**, with a typed
  `factory_budget_exhausted` error carrying the exhausted counter, its window
  reset, and a recovery hint. It never stops, pauses, or settles running turns
  and never touches sessions or worktrees.
- Read paths (`task.evidence.read`, `task.metadata.read`) are never budgeted:
  a factory that cannot start work can always observe and escalate.
- Owner paths are never budgeted. A host owner delegating by hand over the
  local Unix socket or direct NetBird cannot be locked out by factory
  consumption. Relay-path calls are budgeted whoever makes them, including a
  host owner acting through the relay API (relay RFC section 14); the owner
  path is always available to them instead.
- `task.stop` is never budgeted: stopping work must always be possible.
- Budget state is durable at the relay (PostgreSQL, relay RFC section 15.2) so
  a relay restart cannot reset a window mid-period; the admission transaction
  is atomic with the audit record (relay RFC section 17.1: sensitive work is
  not admitted if its audit cannot be recorded). A **database restore** is
  not a restart: budget counters, admission records and the idempotency-key
  → ticket bindings belong to the recovery manifest of relay RFC section
  19.1, and after a restore the relay stays in restore quarantine for
  factory admission — no `task.start`, `task.continue`, `task.extend` or
  service-account `task.answer` is admitted — until every host's fresh
  snapshot has been reconciled against the restored records (running tasks
  unknown to the restored state become `confirmed` records holding slots;
  unresolved tickets are quarantined) or an operator resolves the remainder
  through the owner path. Because a fresh snapshot cannot reconstruct
  refused starts, earlier `continue`/`extend`/`answer` charges or lost key →
  ticket bindings, every admission decision (key, fingerprint, ticket id,
  counter deltas, run owner and generation) is also appended to a
  monotonic **admission ledger** in the witness-protected storage of relay
  RFC section 19.1 before the database transaction commits; after a
  restore the ledger supplies counter floors and the key → ticket bindings,
  and admission stays closed until the ledger and the database agree. The
  two are kept consistent by a prepare/commit protocol: the ledger entry is
  appended as `prepared`, the database transaction commits, then the entry
  is marked `committed`; on restart a `prepared` entry without its database
  row is treated as **consumed** (counters charged, key bound to its ticket,
  audit row re-emitted from the entry) because the ticket may already have
  been issued, and a database row without a ledger entry cannot exist since
  the entry precedes the commit. Compaction of the ledger never drops an
  entry before its ticket has expired, its budget window has reset and its
  audit retention has passed. If
  the ledger itself is unavailable, admission stays closed until every
  ticket that could exist has expired and every affected budget window has
  reset. Restore and rollback tests cover both.
- Budget exhaustion is a normal event: the factory's stop condition
  `budget_exhausted` (section 12.4) ends the run cleanly with its state intact
  for resumption in the next window.

### 8.3 Admission records and reconciliation

Every budgeted relay-path operation gets a durable **admission record** in
PostgreSQL, written in the same transaction that consumes its counters and
records its audit decision. Key: `(team, principal, idempotency key)`.
Fields: the request fingerprint (section 8.2), the operation ticket and its
expiry, operation, target share and host, consumed counters, the host's
`task_id` once known, state, and timestamps. The chain is fixed: one
admission record → one operation ticket → at most one executed host
operation. The relay never mints a second ticket for a record (relay RFC
section 12.5 forbids replacement tickets after a lost issue
acknowledgement), so no reconciliation path can start work twice.

| State | Meaning | Holds a `max_active_tasks` slot (only when the operation is `task.start`; other operations never hold one) |
| --- | --- | --- |
| `reserved` | Counters consumed, request not yet answered by the host | yes |
| `confirmed` | Host returned a `task_id` (for `task.start`) or accepted the operation | yes, until `ended` |
| `refused` | Host returned a typed refusal before creating anything | no |
| `uncertain` | The request may have reached the host but no answer arrived (link loss, relay restart) | yes |
| `awaiting_resubmission` | The host has the ticket but not a dispatchable payload (task RFC section 8.8), or the relay must re-`begin` and holds no payload | yes, until resubmitted or expired |
| `quarantined` | The ticket expired and its outcome cannot be established | yes, until an operator resolves it |
| `ended` | The task was reported `ended`, or became unreachable for good | no |

Every budgeted operation — `task.start`, `task.continue`, `task.extend`,
a service account's `task.answer` — goes through the same states and the
same reconciliation, including `uncertain` and `awaiting_resubmission`
(task RFC section 8.8 applies to every delivery). The only start-specific
part is the concurrency slot: only `task.start` records hold a
`max_active_tasks` slot. A `task.continue` that the daemon queued during a
re-open window (task RFC section 8.2) is `confirmed` when accepted; if the
queued turn is later cancelled (`task_turn_reopened`, `task_stopped`), the
record stays `confirmed` with that result recorded and its turn is not
refunded — admission was consumed, as for any refused host operation. For
other operations `confirmed` is final and never
moves to `ended`.

Reconciliation:

- **Observation first.** `reserved`/`uncertain` records are resolved by
  `operation.result.get` on the record's ticket once the host link is
  current. A recorded result moves the record to `confirmed` or `refused`;
  `in_progress` leaves it `uncertain`; a result showing the delivery is
  `awaiting_resubmission` (task RFC section 8.8) moves the record there.
  Automatic reconciliation only observes; it never re-sends.
- **Re-sending needs the client.** The relay does not persist prompts or
  payloads (relay RFC section 17.2), so it cannot re-`begin` on its own. A
  record that needs the operation to be (re)submitted stays
  `awaiting_resubmission` until the client retries with the same key, the
  same fingerprint and the full payload; the relay then calls
  `operation.begin` with the **same** ticket, which the daemon accepts only
  if the payload matches the ticket fingerprint. Resubmission consumes
  nothing (section 8.2).
- **Ticket expiry is final.** Once the ticket's encoded expiry passes, the
  daemon rejects any `begin` for it, including after result compaction and
  clock rollback (relay RFC section 12.5 expiry floor), and a post-expiry
  lookup may report reconciliation evidence but never executes. A record
  still `reserved`, `uncertain` or `awaiting_resubmission` at expiry is
  resolved from the last retained result if one exists; otherwise it moves
  to `ended` with reason `ticket_expired_unexecuted` only when the
  daemon's retained evidence shows the ticket was never begun, and to
  `quarantined` when the outcome cannot be established — it keeps its slot
  (a task might be running) and raises an audit flag until an operator
  resolves it through the owner path, which moves it to `confirmed` or
  `ended`. In no case is the work started again; the client must submit a
  new operation under a new key, budgeted anew.
- While the host is unreachable a record stays in its state and keeps its
  slot: an outage never frees headroom.
- `confirmed` records move to `ended` when an installed snapshot or event
  reports the task `ended`. Absence from a snapshot alone never ends a
  record, because a stale or partial view is indistinguishable from a task
  that exists.
- **Operator termination.** A host can disappear for good without its share
  ever being suspended or revoked (a destroyed machine, an abandoned
  enrollment). A team `Admin` or `Owner` may therefore end any `confirmed`,
  `uncertain`, `awaiting_resubmission` or `quarantined` start record through
  an explicit administrative action with a mandatory reason. It releases
  the slot, keeps the periodic counters consumed, is audited as that
  administrator's action, and never touches the host: if the host returns
  and the task still runs, the task is unaffected and simply no longer
  counts against `max_active_tasks`. The relay surfaces records whose host
  has been unreachable longer than `factory.stale_admission_report_after`
  so that this is a visible decision, not a silent leak.
- **Share removal.** On `HostShare` suspension, records stay as they are and
  keep their slots; the tasks keep running under host authority and may
  become reachable again. On terminal revocation, the relay can never see
  those tasks again (relay RFC section 12.4), so their records move to
  `ended` with reason `share_revoked`, releasing the slots. The periodic
  counters they consumed are never returned.
- Records are retained at least until their task is `ended`, their ticket
  has expired, and the longest budget period has passed, then follow the
  audit retention policy. Host-side task idempotency indexes may expire
  earlier (task RFC invariant 3); the relay never relies on them for
  relay-path operations, only on the ticket contract.

### 8.4 Relationship to relay quotas

Transport quotas (relay RPC concurrency, attach bandwidth, snapshot bounds —
relay RFC section 18) remain orthogonal and authoritative for resources.
Budgets bound *delegation intent*; quotas bound *mechanism*. A factory within
budget can still hit transport backpressure, and vice versa.

## 9. Audit and Data Classification

Every mutating `task.*` call on the relay path (`task.start`,
`task.continue`, `task.extend`, `task.stop`, `task.answer`, `task.review`,
`task.retain_worktree`, `task.release_worktree`, and cascading
`session.remove`)
produces the standard audit record (relay RFC section 17.1) extended with:
task and turn identifiers, the run identifier (the task's `run_id`, task
RFC section 13.1: a ULID or UUID, a separate field from the relay's own
request correlation id), budget decision (admitted/refused and counter name), and — for
`task.review` — the verdict and `result_id`; for `task.answer` also whether
the actor is a human or a service account, the `attention_id` and
`settlement_revision` answered, and `answer_verification`
(`provider_addressed` or `unverified`, task RFC section 8.5). Prompt text,
prompt digests, results, check logs, final messages and terminal content
never appear; the retry fingerprint is a keyed HMAC under a relay-local key
that no audit record, log, trace or API response exposes.

Evidence reads (`task.wait`, `task.result`, `task.check_log`, `session.diff`
with `turn`) are audited as
access decisions like terminal-access open/close (relay RFC section 17.1),
coalesced to one record per principal, task, `result_id` (or check log
reference) and authorization generation, within a bounded access lease
`factory.audit_access_lease`; a new settlement revision, a different log, an
ACL or credential change, or lease expiry opens a new record, so repeated
waits on one result do not multiply audit volume while no access to new
evidence goes unrecorded. As with every sensitive access, a read is
not granted if its first access record cannot be written.

Data classification additions to relay RFC section 17.2:

| Data | Host persistence | Relay persistence | Logs/audit | Retention |
|---|---|---|---|---|
| Task/turn metadata (ids, `mode`, lifecycle state, outcomes, `settled_by`, integrity, attention kind, failure class, timings) | Authoritative | Catalog projection (section 10) | IDs, states, decisions | Host: `tasks.metadata_retention`; relay: configured metadata policy |
| Task results, check logs, final messages | Owner-private host files (task RFC section 14) | Never | Never | Existing host policy (retire with the session) |
| Prompts (task metadata) | Keyed fingerprint only (task RFC invariant 7) | Keyed fingerprint on the admission record only | Never | Admission-record retention |
| Prompt text as typed into the PTY | Owner-private scrollback of the session (existing session content, session ACL and retention) | Never | Never | Existing scrollback policy |
| Budget counters | No | PostgreSQL | Decision + counter name | Configured policy |
| Run IDs (`run_id`) | Task metadata, attribution only | Catalog projection and audit | ULID/UUID only, never free text | As task metadata / audit policy |

Task content retires with its session; task metadata survives it for
`tasks.metadata_retention` (task RFC section 14) so a factory can reconstruct
runs. Catalog retirement and its anti-resurrection machinery (relay RFC
section 17.3) apply to task projection fields exactly as to the session row
that carries them.

## 10. Task Projection and Events

A task binds exactly one session and a session belongs to at most one task
(task RFC section 6.1), so the task projection is a bounded, optional
**`task` field on the existing session projection row** (relay RFC section
16), not a separate projection. For a relay-origin session on an active share
that is a task session, the row carries the metadata of the classification
row in section 9: task id, immutable `mode` (`execute` or `investigate`,
which the relay needs to authorize `task.stop` for `task.investigate`
holders and to refuse an executor `worktree_of` through an investigate task
before forwarding), lifecycle state, open turn, last outcome, attention
kind and `auto_resume` of a pending attention (task RFC section 8.5), `settled_by`,
integrity, turn count, owner task, `retain_hold`, `run_id` and timestamps. Turn-level detail (per-turn results) is not projected; clients
fetch it through `task.evidence.read` routed calls.

- Folding tasks into the session row keeps the snapshot coordinator's
  commit-order contract (relay RFC section 16.2) at its three projections:
  task changes are session-row changes, committed and sequenced exactly like
  them. It still grows each row, so the reference profile's per-entry and
  snapshot byte budgets (relay RFC section 18.1) must be re-measured with
  task fields present — a requirement the unattended benchmark (section 15)
  records. `max_snapshot_items` (relay RFC section 16.3) is unchanged, since
  no new items appear.
- Visibility follows section 7.1, not the existence record: the `task` field
  is delivered only to principals holding both `task.metadata.read` and
  `session.metadata.read` on the session. Principals who see only the
  minimal existence record (relay RFC section 14) never see that a session
  is a task session.
- `task_turn_opened`, `task_result_published`, `task_result_final`,
  `task_turn_attention` and `task_reviewed` (task RFC section 13.1) become session-row update events
  with the same epoch/sequence and frame bounds.
- Gap/overflow/restart handling is identical: discard, resnapshot, never
  replay (relay RFC section 16.2).

## 11. Attention Escalation

When a turn settles `attention`:

1. The event enters the projection; the host's `task_attention`
   notification (task RFC section 13.1) is projected like other relay
   notifications and delivered to the escalation targets, with the same
   filtering as every other notification (relay RFC section 14). Targets
   resolve in this order:
   - a human creating principal, if the task was created by a human;
   - for service-account-created tasks, the team's configured escalation
     role (a team setting naming a role, `Owner`/`Admin` by default);
   - only human principals are escalation targets. A service account holding
     a `task.answer` grant may observe the attention through the projection
     like any authorized principal, but it is never the escalation target,
     so a human learns of the question whenever a human target exists. That
     guarantee is fail-closed rather than absolute — an external identity
     provider can still disable the last human account mid-run — and is
     enforced as follows: the target set is validated to contain at least one active
     human when a factory is set up on a team (role assignment, share
     approval) and on every membership or credential change; if it would
     become empty, the relay falls back to the team's active human `Owner`s
     and `Admin`s, and if none exists it refuses service-account `task.start`
     on that team with `factory_no_escalation_target` (fail closed) and flags
     any pending attention in audit. While any service-account task on the
     team is `active`, a membership change that would remove the last active human target is
     refused unless the same change installs a replacement, while a
     **credential revocation is always allowed** — a compromised credential
     must never wait on factory work — and, when it removes the last human
     target, atomically switches the team into the
     `factory_no_escalation_target` mode described next; if the set still empties through a path the relay does
     not control (an identity provider disabling the last account), every
     turn-opening operation on the team — `task.start`, `task.continue`,
     `task.extend`, service-account `task.answer` — is suspended with
     `factory_no_escalation_target`, in-flight turns may still settle
     `attention`, and each such attention is flagged `unescalated` in audit.
     Recovery is an operator action: installing a human target (membership
     change or a new human `Owner`/`Admin`) re-delivers every `unescalated`
     attention notification to the new target set and lifts the
     suspension; the run's terminal report lists any attention that parked
     unescalated.
2. The turn stays `attention` until answered or explicitly stopped. There is
   no escalation timeout that auto-resolves, auto-approves, or auto-fails. A
   `usage_limit` attention (task RFC section 8.5) is the one kind the
   **provider** resolves by continuing on its own; that is provider
   evidence, not an answer, and nothing in the relay or the factory
   triggers it. While its `auto_resume` is `expected` the notification is
   informational: targets learn that the run is paused on a provider limit,
   and nobody is asked to act. When `auto_resume` is `no` or `unknown`, the
   attention is escalated like any other; a human resolves it at the
   terminal or stops the task, because `task.answer` does not apply to it.
3. An authorized principal answers via `task.answer` under section 7.3.
   Being an escalation target grants nothing: the target also needs the
   `task.answer` class and `session.task.control` on the task's session,
   and — because attention text and choices travel only on the evidence
   path, never in the notification — `task.evidence.read` and
   `session.terminal.observe` to read the question before answering
   (section 7.1), or team `Owner`/`Admin` authority (relay RFC section 14).
   The target validation of step 1 checks all four.
   Team administrators configuring the escalation role must grant those, and
   the notification states when a target lacks them. The answer is
   attributable to that principal in audit, distinct from the factory's own
   turns. On a share without `allow_delegated_answers`, the
   notification says so and points the target to the owner path or to a
   relay attach, which requires `session.terminal.control` (section 7.3);
   targets who hold neither can only escalate further out of band. Team
   administrators setting up a factory on such a share are warned at setup
   time that escalations will not be answerable over the relay.
4. An unanswered `attention` at a run checkpoint is a stop condition
   (`ask`, section 12.4), not a hang: the factory parks the run and reports.
   A `usage_limit` attention with `auto_resume: expected` is not
   unanswered: the factory keeps waiting on it (`task.wait` waits through
   it) under the run's own wall-clock policy.

A human answering directly in the terminal (task RFC section 8.6) is equally
valid; the factory observes the resumed turn like any other evidence.

## 12. Factory Client Contract

### 12.1 The loop

One round of the Manage-Execute-Audit loop, entirely through public interfaces:

1. **Manage**: pick the next subtask from client-side task state; compose a
   bounded contract (prompt, `checks`, `mode`, deadline override).
2. **Execute**: `task.start` (the first round creates the worktree with
   `retain_worktree: true`, so the tree outlives every task of the run; later
   rounds use `worktree_of`, task RFC sections 8.7 and 16.2) or
   `task.continue`; then `task.wait`. Parallel executors on one host are
   collected with one multi-target `task.wait` (`mode: "any"`, each consumed
   `result_id` passed back as the target's `after` cursor, task RFC section
   8.3);
   executors on several hosts need one such wait per host, raced by the
   client. The relay authorizes and audits a multi-target wait as one
   evidence read per target (section 9) and refuses the whole call when
   any target is not authorized, so the response never reveals a task the
   caller may not read. Every task of the run carries the run's
   `run_id`. Budget admission and ACLs are the relay's; settlement
   is the daemon's.
3. **Audit**: read the result's verified fields (task RFC section 10),
   judging the round by `turn_delta`; if the round's claims matter, the
   manager first stops the executor task (`if_latest_turn`, `require_idle`)
   because the daemon hands a shared tree to another task only once the
   previous occupant's runtime is stopped and joined (task RFC invariant
   11), and the auditor principal then runs a `mode: "investigate"` task
   with `worktree_of` the executor's task, so it inspects the executor's actual uncommitted state,
   or relies on the round's `checks`; record `task.review` bound to the
   executor result's `result_id` and the verified `worktree_fingerprint`
   (task RFC section 13.1). A `finality: heuristic` result is audited only
   after its re-open window closes; the daemon keeps the worktree occupied
   until then, so the audit task cannot start early (task RFC section 8.2,
   invariant 11).
4. **Clean up**: once the verdict is recorded, the manager durably writes
   the **round record** (round id, executor and auditor task ids, reviewed
   `result_id`, each task's latest turn at close) and then each principal
   stops the tasks **it created**: the manager stops the executor task, the
   auditor client stops its own investigate task. Both use `task.stop` with
   `if_latest_turn` and `require_idle` (task RFC section 13.1). This split
   follows the grants: each principal holds `session.lifecycle.control` on
   the sessions it created (relay RFC section 14), and the manager holds
   none on the auditor's sessions (section 7.2). Stopping ends the tasks and
   releases their `max_active_tasks` slots through the admission records
   (section 8.3). The owner task (the first executor) is stopped after its
   round like any other; the shared worktree survives on its retain hold,
   which the manager releases with `task.release_worktree` in the run's
   terminal step. Without this step a sequential run
   accumulates live sessions and eventually exhausts `max_active_tasks`,
   which a new period does not reset.

No step requires relay-specific intelligence: the loop is expressible with the
task-layer CLI (`task run --wait`, `task continue --wait`, `task review`) once
the task RFC's CLI workstream lands.

### 12.2 Manager state placement and recovery

- Run state (requirement checklist, artifacts, facts — the manager's task
  state in the Manage-Execute-Audit loop) lives in the manager's own memory,
  its own session, or files it writes. It never enters daemon or relay
  persistence (task RFC section 16.4).
- **Everything evidential is reconstructible**: `task.list` filtered by the
  run's `run_id`, plus `task.inspect`, `task.result` and the
  recorded `task.review` verdicts, allow a fresh manager to rebuild the
  verified picture of any run. Task metadata and verdicts outlive swept
  sessions for `tasks.metadata_retention` on the host and for the catalog
  retention on the relay (section 9), and result content is available while
  its session exists. A manager crash loses its plan, never its evidence,
  within those retention windows; a run meant to be resumable across them
  must checkpoint what it needs.
- The recommended pattern is therefore checkpoint-shaped: after every round,
  the manager writes its next-step decision where its successor can read it,
  so recovery is a new client process, not a restore.
- **Single writer per run.** A run has exactly one live manager and one live
  auditor client, and failover is fenced rather than assumed: before a
  successor starts, the predecessor's service-account credential is **revoked or expired** — a rotation whose overlap keeps the
  old credential valid is not enough (relay RFC section 13.3 then cancels
  its streams and refuses its calls) — and the successor records a new **run generation** in its round
  records; a client that finds a newer generation than its own in the
  records stops acting. Revocation alone cannot undo an operation whose
  host-side irreversible commit already won before the response was lost,
  so two further rules close that window. First, a **failover barrier**:
  before its first mutating call the successor resolves every admission
  record of the run that is not terminal — `reserved`, `uncertain`,
  `awaiting_resubmission` **and** `quarantined` — through the relay's
  `factory.run.reconcile { run_id }` method, which the run owner or a team
  administrator may call: admission records carry `run_id` and
  `run_generation` as indexed fields, and the method returns them paged
  with their current state (resolving `reserved`/`uncertain` through
  `operation.result.get` on the way). The barrier is closed server-side:
  `factory.run.failover` succeeds only once no **unresolved** record remains
  — exactly `reserved`, `uncertain`, `awaiting_resubmission` and
  `quarantined`; `confirmed` starts of running tasks are not unresolved and
  are handed to the successor for adoption — and a `quarantined` record is
  resolved only by the explicit administrative action of section 8.3, so
  the successor cannot proceed past it. The
  successor then adopts every task the catalog lists under that `run_id`
  and never starts a new first-round task while any task of the run
  exists. Second, a **server-side generation fence**: every budgeted
  request carries `run_id` and `run_generation`; the relay keeps a **run
  record** per `(team, run_id)`, opened by the first `task.start` that uses
  the id (or explicitly by `factory.run.open`), owned by that principal, and
  carrying the member principals the owner admits with
  `factory.run.add_member` — the auditor in the reference loop — plus one
  generation counter shared by every member. A principal that is neither
  owner nor member of the record cannot admit work under that `run_id`
  (`factory_run_not_member`), so a colliding id neither fences nor joins a
  run; catalog recovery adopts only tasks whose admission records belong to
  the run record, never bare `run_id` matches. The relay refuses a lower
  generation from any member with `factory_run_fenced` before forwarding; the fence applies to **every**
  run-scoped mutation by a run owner or member — `task.stop`,
  `task.review`, `task.retain_worktree`, `task.release_worktree` and
  cascading `session.remove` as well as the budgeted calls — so a
  predecessor cannot stop, review or remove the successor's work either.
  Two classes of callers are outside the fence and the membership check by
  design, each audited with a `fence_bypass` marker: a **human** escalation
  target answering through `task.answer` under section 11
  (`human_escalation`), and a team `Owner`/`Admin` performing the
  administrative cleanup of section 12.2 or the resolution of section 8.3
  (`admin_cleanup`). Neither needs to know or advance `run_generation`. The generation advances only
  through an audited `factory.run.failover` action by a team administrator
  or by the owner account after its credential rotation; a request cannot
  raise it by itself. `run_id` stays attribution, not authorization: the
  fence is admission state keyed by the run record and its members. A same-key,
  same-fingerprint retry of an existing admission record — the write-ahead
  resubmission below — is resolved **before** the fence and is never refused
  by it, so a successor can finish the predecessor's `awaiting_resubmission`
  operation under the original ticket. A predecessor
  that resumes
  after the successor's first admission cannot delegate again even with a
  still-valid credential. The daemon's `if_latest_turn` preconditions and
  `worktree_of` occupancy refuse the stale plan's stops and starts on the
  host. Operators who cannot revoke the old credential must not start a
  successor.
- **Cleanup recovery.** Task state alone cannot tell a finished round from
  one that continued: a still-valid verdict on the latest result says
  nothing about a turn opened since, and an auditor task never carries a
  verdict of its own (the verdict is recorded on the executor's result).
  Recovery therefore works from the durable round records, not from
  `task.list`: a restarted manager stops the executor tasks of closed
  rounds, and a restarted auditor client stops its own investigate tasks
  of the audits it closed, each with the recorded turn as `if_latest_turn`.
- **Where records live.** Each client keeps its **own** records in its own
  client-side state (its state directory, its own session's files, or a
  store the operator provides) — the manager its round records, the auditor
  its closed-audit records. No record is shared between the two clients,
  and no record is ever written inside the objective's worktree, where it
  would pollute `turn_delta` and be visible to the executor.
- **Write-ahead record before every mutating request.** Recovery from
  `awaiting_resubmission` needs the exact idempotency key, target,
  parameters and full payload, and the relay keeps only a fingerprint
  (section 8.3). Each client therefore persists an owner-private
  write-ahead record with all of those before it sends any `task.start`,
  `task.continue`, `task.extend` or `task.answer`, and deletes it only after
  the operation is `confirmed` or `refused`. A successor resubmits from
  these records; without one, a reserved or quarantined admission holds its
  slot until ticket expiry or operator repair, exactly as section 8.3
  states. Tests cover a crash at every pre-confirmation boundary.
- **Cleanup is retried, not abandoned.** A `task_stop_precondition_failed`
  means work continued after the round closed; the task is left running and
  put on the client's **pending-cleanup list** with its new latest turn.
  The client re-attempts the stop each time that turn settles and the task
  is idle, and before the run's terminal report. A run cannot report itself
  finished while its pending-cleanup list is non-empty; if cleanup still
  fails at run end, the terminal report lists the tasks left `active` and
  the reason, so slots never leak silently. `task.stop` is idempotent
  (including on already-ended tasks), so a repeated cleanup is harmless. `task.list` by `run_id` (paged) is used only
  to find tasks that no round record mentions — work started just before a
  crash — which the resumed loop adopts or, once idle, stops. Worktrees and
  evidence are not affected by this step.
- If the auditor client is gone for good, the auditor's tasks are stopped
  by a team `Owner`/`Admin` or by a separately granted, narrowly scoped
  cleanup grant: `session.lifecycle.control` on sessions created by that
  auditor service account on the factory's shares, with mandatory expiry.
  The manager does not receive lifecycle authority over auditor sessions by
  default, so an executor principal can never stop the audit of its own
  work.

### 12.3 Multi-host delegation

The loop is host-agnostic: `task.start` targets hosts exactly like sessions
(task RFC section 3), budgets are team-scoped across hosts, and a single run
may span hosts. `task.list` on the relay path has two forms with distinct
meanings:

- **Catalog query (default).** The relay answers from its synchronized
  catalog across all hosts the caller can see: one paged, ACL-filtered
  answer over the task fields of the session projection (section 10),
  filterable by `run_id`, host, lifecycle state and outcome. It
  covers tasks whose catalog entries are current or retained (relay RFC
  section 17.3); a stale host is marked stale in the response, never
  silently omitted. This is the form multi-host runs use to find their
  tasks.
- **Routed per-host query (`host` set).** Forwarded to one daemon and
  answered from its task store, subject to the same ACL rules (section 7.1):
  it returns only tasks whose catalog entries are current or retained,
  because the relay holds no authorization record for a retired entry. Task
  metadata the host keeps beyond catalog retirement under
  `tasks.metadata_retention` is reachable on the owner path only; the routed
  form neither lists nor reveals it (identical filtering).

### 12.4 Stop conditions

A run ends when any of: goal satisfied per the auditor's verdict; `ask`
(unanswered `attention`); `budget_exhausted`; `lost`/`stopped` turns beyond
the run's tolerance; or the round budget (task RFC section 16.2 — N rounds or
a cost ceiling, orchestrator policy). The run's terminal report states which
condition fired and the last verified state.

## 13. Failure Semantics

| Situation | Behaviour |
| --- | --- |
| Relay unavailable | Factory cannot admit or read routed calls and halts; host turns continue and settle (task RFC section 15). Owner paths unaffected. |
| Relay restart | Budgets and audit restore from PostgreSQL; projections resnapshot; the factory reconnects and reconciles from `task.list` by `run_id`. |
| Host link down (host still running) | Its projection goes stale and routed calls fail; host turns continue and settle normally; stale active tasks keep counting against `max_active_tasks` (section 8.1). On reconnect the relay resnapshots and the factory reconciles. |
| Host or worker failure | Runtime-generation loss settles open turns `lost` per the task RFC; the next snapshot reports it. The factory records it and replans. |
| `HostShare` suspended | Relay-path calls to the share's tasks are refused and its sessions leave snapshots (relay RFC section 12.4); host turns continue. The run parks; on reactivation the tasks become reachable again. |
| `HostShare` revoked | Same, permanently: its tasks stay owner-only forever (relay RFC section 12.4). The run treats them as lost to the factory and replans on another share; the host owner decides what happens to the worktrees. |
| Manager principal revoked | Relay cancels its streams promptly (relay RFC section 13.3); open turns settle by their own rules; nothing on the host is killed. |
| Auditor principal revoked | Same; in-flight investigate tasks settle `stopped` only if explicitly stopped by an authorized principal. |
| Budget exhausted | Typed refusal at admission only (section 8.2); the run parks with state intact. |
| `attention` unanswered | Turn stays `attention`; run parks at `ask`; escalation targets remain notified. |
| Provider usage limit | Turn settles a `usage_limit` attention. With `auto_resume: expected` the run waits, the targets get an informational notification, and the provider's continuation re-opens the turn without an admission; with `no` or `unknown` it is escalated and the run parks at `ask` (section 11). |
| Service-account answer grant expires or is revoked | In-flight answer attempts are cancelled; later attentions escalate to humans only. |
| Relay waiter limits reached by long `task.wait` | Typed overload from relay quotas (relay RFC section 18); the factory retries later. Long waits are sized in the re-measured reference profile (section 15). |
| Duplicate delivery after timeout | The retry carries the same client key and fingerprint, so the relay resumes the same admission record and operation ticket (sections 7.1, 8.2); the daemon's ticket contract prevents a second execution. Never two inputs; the retry is free and exhaustion-proof. |
| Relay loses a `task.start` response | Admission record goes `uncertain`, keeps its slot, and is resolved by `operation.result.get` on its ticket once the link is current; nothing is re-sent automatically (section 8.3). |
| Relay or daemon restarts before the payload was dispatched | Record goes `awaiting_resubmission`; the client resubmits the same payload under the same key and ticket, or the ticket expires without execution (section 8.3, task RFC section 8.8). |
| Manager crashes between review and cleanup | The restarted manager and auditor clients stop the tasks of closed round records, each with an `if_latest_turn` precondition; continued work is left running (section 12.2). |
| Owner session of a shared worktree removed over the relay | The relay authorizes lifecycle control on every affected task session; the daemon requires the exact user set and one share, and blocks new users while removal is pending (task RFC section 8.7). |
| Service account tries to approve through the terminal | Refused: service accounts hold no `session.terminal.control` unless explicitly granted (section 7.1). With such a grant the approval is audited as terminal control and recorded as a terminal resolution on a steered turn. |
| Factory bug (loop storm) | Budgets cap volume; transport quotas cap resources; audit records show the pattern; revocation is one API call. |

## 14. Required Invariants

1. Factory roles authorize API calls only; they are never a workload-isolation
   claim ([#88](https://github.com/zajca/pohunek/issues/88)).
2. The relay never decides task outcomes: settlement is the daemon's,
   causally scoped as defined in the task RFC.
3. Nothing answers or approves `attention` implicitly: no timer, default,
   role template or relay logic does it. A `task.answer` comes only from a
   principal explicitly authorized under section 7.3 — for a service
   account, a share-scoped, expiring, administrator-created grant on a share
   the host owner opted in — and is audited as that principal's own action.
   An approval typed into the terminal comes only from a holder of
   `session.terminal.control`, which a service account holds only through an
   explicit grant (section 7.1); it is audited as terminal control and
   recorded by the task layer as a terminal resolution. A provider
   continuing by itself after a usage limit (task RFC section 8.5) is
   provider evidence that resolves a `usage_limit` attention, not an answer,
   and nothing in the relay or the factory triggers it. Escalation reaches a human whenever one exists and is **fail-closed**
   otherwise: turn-opening operations are suspended, in-flight attentions are
   flagged `unescalated`, and operator recovery (installing a human target)
   re-delivers them (section 11); no silent auto-resolution ever fills the
   gap.
4. Budget exhaustion never stops, pauses or mutates running work.
5. Relay persistence and audit carry no prompt text, results, check logs,
   final messages or terminal content.
6. A factory principal cannot widen its own authority; grants are team-admin
   actions and are audited.
7. Host owner paths always outrank the factory: owner sessions, owner task
   calls and local repair are never budgeted or ACL-blocked (relay RFC
   section 12.3).
8. Manager/auditor task state lives in clients only; the daemon and relay stay
   orchestration-free (task RFC section 16.4). `run_id` is opaque
   attribution metadata, never a scheduling or authorization input.
9. Task data never widens session access: every task class requires the
   corresponding session class on the task's session (section 7.1), and task
   projection fields are never delivered with an existence record.

## 15. Workstreams and Definition of Done

Ordered by dependency; each lands with the tests named:

1. **Protocol and daemon (`task.*` substrate):** the delegated task runs RFC
   workstreams land first; this RFC adds nothing to settlement.
2. **Host share policy:** `task.*` operation classes, the
   `allow_delegated_answers` and `allow_unverified_answers` share
   capabilities and the permitted-check list (host names and `repo:` names)
   in `HostShare` policy, all enforced by the daemon, with the daemon-side intersection (relay RFC section
   12.2) and share-approval UI text for the capability (section 7.3); tests
   for share-refuses-task, share-refuses-delegated-answer,
   share-refuses-unverified-answer, share-refuses-check, and a plain check
   entry never resolving to a repository definition, `repo:<name>` not
   permitting a shadowing host definition, and an executor `worktree_of`
   naming an investigate task refused, regardless of relay role.
3. **Relay typed API and authorization:** `task.*` methods in the versioned
   WebSocket control API (relay RFC section 15.3) with typed relay protocol
   and client definitions and the generated TypeScript contract (team
   clients never send arbitrary daemon NDJSON, relay RFC section 15.1);
   `task.*` permission classes with their session-class intersections, the
   `session.task.control` class and the service-account creator default
   without terminal control, `task.investigate`, `worktree_of`
   authorization, role templates, the share-scoped expiring service-account
   `task.answer` and terminal-control grant flows, the
   `allow_unverified_answers` capability, operation-ticket binding; tests for
   the three-layer answer resolution (section 7.3), self-widening refusal,
   `FactoryOperator` refusal for service accounts, evidence read refused
   without `session.terminal.observe`, `task.stop` refused without
   `session.lifecycle.control`, an executor `worktree_of` naming an
   investigate-mode task refused at the relay with
   `task_worktree_via_investigate` before forwarding, `task.retain_worktree`
   naming a chain member authorized on the owner session, and identical
   filtering of absent vs. forbidden tasks.
4. **Relay budgets:** durable counters across all applicable scopes,
   admission records bound one-to-one to operation tickets with request
   fingerprints and the `reserved`/`confirmed`/`refused`/`uncertain`/
   `awaiting_resubmission`/`quarantined`/`ended` lifecycle and
   reconciliation, atomic admission + audit, typed
   `factory_budget_exhausted`; tests for fixed window reset, restart
   persistence, multi-scope atomicity, owner-path bypass, read-path,
   `task.stop` and human-answer bypass, `task.extend` accounting,
   host-refused start releasing its slot but not its periodic counters, a
   lost `task.start` response resolved by `operation.result.get` without a
   second charge, a same-key retry with a changed target or payload
   refused with `task_request_conflict` and never forwarded, a relay
   restart before `begin` yielding `awaiting_resubmission` and a matching
   client resubmission executing once under the original ticket, ticket
   expiry never re-executing (including after host-side task metadata
   retention removed the task and after clock rollback), an unknown expired
   outcome quarantined with its slot held, a same-key retry succeeding
   after the budget is exhausted,
   an `uncertain` record keeping its slot through a host outage, suspension
   keeping and revocation releasing slots, absence from a snapshot never
   ending a record, and "running work untouched".
5. **Projection and events:** `task` field on the session projection row and
   task events as row updates within existing frame/queue bounds; tests for
   gap/overflow resnapshot with task fields present, ACL-filtered delivery
   (no task fields with existence records), and retirement of task fields
   with their catalog entry.
6. **Escalation:** `task_attention` routing to human targets, the team
   escalation role setting, and the no-delegated-answers warning;
   informational delivery of `usage_limit` attentions while `auto_resume` is
   `expected` and normal escalation once it is not; tests for target
   resolution, human-only targets, no-auto-resolve, and a provider
   continuation that re-opens a `usage_limit` turn without consuming a
   budget counter.
7. **Client/SDK and skill:** factory loop reference implementation in the SDK
   (with recovery from task records), multi-target waits per host with
   `after` cursors for parallel executors, waiting through `usage_limit`
   attentions under a run wall-clock policy, the "Delegating long-horizon work" skill
   section extended to multi-host runs, `FactoryManager`/`FactoryAuditor`
   setup documentation; knowledge bundle updated (`cargo xtask docs check`).
8. **Validation:**
   - adversarial tests: answer escalation attempts, grant self-widening,
     a service account obtaining `FactoryOperator` or a team-wide answer
     grant, task evidence read as a session-observe bypass, `worktree_of`
     into a session the caller cannot control (including the investigate
     exception: observe rights plus a self-created investigate task never
     yield an executor start), budget bypass via retry
     storms or `task.extend`, idempotency-key collision across principals,
     projection leaking content fields or task fields to existence-only
     viewers, a stale `task.answer` (older `attention_id` or revision)
     refused after forwarding, a service-account creator receiving no
     `session.terminal.control` and being refused attach and
     `session.input`, `task.continue` succeeding with only
     `session.task.control`, a terminal-control grant to a service account
     requiring administrator creation, share scope and expiry, and an
     unverified `task.answer` refused without `allow_unverified_answers` on
     the share or without `unverified` in a service account's grant, grant
     provenance (role, group, team-wide and wildcard grants never confer
     `task.answer` or terminal control on a service account), database
     restore quarantine for factory admission, an empty human escalation
     target set failing closed, audit coalescing opening a new record per
     `result_id` and authorization generation, a fenced-out predecessor
     manager refused by credential revocation, by `factory_run_fenced`
     (keyed by run owner, so another principal's `run_id` collision neither
     fences nor joins the run, a request cannot raise the generation, a
     same-key retry passes it, and it also covers `task.stop`, `task.review`
     and worktree holds), `factory.run.failover` refused while a
     `quarantined` record remains, a rotation with overlap refused as a
     failover precondition, a restore reconciled from the admission ledger,
     an escalation target lacking evidence rights rejected at setup, a
     credential revocation never blocked by active factory work, a human
     escalation answer and an administrative cleanup passing the run fence
     with an audited `fence_bypass`, a retained worktree's owner row
     surviving catalog retirement until release, an
     auditor without `session.metadata.read` unable to review, a non-member
     principal refused under a colliding `run_id`, the auditor member fenced
     by the manager's failover, `factory.run.failover` succeeding with
     `confirmed` running starts adopted, a crash between the ledger
     `prepared` entry and the database commit recovered as consumed, a late
     `operation.cancel` after the response left being a no-op, and
     by stale preconditions, revocation after an irreversible host commit
     but before response delivery resolved by the failover barrier, the
     last human target's removal refused or suspending turn-opening
     operations, and a successor resubmitting from a client write-ahead
     record;
   - an **unattended benchmark**: the task RFC benchmark run lights-out
     through the relay path (manager + auditor service accounts), reporting
     turns per run, budget headroom consumed, escalations raised and
     answered, and zero human interventions other than defined escalation
     points — this measurement calibrates the proposed budget values and
     re-measures the relay RFC section 18.1 snapshot bytes and long-wait
     waiter usage with task fields present.

Definition of done: all gates in `AGENTS.md` pass; every invariant in section
14 has at least one test; the unattended benchmark runs a complete
multi-round, multi-host run with no human input outside `attention` answers,
records its budget consumption, and leaves reconstructible audit evidence.

**Implementation issues** (filed 2026-09-29 as sub-issues of #185): relay
RFC amendments #233; share policy `task.*` classes, answer capabilities and
check list #234; relay `task.*` authorization and role templates #235; relay
delegation budgets #236; task projection fields #237; escalation routing
#238; SDK factory loop and skill #239; unattended benchmark #240.

## 16. Alternatives Considered

- **Manager/auditor inside `pohunek-relayd`.** Would make the relay an
  orchestration authority and put task state where the accepted relay RFC
  forbids durable content. Rejected (section 5, task RFC section 16.4).
- **Store task results at the relay for manager convenience.** Duplicates
  owner-private content across the trust boundary. Rejected; projections stay
  metadata-only and `task.evidence.read` routes reads on demand.
- **Auto-approve or auto-answer on timeout.** Makes the factory its own
  permission authority. Rejected (task RFC non-goal; invariant 3).
- **Human-only answers as a permanent rule.** Simplest to reason about, but
  forces a human into every provider approval a profile could not pre-grant,
  which defeats unattended operation for legitimate approval flows. Rejected
  in favour of the explicit, share-scoped, expiring service-account grant
  gated by the host owner's opt-in (section 7.2), with escalation still
  always reaching a human.
- **A fourth snapshot projection for tasks.** Mirrors how notifications are
  projected, but changes the coordinator's freeze contract and item budgets
  for data that is 1:1 with sessions. Rejected in favour of a `task` field
  on the session row (section 10).
- **Per-task budgets instead of per-period counters.** Cannot express a spend
  rate and admits burst storms between tasks. Rejected in favour of
  per-period counters plus `max_active_tasks`.
- **A dedicated factory daemon (`pohunek-factoryd`).** Reasonable as a later
  packaging step, but the client contract (section 12) is deliberately
  implementable as an SDK loop first; nothing here requires a new binary.
  Deferred to the open question below.

## 17. Open Questions

1. Budget granularity: are per-project or per-profile scopes needed beyond
   per-team/per-service-account/per-share (section 8.1)?
2. Should an unanswered `attention` escalate further (secondary targets,
   external webhook) within this RFC, or is the notification policy machinery
   sufficient as-is?
3. When provider metrics are `unknown` (task RFC invariant 6), should the
   cost ceiling remain purely advisory, or should `unknown`-heavy runs trip a
   separate advisory warning?
4. Does a packaged `pohunek-factoryd` (or a `pohunek factory` CLI subcommand)
   ship with the client contract, or does the loop stay an SDK/skill recipe
   until demand is proven?
5. Should a service-account `task.answer` grant be further narrowed by
   attention kind (for example `approval` only, never `question`), or by
   provider choice?

Resolved: service accounts may hold `task.answer` only through the explicit,
share-scoped, expiring administrator grant of section 7.2, exercisable only
on shares with `allow_delegated_answers`. Delegation is separated from
terminal control: task-layer delivery needs only `session.task.control`, and
service accounts receive no creator terminal control (section 7.1).

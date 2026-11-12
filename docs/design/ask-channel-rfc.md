# RFC: Ask channel and a Pohunek MCP server for agents

- **Status:** Withdrawn 2026-11-12 (see #1987). Not planned: nothing here will be implemented. The document stays as the record of the design and its measurements.
- **Date:** 2026-11-06
- **Scope:** Part A, a durable ask primitive between a running agent and its operator, with per-runtime
  emission, native permission prompt routing, surfaces and a secret answer kind. Part B, the Pohunek MCP
  server with per-session rights, including the MCP versus A2A decision.
- **Audience:** maintainers of `pohunekd`, `pohunek-sessiond`, the protocol, the CLI and the runtime
  packages; authors of companion and web surfaces

It amends the [delegated task runs RFC](delegated-task-runs-rfc.md) in three places (sections 5, 8.5 and
13.4, summarised in section 0.2) and reuses its `pohunek mcp` command and its attention model.

**Citations.**

| Shorthand | Meaning |
|---|---|
| `<path>:<line>` | zajca/pohunek `main` at `b7c440fe` (before this RFC landed) |
| `U:codex/<path>:<line>` | openai/codex at tag `rust-v0.166.0` |
| `U:pi/<path>:<line>` | badlogic/pi-mono at tag `v1.5.0` |
| `C:<path>:<line>` | cline at tag `cli-v3.0.74` |
| `CH-n`, `M-CX-n`, `MUSE:<line>`, `KC:<line>` | Maintainer research notes (session backends study, Muse Code plan, agent keychain study) that are not part of the repository; each claim that rests only on them is marked where it matters |
| `M1`, `M2` | Measurements of this study, section 13 |

Web sources are URLs. **UNVERIFIED** marks a claim that could not be confirmed against a primary source
(for example only through a search summary or a third-party mirror). Those markers are kept as found.

---

## 0. Summary and recommendation

**The problem.** Pohunek learns that an agent waits for the operator from screen rules and hooks only
(`docs/architecture.md:1186-1199`). Rules match text in a region, so model text can defeat them: #1968
shows a Cline question whose clipped list and a rule line in the model text leave no rule able to read
`blocked`, so typed input can answer the dialog (zajca/pohunek#1968 body; pohunekd/plugin-cline#10 round 2).
Every new runtime repeats the rule work.

**The recommendation, Part A (ask channel).**
1. **A core primitive, the ask.** A durable, session-owned record with a question, a context block, one to
   four items of a closed set of answer kinds (`choice`, `multi`, `text`, `confirm`, `secret`), a deadline,
   and a terminal outcome (`answered`, `refined`, `declined`, `cancelled`, `expired`). The daemon stores it
   and checks it; the worker attests who asked. It is not a chat: the operator answers or refines, the agent
   gets a typed result, and nothing else flows.
2. **Plain tools first, elicitation never in core.** Agents emit asks through a Pohunek MCP server
   (`pohunek mcp`, stdio, one per session) with an `ask` tool and an `ask_wait` tool, and through a
   `pohunek ask` CLI for shells and hooks. The tool blocks for a bounded slice (with progress notifications),
   then returns `pending` plus an id, because client tool timeouts differ: Codex 300 s, Pi 60 s, Claude
   about 28 h (section 4.2). MCP elicitation is the wrong direction (server asks the client's user, in the
   agent's TUI), and the 2026-07-28 revision replaced it with MRTR anyway (section 2).
3. **Native prompts are routed where a hook can answer them, not scraped.** Claude `AskUserQuestion`
   (PreToolUse with `updatedInput`) and Claude/Codex/Muse `PermissionRequest` hooks can return an answer
   from the channel. Protocol backends (#1561: Codex app-server, Cline hub, Muse MSP, Pi RPC, ACP) carry
   approvals and questions as structured requests. A pending routed ask is `blocked` evidence and refuses
   PTY input; screen rules stay as the fallback for prompts nothing routes.
4. **Secrets come back as references.** A `secret` item never returns its value. The operator enters or
   picks it in an owner surface, it is stored in a vault backend, and the session gets a handle. The value
   reaches a process only through `pohunek secret exec` (run in place inside the sandbox and the PTY tree, output masked) or a later launch's
   `secret_env`. This protects the model context and transcripts, not the process tree (M1): a hostile
   same-UID agent can still read a child's environment.
5. **Pilot runtime: Claude Code.** It has the widest structured surface today: stdio MCP with no
   per-request timer, an answering PreToolUse hook on `AskUserQuestion`, an answering `PermissionRequest`
   hook, and a compiled core handler (`claude-hook-v1`). The per-launch input seam exists (Codex and Muse
   use it, `crates/daemon/src/integration/provider_handlers.rs:47-61`,
   `crates/daemon/src/integration/muse/mod.rs:92`); slice AC-05 adds a Claude contribution.

**The recommendation, Part B (MCP server with per-session rights).**
1. **One server, two layers.** `pohunek mcp` is the same process as in Part A. Its tools map onto the
   daemon methods agents already use through the CLI (`docs/knowledge/guides/agent-skill.md:31-335`). The
   CLI stays for humans; both call the same daemon API.
2. **Rights are per-session capability sets, default deny, granted only downward.** The operator (host
   profile and package descriptor) sets a ceiling; a parent session can grant a child only a subset of its
   own set; the daemon enforces them on the worker-attested path. The closest prior art is Pohunek's own
   Hermes operator policy with `read` / `manage` / `full` access modes (`docs/architecture.md:1572-1593`).
3. **Say it plainly: this is a guardrail, not a boundary.** Today every session sees the owner socket
   (`POHUNEK_SOCKET_PATH` is in the session environment), and the origin guard "is not authentication"
   (`docs/architecture.md:2216-2233`). A process can leave its PTY tree with a `setsid` double fork (M2)
   and read other processes' environments (M1). Rights stop a confused or prompt-injected agent that uses
   the sanctioned path. They do not stop a hostile agent until sessions run behind a real boundary
   (#88 container/VM profiles). Slice AC-20 adds the same rights to the public socket for callers whose kernel
   peer sits inside a session's tree, which closes the casual bypass but not the deliberate one.
4. **MCP, not A2A, for both agent-to-Pohunek control and session-to-session delegation.** Every runtime we
   run is an MCP client; none is a native A2A client or server (section 9.8). Session-to-session tasks are
   Pohunek's own records (sessions and asks) that borrow A2A's state names. An A2A gateway for external
   agents is a later, separate adapter.

**Owner questions: 12, answered 2026-11-06** (section 12). The issue tree is the sub-issues of epic #1987
(section 10).

### 0.1 Owner answers applied (round ask-v1)

| Question | Answer | Where it changes this design |
|---|---|---|
| Q1 emit | (a) MCP tool + `pohunek ask`; note: "will this work universally? What if agent need for example permission which is still native" | New section 4.7 answers it |
| Q2 pilot | (a) Claude Code | unchanged |
| Q3 expiry | (a); note: may wait until answered if super important | `importance: important` asks without expiry (3.1, 3.3, 3.6) |
| Q4 refine | (a) | unchanged |
| Q5 manager answers | (a) only choice/text, only when granted; note: depends on agent settings | The grant lives in the manager's agent settings (host profile, package), not a global switch (3.4) |
| Q6 vault | changed order: plugin-vault if installed, then system keychain, then password manager | 6.3 |
| Q7 secret use | (a) | unchanged |
| Q8 remote secret entry | (b), not the recommendation: from any owner device over the overlay | Security conditions in 6.5 |
| Q9 block input | (a) | unchanged |
| Q10 guardrail | (a); note: if bigger effort, questions first, then permissions; VMs future scope | Slices reordered (10); #88 out of scope |
| Q11 defaults | (a) | unchanged |
| Q12 protocol | (a) MCP now, A2A gateway later | unchanged |

### 0.2 Alignment with the delegated task runs RFC (#182)

The accepted task RFC (`docs/design/delegated-task-runs-rfc.md`, epic #182, all workstreams open) already
defines three things this design must reuse rather than duplicate:
- **`pohunek mcp`**, a stdio MCP server inside the CLI binary (section 13.4; issue #231). The
  ask tools are added to that same command; "A daemon-side MCP server" stays a non-goal (section 5). The ask
  tools need one addition the task adapter does not: a worker-attested channel for session-bound calls
  (section 9.3), because origin markers are not authentication.
- **Attention** for provider approvals and questions inside a task turn (section 8.5), answered
  by `task.answer`. Its Claude and Codex answers are keystrokes (`verification: unverified`) and refused by
  default (`task_answer_unverifiable`). A routed native prompt (4.4, 4.7) is exactly the missing
  **addressed** path: the hook holds the provider's own request and returns the decision to it, so a stale
  answer cannot reach another prompt. The tree therefore makes a routed ask the source of a task attention
  when a task is running, and `task.answer` on such an attention closes the ask.
- **"Decision requested is not waiting"** (section 8.5): a routed hook is the waiting point, but another hook,
  a rule or the mode may still decide. A routed ask records the provider's real outcome and closes as
  `resolved_elsewhere` when something else decided (4.7). The task RFC records this in its section 8.5 amendment.
- **"Automatic approval of provider permission prompts on the owner's behalf"** is a non-goal (section 5). The
  ask channel keeps it: the daemon never answers for the owner (3.4); an expiry default (3.1) is the asker's own
  declared choice applied by the store, not an answer.

---

## 1. Today's state in core

| Topic | Fact | Source |
|---|---|---|
| Blocked detection | OSC title/progress, then screen rules, then PTY activity, then process state; approval prompts are detected by screen rules | `docs/architecture.md:1186-1205` |
| OSC 7501 | A program may report `blocked` itself; a visible dialog still beats a non-blocked record | `docs/architecture.md:1207-1255` |
| Input while blocked | `refuse_while_blocked` runtimes refuse fire-and-forget input while `blocked`; waited input is refused for every runtime (`session_agent_blocked`) | `docs/public-api.md:514` |
| Attention store | Durable notifications with kinds `agent_blocked`, `approval_required`, `turn_completed`, `session_finished`, `error`, `system` and statuses unread/read/acknowledged/archived/deleted | `crates/protocol/src/notification.rs:28-49,130-150` |
| Hook path | Hooks report through the owner-private worker socket; the worker checks the kernel peer and its ancestry against the PTY tree and journals before forwarding | `docs/architecture.md:1278-1328`; `crates/session-worker/src/server.rs:7144-7187` |
| Hook schemas | A closed, compiled set of actions per schema (`identity_report`, `subagent_*`, `prompt_accepted`, `notification_create`); `admit_hook` is the one admission rule for both sockets | `crates/worker-protocol/src/hook_schema.rs:20-39,250-281` |
| Origin guard | Nine self-target mutations are refused when the envelope's origin pair names the target; "it is not authentication" | `docs/architecture.md:2216-2233`; `crates/daemon/src/api/handler/mod.rs:384-401` |
| Trust domain | One operator, no application-level authorization; prompt injection is inherited from the agents | `docs/architecture.md:2204-2239` |
| Secret env | `session.new` `secret_env`: names from the profile, values from the request, never persisted; resume and fork fail with `secret_env_missing` | `docs/architecture.md:2240-2251`; `docs/public-api.md:502` |
| Per-launch input | Only a compiled integration handler can add env pairs, root arguments and private files to a launch; files live in the worker state dir, outside the agent sandbox | `docs/knowledge/guides/runtime-packages.md:562-574`; `crates/daemon/src/integration/provider_handlers.rs:51-59` |
| Placement | `placement-v1` keeps an agent in the PTY tree (`in_process`) and runs a report-only escape check | `docs/architecture.md:1646-1682` |
| Agent rule on approvals | "Approvals go to the operator. An agent observes blocked and approval states and surfaces them; it does not answer them." | `docs/knowledge/guides/agent-skill.md:368-396` |
| Hermes policy | Read, `manage`, `full` tiers; fixed-argv CLI runner; "a delegated-tool guardrail, not a same-user sandbox" | `docs/architecture.md:1552-1593` |

---

## 2. Prior art

| Mechanism | Shape | Outcomes and lifecycle | Secrets | Timeout / no answer | Source |
|---|---|---|---|---|---|
| MCP elicitation 2025-06-18 / 2025-11-25 | Server-to-client `elicitation/create`; `form` mode with a flat `requestedSchema` (string, number, integer, boolean, enum, titled enum, multi-select array, defaults, formats); `url` mode with `elicitationId`; capability `elicitation: {form:{}, url:{}}` | `accept` (with `content`), `decline`, `cancel`; optional `notifications/elicitation/complete`; `-32042` URLElicitationRequiredError | "Servers MUST NOT use form mode elicitation to request sensitive information"; URL mode for secrets; the client shows the full URL and asks consent | Not specified | https://modelcontextprotocol.io/specification/2025-11-25/client/elicitation |
| MCP 2026-07-28 (current) MRTR | Server-initiated requests replaced: a `tools/call` returns `InputRequiredResult` with `inputRequests` and an opaque `requestState`; the client retries with `inputResponses` | Outcome learned by retry; `elicitation/complete` and `elicitationId` removed; `-32042` only in 2025-11-25 (schema comment, `main` branch, tag match UNVERIFIED) | Form still not for secrets; `requestState` must be integrity-protected (HMAC/AEAD) by the server | Not specified | https://modelcontextprotocol.io/specification/2026-07-28/changelog ; https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr |
| MCP tasks | 2025-11-25: `working`, `input_required`, `completed`, `failed`, `cancelled`, `ttl`; 2026-07-28: opt-in extension with `tasks/update` | Cooperative `tasks/cancel` | n/a | `ttl` is retention, not an answer deadline; client support UNVERIFIED for every runtime | https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/tasks ; https://modelcontextprotocol.io/extensions/tasks/overview |
| ACP permission | Agent-to-client `session/request_permission` with options of kind `allow_once`, `allow_always`, `reject_once`, `reject_always` | `selected` + `optionId`, or `cancelled` (mandatory when the turn is cancelled) | n/a | Not specified | https://agentclientprotocol.com/protocol/tool-calls (summary only, UNVERIFIED raw text) |
| ACP elicitation | `elicitation/create`, session- or request-scoped, `mode` always required; stabilized 2026-07-24 | `accept`/`decline`/`cancel`, unknown action = cancel; `session/close` cancels all | "Agents MUST NOT send credentials or tokens obtained through URL mode back over ACP", never into the model context; no fallback from URL to form (summary only, UNVERIFIED raw text) | Not specified | https://agentclientprotocol.com/announcements/elicitation-stabilized ; https://agentclientprotocol.com/protocol/v2/elicitation |
| A2A task states | `TASK_STATE_INPUT_REQUIRED`, `TASK_STATE_AUTH_REQUIRED` interrupt a task; the client continues with the same `taskId` | Nine states in 1.0 | "Agents MUST arrange to receive credentials via an out-of-band means" (section 7.6.1, summary, UNVERIFIED raw) | Not found in the part read | https://a2a-protocol.org/latest/specification/ |
| Claude Code `AskUserQuestion` | `questions[]` of `question`, `header`, `options[]` (`label`, `description`), `multiSelect`; answers map question text to label | A `PreToolUse` hook may return `permissionDecision: "allow"` plus `updatedInput` with `answers`; `defer` ends a `-p` turn | n/a | n/a | https://code.claude.com/docs/en/hooks (interactive behaviour of the answering hook UNVERIFIED) |
| Claude Code approvals | `PermissionRequest` hook returns `decision.behavior` `allow`/`deny` (with `updatedInput`, `message`, `interrupt`); `--permission-prompt-tool` names an MCP tool for `-p` | Deny and ask rules still apply | n/a | n/a | https://code.claude.com/docs/en/hooks ; https://code.claude.com/docs/en/cli-reference |
| Claude Code elicitation | Client for form and URL; `Elicitation` hook can `accept` with `content`, `decline`, `cancel`; only in interactive terminals | Introduced v2.1.76 per mirrors (UNVERIFIED against the official changelog) | n/a | n/a | https://code.claude.com/docs/en/mcp |
| Codex app-server | Server requests `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, `item/tool/requestUserInput` (experimental), `mcpServer/elicitation/request`, `item/permissions/requestApproval` | Client answers the JSON-RPC request; accept ran the command, cancel declined and interrupted (M-CX-14/15) | `requestUserInput` questions carry `isSecret` (`U:codex/codex-rs/protocol/src/request_user_input.rs:15-29`); the TUI only masks the field (`U:codex/codex-rs/tui/src/bottom_pane/request_user_input/render.rs:395-399`); the answer type is plain `answers: Vec<String>` (`request_user_input.rs:45-52`), so the value goes back to the tool call (inferred from the types, not traced into the model input) | n/a | `U:codex/codex-rs/app-server-protocol/src/protocol/common.rs:1843-1876`; `U:codex/codex-rs/app-server-protocol/src/protocol/v2/item.rs:1839-1864` |
| Codex hooks | `PermissionRequest` returns allow/deny; no documented hook answers a user question | | | | https://learn.chatgpt.com/docs/hooks (researcher, not reverified) |
| Cline | `ask_followup_question` and `ask_question` are on the CLI's auto-approve list; the hub emits `approval.requested` and a client with capability `approval.respond` answers it | `approval.resolved` | n/a | n/a | `C:apps/cli/src/runtime/tool-policies.ts:3-5`; CH-10; the official ask-tool schema, hook answering and MCP elicitation are UNVERIFIED |
| Pi | No built-in approval; extensions call `ctx.ui.select/confirm/input/editor`; RPC mode emits `extension_ui_request` (select, confirm, input, editor) | Answered by the RPC client | n/a | MCP tool `timeout` default 60 s, reset by progress | `U:pi/packages/coding-agent/src/core/extensions/types.ts:166-172,260`; `U:pi/packages/coding-agent/src/modes/rpc/rpc-types.ts:253-263`; `U:pi/packages/coding-agent/docs/mcp.md:1-3,68` |
| Muse Code | Hooks `PermissionRequest`, `Notification` (`permission_prompt`, `question_prompt`); MSP `session/statusChanged` with attention `approvalPending`/`inputPending`, lease-free `approval/listPending` | | n/a | n/a | `MUSE:71-72,78` |
| 1Password `op run` | Resolves `op://` references into the environment of a child command | | "Secrets printed to stdout or stderr are concealed by default"; `--no-masking` turns it off | n/a | https://www.1password.dev/cli/reference/commands/run (redirect from developer.1password.com) |

**What to take from it.**
- Three outcomes (`accept`/`decline`/`cancel`) are common ground; Pohunek adds `refined` and `expired` because
  it has an operator who can add context and a durable store that can time out.
- Every spec routes secrets out of band and forbids returning them through the protocol. Codex's
  `isSecret` is the counterexample: masked on screen, plain in the answer.
- No spec defines an answer deadline. Pohunek must own it.
- Server-initiated elicitation is unstable across MCP revisions. A tool that returns a result works in every
  revision and in every client.

---

## 3. The primitive: an ask

### 3.1 Record

| Field | Meaning | Bound (config, `service.toml` `[ask]`) |
|---|---|---|
| `id` | `ask-<ulid>`, assigned by the daemon | |
| `session_id`, `runtime` (worker instance id + generation) | The owner of the ask, set from the attested caller, never from the payload | |
| `origin` | `tool` (MCP), `cli`, `routed` (a native prompt through a hook: provider, native kind), `backend` (a protocol backend server request) | |
| `title` | One line | `title_max_bytes` |
| `question` | Plain text | `question_max_bytes` |
| `context` | Plain text explanation or quoted evidence, rendered as untrusted | `context_max_bytes` |
| `items[]` | One to `max_items` items, each with `id`, `label` and a `kind` | `max_items` (4 matches Claude's `questions[]`) |
| `blocking` | Whether the asker waits; a non-blocking ask is collected later with `ask_wait` | |
| `importance` | `normal` (default) or `important`: an important ask has no `expires_at`, waits until answered, and re-notifies on the escalation schedule | `max_important_pending_per_session`, `important_renotify_interval` |
| `expires_at` | Absolute deadline; the request may shorten it, never lengthen it past the ceiling; absent for an `important` ask | `default_ttl`, `max_ttl` |
| `on_expire` | `cancel` (default) or `default` (only when every item declares a default and no item is `secret` or a `confirm` marked `destructive`; a routed or backend ask is always `cancel`, because the daemon never approves for the owner) | |
| `answerers` | Who may answer, from the rights model: `owner` always; `parent` only when granted (section 3.4) | |
| `revision`, `state`, `outcome` | See 3.3 | |

An `on_expire = default` outcome is the asker's own declared choice, applied by the store when the deadline
passes; it is not an answer given on the owner's behalf, and it never applies to routed or backend asks,
`secret` items or a `destructive` `confirm`.

### 3.2 Answer kinds

| Kind | Request | Answer | Notes |
|---|---|---|---|
| `choice` | 2..`max_options` options (`id`, `label`, optional `description`), optional `default`, `allow_other` | one option id, or `other` with text | `allow_other` covers Codex `isOther` and Claude's free-text "Other" |
| `multi` | as `choice`, plus `min`, `max` | option ids (+ `other`) | Claude `multiSelect`, MCP multi-select arrays |
| `text` | `max_bytes`, `multiline`, optional `placeholder` | text | The answer is operator text and reaches the model as data |
| `confirm` | `destructive: bool`, optional `default` | `yes` / `no` | Approvals map here; `destructive` disables `on_expire = default` and any non-owner answerer |
| `secret` | `slot` (a name the operator sees), `purpose`, `scope` (section 6) | a **reference** only: `{ref, env_name?, expires_at}` | Must be the only item of its ask; never routed to a non-owner surface |

### 3.3 States and outcomes

```
pending ──answer──────────▶ closed(answered)
   │   ──refine───────────▶ closed(refined)      operator note + optional partial values
   │   ──decline──────────▶ closed(declined)     operator refuses; optional reason
   │   ──cancel by asker──▶ closed(cancelled: asker)
   │   ──runtime ended────▶ closed(cancelled: runtime_ended)
   │   ──native answered──▶ closed(cancelled: answered_natively)   routed asks only (4.4)
   │   ──hook gone────────▶ closed(cancelled: hook_timeout)        routed asks only (4.4)
   │   ──decided elsewhere▶ closed(cancelled: resolved_elsewhere)  routed asks only (4.7)
   └── deadline ──────────▶ closed(expired)  or closed(answered: default) per on_expire   (never for important)
closed ──collected──▶ closed + collected_at     the asker read the outcome
```

- **Refine is an outcome, not a loop.** The operator writes a note ("use the staging DB, not prod"), may
  pick values too, and sends it. The agent receives `outcome: refined` with the note and decides whether to
  ask again. No thread is kept, so the channel cannot drift into a chat. A new ask may name the previous one
  (`follows: <id>`) so surfaces can show the pair.
- **One winner.** The first accepted terminal transition wins under the daemon's per-ask lock; later answers
  get `ask_closed` with the winning outcome.
- **Durability.** Asks persist in a new owner-private store beside the notification store
  (`crates/protocol/src/notification.rs:188` is the shape to follow). A daemon restart keeps pending asks
  pending; the MCP server re-attaches with `ask_wait`. A new runtime generation (stop, resume, fork) closes
  the old generation's blocking asks as `cancelled: runtime_ended`, because no process is waiting any more.
- **Important asks.** They still close on cancel, runtime end and hook exit; only the clock is removed. A
  routed ask is never `important` beyond the hook's own timeout (4.4), because the provider stops waiting
  first. A cap (`max_important_pending_per_session`) keeps one session from pinning many.
- **Retention.** Closed asks are kept for a bounded period (`closed_retention`), metadata only for `secret`
  items (never a value).

### 3.4 Who may answer

| Answerer | `choice` / `multi` / `text` | `confirm` (approval) | `secret` |
|---|---|---|---|
| Owner, through an owner surface (CLI outside every session, companion, web) | yes | yes | yes |
| The asking session | no | no | no |
| Its parent (manager) session | only when the manager's agent settings grant it (below) | no | no |
| Any other session | no | no | no |

The "no" cells follow the existing rule that agents surface approvals and never answer them
(`docs/knowledge/guides/agent-skill.md:395-396`). How the daemon tells an owner surface from a session is
the same caller classification Part B needs (section 9.4); its limits apply here too.

**The manager grant lives in agent settings.** The owner decides per agent, not per host: the host profile
the manager session launched with declares `[ask] answer_children = ["choice", "text"]` (any subset of
`choice`, `multi`, `text`; `confirm` and `secret` are refused at install), and a runtime package descriptor
may only narrow it for its runtime. There is no global switch. The grant applies only to the manager's own
children (the parent link of 9.2), so it ships with Part B's parent link and the manager grant (AC-18, AC-19).

**Enforcement before Part B.** Slice AC-02 adds `ask.answer`, `ask.refine` and `ask.decline` to the
origin-session guard (`crates/daemon/src/api/handler/mod.rs:384-401`), refused whenever origin markers
are present at all, not only when they name the target. That has the same strength as today's guard ("not
authentication", `docs/architecture.md:2231`): a CLI started inside a session carries the markers and is
refused, a process that drops them is not. Slice AC-20 replaces the markers with the kernel peer.

**The owner's own shell.** A `shell` runtime session is often the owner's own terminal. Its default
ceiling is owner-equivalent, so the owner can answer from it; a nested agent started inside that shell
(`docs/architecture.md:1329-1336`) then shares the shell session's rights. That is a documented limit:
run agents in their own sessions, not inside a managed shell, when rights matter. The owner-equivalent
ceiling exists from AC-17 and AC-20 on; between AC-02 and then, the owner answers from outside every session.

### 3.5 How the answer returns

1. **MCP tool result** (section 4.1): `ask` returns the outcome, or `pending` after its slice.
2. **CLI**: `pohunek ask ... --wait` prints the outcome JSON and exits with a typed code.
3. **Routed hook**: the hook process waits on the worker socket and turns the outcome into the provider's
   hook output (section 4.4).
4. **Protocol backend**: the backend answers the provider's JSON-RPC request.

The answer never travels as typed PTY input. An operator who wants the agent to act on an answer it did not
collect (the agent stopped waiting) uses an explicit "send as prompt" action that goes through
`session.input` and its blocked-state rules; that is an operator action, not part of the ask.

### 3.6 Nobody answers

- The deadline closes the ask as `expired` (or the declared default). An `important` ask has no deadline: it
  stays pending, and the attention record is re-sent every `important_renotify_interval` until answered,
  cancelled or the runtime ends. The agent gets that outcome like any
  other, and its instructions (the Pohunek skill) say what to do: stop and report, or continue with a
  documented safe choice.
- The attention surfaces show an unanswered blocking ask as `agent_blocked` with severity `action_required`;
  escalation follows the notification policy (section 5.4).
- Rate and count limits refuse further asks (`ask_limit_exceeded`) rather than queueing without bound:
  `max_pending_per_session`, `create_rate` per session.

---

## 4. How an agent emits an ask

### 4.1 The Pohunek MCP server (primary)

- `pohunek mcp` is a stdio MCP server the runtime starts as its child, so it runs inside the session's
  PTY tree. It connects to the worker socket it finds in the session environment
  (`POHUNEK_WORKER_SOCKET_PATH`, already injected; `crates/session-worker/src/server.rs:7397-7400`), and the
  worker attests the caller by kernel peer and ancestry like a hook (`crates/session-worker/src/server.rs:7144-7187`).
  The server sends no session id of its own; the worker binds the request to its session.
- Tools: `ask` (create; blocks up to `wait_slice`, sending MCP progress notifications every
  `progress_interval`), `ask_wait(id)` (same blocking rule), `ask_cancel(id)`, `ask_get(id)`. Results are
  structured content plus a one-line text summary for clients that ignore structure.
- Elicitation is not used in core. It would show the question inside the agent's own TUI, which is what the
  channel replaces, and 2026-07-28 replaced it with MRTR. A later optional mode may answer an `ask` call
  with MRTR `input_required` where a client negotiates it.
- Registration per launch is core work: the runtime's compiled handler adds the server through per-launch
  input (`docs/knowledge/guides/runtime-packages.md:562-574`), never by editing the owner's agent config.

### 4.2 Tool timeouts decide the blocking pattern

| Client | Default MCP tool timeout | Progress resets it | Per-server override | Source |
|---|---|---|---|---|
| Claude Code | `MCP_TOOL_TIMEOUT` 100000000 ms (about 28 h); stdio has no per-request timer | n/a | env, per-server `timeout` | https://code.claude.com/docs/en/env-vars |
| Codex | 300 s (`DEFAULT_TOOL_TIMEOUT`) | UNVERIFIED | `tool_timeout_sec` | `U:codex/codex-rs/codex-mcp/src/rmcp_client.rs:121-122`; `U:codex/codex-rs/codex-mcp/src/connection_manager.rs:522-526` |
| Pi | 60 s | yes | per-server `timeout` | `U:pi/packages/coding-agent/docs/mcp.md:68` |
| Cline | UNVERIFIED | UNVERIFIED | UNVERIFIED | https://docs.cline.bot/mcp/mcp-overview |
| Muse | UNVERIFIED | UNVERIFIED | UNVERIFIED | |

So `wait_slice` stays below the smallest timeout of the runtime that launched the server (the handler knows
which runtime it is), and the tool returns `{status: "pending", ask_id}` with an instruction to call
`ask_wait`. Where the runtime allows a per-server timeout, the handler raises it in the per-launch config.

### 4.3 `pohunek ask` CLI

Same request, same worker-attested path, for shell scripts, hooks and runtimes without MCP. `--wait`
blocks with the same slice rule; `--json` prints the outcome. It is also the test harness for the primitive.

### 4.4 Routing native prompts (hooks)

| Runtime | Native prompt | Route without screen scraping | Status |
|---|---|---|---|
| Claude Code | `AskUserQuestion` | `PreToolUse` matcher `AskUserQuestion` creates a routed ask and returns `allow` + `updatedInput.answers` | **Conflicting evidence:** one reading of https://code.claude.com/docs/en/hooks documents this answer path; a second reading (2026-11-06) states that `AskUserQuestion` "always go[es] to the callback or dialog". Unconfirmed until measured (the AC-06 measurement) |
| Claude Code | Tool approval | `PermissionRequest` hook returns `decision.behavior` | Documented (same page) |
| Claude Code | MCP elicitation from other servers | `Elicitation` hook answers `accept`/`decline`/`cancel` | Documented; not in non-interactive runs (https://code.claude.com/docs/en/mcp) |
| Codex (TUI) | Approvals | `PermissionRequest` hook | https://learn.chatgpt.com/docs/hooks (researcher, not reverified) |
| Codex (TUI) | `request_user_input` | none: no hook answers it | same source |
| Codex (app-server) | Approvals, `requestUserInput`, MCP elicitations | Server requests answered by a backend | `U:codex/codex-rs/app-server-protocol/src/protocol/common.rs:1843-1876` (needs #1561) |
| Cline (hub) | Approvals | `approval.respond` from a registered client | CH-10 (needs #1561) |
| Cline (TUI) | `ask_followup_question` | UNVERIFIED (hook answering not confirmed) | |
| Muse | Approvals | `PermissionRequest` hook (answer shape UNVERIFIED); MSP `approval/listPending` | `MUSE:71,78` |
| Pi | Extension dialogs | A Pohunek Pi extension routes `ctx.ui.*` itself; RPC `extension_ui_request` in a backend | `U:pi/.../types.ts:166-172`; `rpc-types.ts:253-263` |

**Hook timeouts.** A routed hook holds the native prompt only while the hook process lives. Claude command
hooks default to a 600 s `timeout` per hook, set in seconds on each handler; a timed-out PreToolUse hook
"doesn't block the tool call" and the call "continues through the normal permission flow" (summary of
https://code.claude.com/docs/en/hooks; verbatim PreToolUse and PermissionRequest timeout text UNVERIFIED).
Codex and Muse hook timeouts are UNVERIFIED. Rules:
- The handler sets the hook `timeout` to the ask's TTL where the runtime allows it, and the routed ask's
  `expires_at` never exceeds the hook's timeout.
- When the hook exits or times out before an answer, the ask closes as `cancelled: hook_timeout`, the input
  refusal of 4.5 lifts at once, and the attention record points to attach. The native dialog is then the
  only answer source.

**Single winner between the channel and the terminal.** A routed hook holds the native prompt open while
the ask is pending. If the operator answers in the terminal (attach) instead, the native UI resolves, the
hook's wait is cancelled, and the ask closes as `cancelled: answered_natively`. If the channel answers first,
the hook returns the answer and the native dialog never appears (Claude) or closes. Exactly one source
answers, and the ask records which.

### 4.5 Relation to screen detection (`[detect]`)

- A pending **blocking** ask of the session's current runtime generation is structured `blocked` evidence
  with its own source (a new `StateSource` value behind a feature, or the existing `report`), and it refuses
  `session.input` for that session like `refuse_while_blocked` (`docs/public-api.md:514`). That closes
  #1968's failure for every routed prompt: typed text cannot answer a question the channel holds, because the
  daemon knows a question is open whatever the screen shows.
- It replaces screen rules **only** for prompts a runtime routes. Prompts with no route (Codex TUI
  `request_user_input`, Cline TUI questions until verified, trust dialogs, login screens) still need
  `[detect]`, and the fail-closed matcher of #1968 is still needed for them.
- Over time protocol backends (#1561) remove the screen from the approval path for runtimes that have one.

### 4.6 Per-runtime summary

| Runtime | MCP tool today | Routed native prompts today | Blockers |
|---|---|---|---|
| Claude Code | yes (stdio, `--mcp-config`) | `AskUserQuestion`, approvals, elicitations | handler adds `--mcp-config` + hooks per launch |
| Codex | yes (`[mcp_servers]`, 300 s) | approvals via hook; not `request_user_input` | `placement-v1` `--no-daemon` already keeps it in tree (`runtime-packages/codex/runtime.toml:18-32`) |
| Pi | yes (stdio/HTTP; user or trusted project config) | through a Pohunek extension | per-launch MCP config path UNVERIFIED |
| Cline | yes (stdio) | hub approvals need #1561 | hub runs tools outside the PTY tree; attestation fails there |
| Muse | `mcp_servers` in settings (`MUSE:64`) | `PermissionRequest` | hooks run with a cleared environment (`MUSE:71,140`); whether MCP children also lose `POHUNEK_*` is UNVERIFIED, so the handler-written MCP entry passes the socket as an argument (`pohunek mcp --worker-socket <path>`) and needs no environment |


### 4.7 Native permission prompts

**The owner's question:** "will this work universally? What if agent need for example permission which is
still native".

**Plain answer.** No, the tool alone does not cover every case. The `ask` tool and `pohunek ask` cover the
questions the *agent decides* to ask. A permission prompt ("allow this command?") is raised by the *runtime
itself*, before the tool runs, and the model cannot send it anywhere. Those prompts reach the channel only
where the runtime offers an outside answerer: a hook that returns a decision, a permission-prompt tool, or a
protocol (app-server, ACP). Where it does, Pohunek turns the prompt into a routed ask (`origin: routed`,
kind `confirm`, the runtime's options as choices), the owner answers in the same apps, and the hook or
protocol hands the decision back to the runtime. Where it does not, the prompt stays in the terminal and
screen detection (`[detect]`) remains how Pohunek notices it. Verified per runtime today:

| Runtime | Interactive TUI (how Pohunek runs it) | Headless / protocol mode | No answer or timeout | Stays on screen detection |
|---|---|---|---|---|
| Claude Code | **Routable.** `PermissionRequest` hook "runs when Claude Code is about to ask you for permission"; returns `hookSpecificOutput.decision.behavior` `allow` (optional `updatedInput`, `updatedPermissions`) or `deny` (optional `message`, `interrupt`); a hook `allow` "doesn't override a matching deny rule". The `AskUserQuestion` dialog would be routed separately through `PreToolUse` + `updatedInput` (4.4), which is unconfirmed | `--permission-prompt-tool mcp__<server>__<tool>` ("an MCP tool to handle permission prompts in non-interactive mode"); `--permission-prompts none` denies instead; SDK `canUseTool` | A hook that returns no `decision` "leaves the permission flow unchanged", so the dialog appears; command hooks default to 600 s; per-prompt timeout of `--permission-prompt-tool` UNVERIFIED | `AskUserQuestion` until its route is measured (conflicting evidence, 4.4); prompts a hook `allow` cannot approve: critical-path `rm`/`rmdir` and "Dangerous rm" (whether the hook still fires is UNVERIFIED), MCP tools that require user interaction, Monitor-tool shell commands ("its `allow` and `updatedInput` have no effect on them"), plan-mode edits |
| Codex | **Routable.** `PermissionRequest` "runs when Codex is about to ask for approval, such as a shell escalation or managed-network approval"; matchers include `Bash`, `apply_patch`, `mcp__server__tool`; `decision.behavior` `allow`/`deny` (deny takes `message`); "an `allow` lets the request proceed without surfacing the approval prompt"; `updatedInput` etc. must not be returned | `codex app-server`: server requests `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, `item/permissions/requestApproval`, decisions `accept`, `acceptForSession`, `decline`, `cancel` (`U:codex/codex-rs/app-server-protocol/src/protocol/common.rs:1843-1876`; https://learn.chatgpt.com/docs/app-server); `codex exec` has no outside answerer found (UNVERIFIED negative) | "If no matching hook decides, Codex uses the normal approval flow"; hook timeout and the deny-dialog behaviour UNVERIFIED | `request_user_input` questions (no hook answers them); anything no matcher covers |
| Cline | **Not routable from the TUI** as far as verified: hooks can cancel or block a tool, no hook answering a pending approval was found (https://docs.cline.bot/sdk/plugins.md; hook reference only via search, UNVERIFIED) | **Routable through ACP:** `cline --acp` approves "through the client's permission UI" (https://docs.cline.bot/usage/acp); the CLI sends ACP `requestPermission` with `allow_once`, `allow_always`, `reject_once` (`C:apps/cli/src/acp/permissions.ts:17-29,113-115`); the hub's `approval.respond` too (CH-10) | ACP gives no timeout; a cancelled turn MUST answer `cancelled` (https://agentclientprotocol.com/protocol/tool-calls) | Every TUI approval and question until Pohunek runs Cline as an ACP session (#1561) |
| Muse Code | **Unverified.** A `PermissionRequest` hook exists (`MUSE:71`), its output shape is undocumented; interactive grants are "allow once, always allow in this workspace, or reject", and the built-in judge may decide first (https://dev.meta.ai/docs/muse-code/permissions) | MSP `approval/listPending` and attention `approvalPending` (`MUSE:78`); whether a client can answer an approval over MSP is UNVERIFIED | UNVERIFIED | All of it, until the hook or MSP answer path is verified |
| Pi | **Nothing to route:** Pi "does not ask for approval before every tool call" (https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/security.md); gates come from extensions, which a Pohunek extension can route (`U:pi/packages/coding-agent/src/core/extensions/types.ts:166-172`) | RPC `extension_ui_request` (`U:pi/packages/coding-agent/src/modes/rpc/rpc-types.ts:253-263`) | Extension-defined | The project trust prompt ("Print, JSON, and RPC modes cannot show the built-in trust prompt") |

Sources for the Claude and Codex rows: https://code.claude.com/docs/en/hooks ,
https://code.claude.com/docs/en/cli-reference , https://code.claude.com/docs/en/agent-sdk/permissions ,
https://learn.chatgpt.com/docs/hooks . Verified 2026-11-06 by a read of the current pages; quoted phrases
are verbatim, everything marked UNVERIFIED was not readable verbatim.

**Consequences for the design.**
- Routing is per runtime and per prompt type, so it lives in the runtime's core handler (Claude and Codex
  first), with a compatibility lock entry per provider version, like the hooks of #228.
- A routed approval is `confirm` with the provider's own options (allow once, allow always, deny, deny with
  message) as choices. "Always allow" changes the runtime's own permission rules, so it is offered only when
  the provider offers it and only to the owner.
- **The routed hook is the waiting point.** A `PermissionRequest` hook runs "when Claude Code is about to
  ask you for permission", so the dialog most likely does not render while the hook waits (to be measured
  in the AC-06 measurement, together with whether another hook, rule or mode decides while the routed hook waits). The task RFC's rule that a decision request "is not waiting" (`delegated-task-runs-rfc.md`, section 8.5)
  still holds: another hook, a deny rule or the mode can decide first ("any `deny` wins" in Codex). The ask
  therefore records what the provider actually did. A decision made elsewhere closes it as
  `cancelled: resolved_elsewhere`, the owner's answer is not applied, and the attention record is marked `resolved_elsewhere`.
- The hook's decision is final in the provider only where the provider says so. Deny rules, critical-path
  prompts and plan mode in Claude still show the dialog; the ask then closes as `answered_natively` or
  `hook_timeout` (4.4), and screen detection reports the dialog as today.
- Routing removes screen detection only for routed prompts. The `[detect]` rules and the fail-closed
  matcher of #1968 stay for every prompt in the last column.
- Protocol backends (#1561: Codex app-server, Cline ACP, Muse MSP, Pi RPC) are the path to cover the rest;
  each one turns server requests into routed asks the same way.

---

## 5. Surfaces

### 5.1 CLI

`pohunek ask list [--session] [--pending]`, `pohunek ask show <id>`, `pohunek ask answer <id> ...`,
`pohunek ask refine <id> --note ...`, `pohunek ask decline <id>`. Answers in a terminal are prompted
interactively (no value on argv for `secret`; the value is read from the TTY without echo). The CLI refuses
`answer` when it runs inside a managed session unless that session holds the matching right (section 3.4).

### 5.2 Companion (pohunekd/pohunek-companion)

The sidebar that "shows which coding-agent sessions need you" (repo description) gets an ask card: provenance
header (session, agent, project, how it was asked), title, question, a collapsed context block, the item
controls, and Answer / Refine / Decline. A secret item opens a dedicated entry dialog that names the slot and
the destination vault, never the agent's text as the label.

### 5.3 Web

The same card over the overlay owner path. A secret item is accepted remotely only under the conditions of
6.5 (owner answer Q8).

### 5.4 Notifications

A new ask produces an attention record (kind `approval_required` for `confirm`, `agent_blocked` otherwise,
until a dedicated `ask_pending` kind exists behind tolerant decoding,
`crates/protocol/src/notification.rs:21-49`). Closing the ask updates the record. A routed ask produces its record, and its
push, only once its hook is still live after the confirmation window of the task RFC (section 8.5), so a
prompt that another hook or rule decides at once never notifies the owner. Desktop and phone pushes
carry the title and the session only, never the context or any value.

### 5.5 "Extend the question"

Refine (3.3) is the extension: the operator adds context or corrects the premise and returns it as the
outcome. Optionally the operator can also edit the options into a new set and answer from it; the outcome
then carries `refined` with `values` from the operator's set, and the agent sees both the note and the
values. No back-and-forth is held open.

---

## 6. Vault: the `secret` answer kind

### 6.1 Flow

1. The agent creates an ask with one `secret` item: `slot` ("GitHub token for repo X"), `purpose`, `scope`.
2. The operator sees which session asks and where the value will go, and either picks an existing vault
   entry or enters a new value.
3. The value is written to the configured vault backend by the daemon (or the vault plugin's node agent),
   never by the agent. The daemon records only metadata: ask id, slot, backend, reference, scope.
4. The session receives `{ref: "pohunek-secret:<id>", env_name, expires_at}`.

### 6.2 Getting the value into a process without the model seeing it

`secret_env` is launch-only and resume refuses it (`docs/architecture.md:2240-2251`); the owner already hit
that with the Muse profile. A secret answered mid-session therefore cannot appear in the running
agent's environment. Two ways remain:

| Way | How | Leak surface |
|---|---|---|
| `pohunek secret exec --ref <ref> --env NAME -- <cmd> [args]` (recommended) | The CLI fetches the value over the attested worker socket (only for a reference of its own session) and runs the command **in place** as its own child with `NAME=<value>`, so the command keeps the runtime's sandbox and the PTY tree (stop and cleanup still reach it); the CLI masks the exact value and its common encodings in the stdout/stderr it passes on, as `op run` does ("Secrets printed to stdout or stderr are concealed by default", https://www.1password.dev/cli/reference/commands/run) | The value sits in the CLI's memory inside the agent's tree; the command can print or send it; same-UID processes can read the child's `/proc/<pid>/environ` (M1). The model context and transcripts never hold it |
| Next launch | The reference resolves into the profile's `secret_env` slot at every create, resume and fork | Same as today's `secret_env`; fixes the resume refusal for referenced slots |

A file handle (a 0600 file whose path the agent gets) is rejected: the agent's next step is often `cat`.
A worker-spawned command is rejected too: it would run outside the runtime's sandbox and the PTY tree, the
kind of sandbox escape core already avoids for the Muse overlay (`docs/architecture.md:1686-1691`).

### 6.3 Backends

Presence on the owner's machine (`command -v` only, nothing opened): `op` yes, `secret-tool` (Secret Service)
yes, `kwallet-query` yes, `systemd-creds` yes, `sops` yes; `bw`, `bws`, `pass`, `gopass`, `age`,
`agent-keychain` no.

| Backend | Fit | Notes |
|---|---|---|
| `pohunekd/plugin-vault` | Target | Empty repository; described as "an authority with signed, group-encrypted records and a node agent that resolves secret references at session launch". The ask channel should produce its references, not a parallel format |
| Secret Service (`secret-tool`, GNOME Keyring / KWallet) | Default local backend on Linux | Assumption, not measured: needs an unlocked user session, so headless hosts lack it |
| macOS Keychain | Default local backend on macOS | Assumption, not measured |
| 1Password (`op`) | Optional | References `op://...` map directly onto handles; service accounts for headless hosts (`KC:21-23`) |
| sops + age | Headless, many hosts | Recommended by the keychain study for unattended hosts (`KC:21-23`) |
| agent-keychain | No | Single host, polkit seat, sync out of scope (`KC:9-11`) |

**Order (owner answer Q6).** The daemon picks the first available backend in this order: (1) plugin-vault,
when its plugin is installed and its node agent is reachable; (2) the system keychain (Secret Service on
Linux, Keychain on macOS); (3) a password manager plugin (1Password first). A host profile may pin one
backend explicitly. When none is available a secret ask is refused at create (`ask_secret_backend_unavailable`),
never stored in a fallback location. A reference records its backend, so a later change of order never
moves an existing secret.

The core defines a `SecretBackend` seam (store, resolve, revoke, list metadata) and ships the OS keychain
backends; other backends are plugins. The value type reuses the redacting `SessionSecretEnv` handling
(`crates/protocol/src/secret_env.rs:93-120`).

### 6.4 Scope, audit, residual risk

- A reference is bound to the asking session (and optionally its children), expires
  (`secret_ref_ttl`), and can be single-use. A reference used by any other session is refused.
- Audit: every create, answer, resolve, exec and revoke is a structured log line with ask id, session,
  slot and backend; never a value, a digest of a value or the command's output.
- Residual risk: an agent with the reference can run `secret exec` with a command that sends the value
  out. Scope and single use limit it; a per-reference command allowlist is a possible later addition.

### 6.5 Secret entry from any owner device (owner answer Q8 = b)

The owner chose to allow entry from any owner device over the overlay. Today the remote listener relies on
the overlay alone: it binds to the overlay address and "the overlay's policy ... decides which peers can
reach the daemon port", with no application-level authorization (`docs/architecture.md:2204-2215`). That
is enough for reading sessions, not for writing a credential. Remote entry therefore ships only with these
conditions, each a DoD item of its slice:

1. **Authenticated operator device.** The daemon attributes each inbound overlay connection to an overlay
   peer identity (the overlay layer already models provider peer identities,
   `crates/overlay/src/lib.rs:35,221-275`; inbound attribution is new work) and accepts a secret answer
   only from a peer on an owner-maintained device allowlist (`[ask] secret_entry_devices`). Local Unix
   connections from outside every session count as the owner's desktop.
2. **Fresh presence on the device.** The surface re-confirms the owner on the device before sending (OS
   unlock or a passkey/biometric prompt in the companion; a re-typed confirmation in the web client).
3. **Bound, single-use entry.** The answer carries the ask id, its revision and a daemon nonce issued when
   the ask was opened on that device; a second use, a stale revision or another device's nonce is refused.
4. **Transport.** Only the overlay (WireGuard in NetBird and Tailscale); never the relay, never a push
   notification payload, never a URL query. The value lives in the redacting secret type from request parse
   to backend write.
5. **Audit.** One structured line per entry: ask id, session, slot, backend, device peer identity, outcome.
   Never the value or a digest of it.
6. **Write-only.** A remote device can enter a value or pick an existing entry by name; it can never read a
   stored value back.
7. **Limits.** A per-device rate limit on secret answers, and a doctor check that warns when the allowlist
   is empty while remote entry is enabled.

---

## 7. Security of the ask channel

| Threat | Control |
|---|---|
| A process claims to be another session | Session binding comes from the worker's kernel-peer ancestry check, never from the payload (`crates/session-worker/src/server.rs:7160-7187`); a process outside the tree gets no session (it is classified as the owner path, section 9.4) |
| Spoofed UI chrome or instructions in the question | Agent text is shown only inside a framed "written by the agent" region with the provenance header outside it; plain text, no markdown/HTML; options are labels, not actions |
| Terminal escapes and bidi tricks | Reject C0/C1 controls except LF and tab and the Unicode bidi override and isolate controls at create (`ask_text_invalid`); the C0/C1 part matches Hermes-safe text (`docs/public-api.md:514`), the bidi part is new; CLI rendering escapes anything left |
| Phishing for secrets | The secret dialog names the destination vault and slot from daemon data; the value never returns to the agent; a secret ask is refused unless the session holds the `ask.secret` right |
| Flooding the operator | `max_pending_per_session`, `create_rate`, `max_items`, size bounds; refusals are typed |
| Answering one's own ask, or a manager approving for a worker | The answerer table (3.4); answer methods check the caller class |
| Typed text answering a routed dialog | A pending routed ask refuses `session.input` (4.5) |
| Remote hosts | Asks are host-local records; remote surfaces reach them over the overlay owner path like every other method; a secret entered remotely needs the conditions of 6.5 (owner answer Q8) |
| Logs and transcripts | Question and context text are in the ask store only, not in daemon logs; secret values are never in either |

---

## 8. Core versus plugin (Part A)

| Core (zajca/pohunek) | Plugin or other repository |
|---|---|
| Ask protocol types and methods (`ask.create/get/wait/cancel/list/answer/refine/decline`), the store, limits, the state machine, events | Companion ask card (pohunek-companion), web card |
| Worker request kinds for attested asks; blocked evidence and input refusal for pending asks | Vault backends other than the OS keychains (plugin-vault, 1Password, sops) |
| `pohunek ask` and `pohunek mcp` in the CLI binary | Runtime plugins (plugin-cline, plugin-muse-code) wire their handler contributions once core offers the seam |
| Per-launch MCP registration and routed hooks in the core handlers (`claude-hook-v1`, `codex-hook-v1`) | |
| The `SecretBackend` seam, `secret exec`, reference resolution in `secret_env` | |

This follows "core owns what plugins build on" (`docs/architecture.md:1693-1700`): the MCP server is
core because attestation and rights are core; a plugin cannot be the thing that enforces them.

---

## 9. Part B: a Pohunek MCP server for agents, with per-session rights

### 9.1 What agents do through the CLI today

| Today (CLI) | Who | MCP mapping |
|---|---|---|
| `host list`, `session list`, `doctor` | managers, any agent | tools `hosts_list`, `sessions_list`; resource `pohunek://sessions` |
| `session inspect`, `screen`, `read`, `output`, `detection` | managers reading workers; workers reading themselves | `session_inspect`, `session_screen`, `session_read`; resource `pohunek://session/self` |
| `subscribe`, `notifications watch`, `session wait` | managers following workers | `session_wait` (bounded slices, as 4.2), `notifications_list` |
| `session new --input`, `session input --stdin` | managers spawning and briefing workers | `session_spawn` (profile, project, branch, brief text), `session_input` |
| `session stop`, `diff`, `rm`, `fork` | managers, with owner confirmation | `session_stop`, `session_diff`; `session_remove` only with `full` and an ask-confirm |
| `session input <manager> "[worker] DONE ..."` | workers reporting to managers | `report_to_parent` (bounded text) instead of raw input to an arbitrary target |
| Hooks report identity, subagents, notifications | runtime hooks | stays on the hook path; not an MCP tool |
| (none) | any agent | `ask`, `ask_wait`, `ask_cancel` (Part A) |

Sources: `docs/knowledge/guides/agent-skill.md:31-335`; the reporting command of the same guide.

### 9.2 Rights model

**Capabilities** (closed set, compiled into core, like hook actions `crates/worker-protocol/src/hook_schema.rs:20-39`):

| Capability | Allows |
|---|---|
| `self.read` | inspect, screen, read, wait on the caller's own session |
| `ask` | create asks (`choice`, `multi`, `text`, `confirm`) |
| `ask.secret` | create `secret` asks and use `secret exec` on its own references |
| `children.spawn` | create sessions, which become its children; with a ceiling on count, profiles and projects |
| `children.read` | read its children |
| `children.input` | send input to, and wait on, its children |
| `children.stop` | stop its children |
| `ask.answer.children` | answer its children's `choice`/`multi`/`text` asks; derived from the manager's agent settings `[ask] answer_children` (3.4), never granted by a parent |
| `report.parent` | send a bounded report to its parent |
| `sessions.read` | read sessions it does not own |
| `host.read` | hosts list, doctor |
| `remote` | any of the above against another host |

**Who grants.**
1. The operator sets a **ceiling** per host profile (`[rights] allow = [...]`), and a package descriptor may
   only lower it for its runtime. The default ceiling is `self.read`, `ask`, `report.parent`.
2. A session's set at launch is the ceiling, or less when the parent spawns it: a parent grants only a
   subset of its own set (monotonic narrowing), so a worker never ends up with more than its manager.
3. The operator can change a running session's set (`session.rights.set`, owner-only); a session can only
   drop its own rights.
4. Nothing is granted by a session's own request, a repository file or text in a prompt.

**Ownership.** "Children" means sessions created through `session_spawn` by this session, recorded as an
immutable `parent_session_id` on the child's record. Today's "managers" spawn through the CLI with no such
link; AC-18 adds it for sessions created on the attested path.

**Revocation.** Changing a set takes effect on the next request; stopping a parent does not stop its
children but revokes the parent's rights over them; a resumed session re-derives its set from the profile
ceiling and its parent's current set.

**Audit.** Every rights-checked request logs session, capability, method, target, allow or deny, and a
request id; never prompt text or output.

### 9.3 Attestation: how the server knows which session calls

- **Per-session server, worker-attested.** The runtime starts `pohunek mcp` as its child (stdio). The
  server talks only to the worker socket from its environment. The worker accepts a request only from a
  process inside its PTY tree (kernel peer plus ancestry, `crates/session-worker/src/server.rs:7144-7187`)
  and forwards it to the daemon bound to its own session id. No token crosses the environment, so there is
  nothing a child can copy and replay elsewhere: a process that leaves the tree loses the session binding,
  and a process in another session's tree is that other session.
- **Why not a token in the environment.** M1 shows that any same-UID process can read another process's
  `/proc/<pid>/environ`, so an env token is readable by every session on the host. A token is only needed
  where the kernel peer is unavailable (a remote host's server, a shared daemon), and there it must be a
  per-launch capability bound to the worker generation, never a shared secret.
- **Shared daemons.** Where the runtime runs its MCP children from a shared host daemon (Codex app-server,
  Cline hub), the server is outside the PTY tree and the worker refuses it. `placement-v1` `in_process`
  (#1947) is therefore a prerequisite for rights on such runtimes, or a protocol backend (#1561) that owns
  the server per session.
- **Every process in the tree is the session.** Tools the agent runs, subagents and scripts share its rights;
  the PTY tree is not an identity boundary inside a session (`docs/architecture.md:1747`).

### 9.4 The bypass, and how far it can be closed

| Path | Today | With AC-20 |
|---|---|---|
| The MCP server through the worker | n/a | rights enforced |
| `pohunek` CLI inside the session against the owner socket | full owner rights, minus the nine self-target mutations | the daemon resolves the kernel peer of each public-socket connection; a peer inside a managed session's tree gets that session's rights (precedent: `session.report_native_id` already attests its caller by the local connection's kernel peer, `docs/architecture.md:2224-2228`) |
| A process that left the tree (`setsid` double fork, M2) | full owner rights | still full owner rights: it is indistinguishable from the owner's own shell |
| A process reading another's environment (M1) | possible | possible |
| A sandboxed tool call (Codex Linux sandbox) | AF_UNIX sockets allowed, so the owner socket is reachable (`U:codex/codex-rs/linux-sandbox/src/seccomp.rs:223-233`) | same |

Only a session that cannot reach the owner socket at all (a separate UID, a mount namespace without the
socket, a container or VM, #88) makes rights a boundary. Until then the honest claim is: rights stop
mistakes and casual misuse by an agent on the sanctioned paths; a hostile or deliberately injected agent
can escape them. The same limit already applies to the Hermes policy (`docs/architecture.md:1573-1575`).

### 9.5 Transport and remote hosts

- **Per-session stdio server** (recommended): no listener, no port, no token, attestation by the worker;
  it dies with the session. Cost: one small process per session.
- **One local Streamable HTTP server**: one process, but every request needs a bearer per session, which
  must live somewhere the agent can read (M1), and the HTTP transport needs the OAuth metadata the spec
  requires (https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization.md). Rejected for
  the local case.
- **Remote hosts.** The per-session server calls its local worker; a `remote` capability lets it address
  another host's daemon over the overlay like the CLI does (`--host`), with the target daemon applying its
  own policy; no new listener and no relay hop (`docs/architecture.md:2209-2215`).
- **Coexistence with the CLI.** The CLI stays the human interface and the fallback. Both call the same
  daemon methods; AC-20's caller classification gives a CLI run inside a session the same rights as its MCP
  server, so the two paths cannot disagree.

### 9.6 Security

| Threat | Control |
|---|---|
| Confused deputy: a worker makes its manager act for it | Rights are checked against the caller's own session, never an argument; `report_to_parent` delivers bounded text that the manager's skill treats as data (`docs/architecture.md:2257-2258`) |
| A prompt-injected agent uses its rights | Least privilege by default, narrowing on spawn, `confirm` asks for destructive tools (`session_remove`, cross-host spawn) answered by the owner only |
| Spawn storms | `children.spawn` carries a count ceiling, a rate, and the profile/project allowlist |
| Reading other sessions' screens (data exfiltration between projects) | `sessions.read` is off by default; screen and output tools return bounded text |
| Injection through tool results | Screen and output text is returned as data in structured fields; no tool result contains instructions from Pohunek itself |
| Rate limits | Per session and per capability, configured in `service.toml` |
| Same-UID bypass | Section 9.4; documented as a guardrail (owner answer Q10); isolation (#88) is future scope |

### 9.7 Core versus plugin (Part B)

Everything that enforces rights is core: the capability set, the grant rules, the parent link, caller
classification, the worker request kinds and the tool table of `pohunek mcp`. Plugins only extend
surfaces (a companion "rights" view) or add runtime handlers that register the server.

### 9.8 MCP or A2A

| | MCP (2026-07-28) | A2A (1.0.0; GitHub latest v1.0.1) |
|---|---|---|
| Purpose | An agent calls a server's tools and reads its resources | An agent delegates a task to another opaque agent |
| Transports | stdio, Streamable HTTP | JSON-RPC, gRPC, HTTP+JSON |
| Caller identity | stdio: the spawning process; HTTP: OAuth 2.1 bearer with resource binding | Per-request security schemes on the Agent Card (API key, OAuth, OIDC, mTLS) |
| Long-running work | Tasks extension, opt-in | Core task lifecycle with `INPUT_REQUIRED`, `AUTH_REQUIRED`, push notifications |
| Claude Code | client (stdio, HTTP) | no official support found (UNVERIFIED absence) |
| Codex | client (`[mcp_servers]`) | not found |
| Cline | client | not found |
| Pi | client (`U:pi/packages/coding-agent/docs/mcp.md:1-3`) | not found |
| Muse | `mcp_servers` setting (`MUSE:64`) | not researched |
| Gemini CLI (reference) | client | experimental `packages/a2a-server` |

Sources: https://a2a-protocol.org/latest/specification/ ; https://github.com/a2aproject/A2A/releases ;
https://code.claude.com/docs/en/mcp ; https://learn.chatgpt.com/docs/extend/mcp?surface=cli ;
https://docs.cline.bot/mcp/adding-and-configuring-servers ;
https://github.com/google-gemini/gemini-cli/tree/main/packages/a2a-server .

**Recommendation.** MCP for agent-to-Pohunek control: every runtime speaks it today, stdio gives the kernel
peer for attestation, and tools match what agents already do. MCP also for session-to-session delegation:
both ends are Pohunek sessions on one daemon, so a delegated task is a child session plus asks and reports,
whose states can borrow A2A's names (`working`, `input_required`, `auth_required`, terminal states) without
the protocol. A2A adds nothing between two local sessions and no runtime can consume it. An A2A gateway that
exposes a Pohunek session to an external agent (or calls one) is a later adapter outside core.

---

## 10. Slices in dependency order

Owner answer Q10: questions ship first, permissions (Part B rights) after; VMs (#88) are future scope and
not part of this work. The ids below are the sub-issues of epic #1987 (design issue #1982).

**Milestones.** AC-01 ships in 0.40, AC-02 to AC-07 are planned for 0.42 and not started, the secret and further-runtime slices in 0.43,
and AC-15 with the rights phase (AC-17 to AC-20) in 0.44. AC-08 lives in `pohunekd/pohunek-companion` and AC-09 in `zajca/pohunek-work`; their milestones are
recorded on their issues. AC-16 is unscheduled until #1561 lands.

**Phase 1: questions.**

| Id | Content | Depends on |
|---|---|---|
| AC-01 | Land the RFC (`docs/design/ask-channel-rfc.md`) and amend the task RFC (13.4 shared `pohunek mcp`, 8.5 routed asks as addressed answers) | none |
| AC-02 | Ask protocol, store, limits, importance and expiry, attention records, origin guard for answer methods, owner-side `pohunek ask`, worker-attested `ask.create` and `pohunek ask create` | AC-01 |
| AC-03 | Asker wait, cancel, generation end, limits | AC-02 |
| AC-04 | `pohunek mcp` with the ask tools (the command #231 extends) | AC-03 |
| AC-05 | Claude pilot: per-launch registration of `pohunek mcp`, agent skill | AC-04 |
| AC-06 | Claude routed prompts: `AskUserQuestion` and `PermissionRequest` | AC-05; coordinates #228 |
| AC-07 | Pending routed ask is blocked evidence and refuses typed input | AC-06 |
| AC-08 | Companion ask card (pohunekd/pohunek-companion) | AC-02 |
| AC-09 | Web ask card | AC-02 |
| AC-10 | `secret` kind: backend seam, ordered backends, references | AC-03 |
| AC-11 | In-place `pohunek secret exec` and referenced `secret_env` slots | AC-10 |
| AC-12 | Secret entry from owner devices over the overlay (6.5) | AC-10, AC-08 |
| AC-13 | Codex: registration and `PermissionRequest` routing | AC-06 |
| AC-14 | Pi: extension routing and registration | AC-04 |
| AC-15 | Core seam for plugin runtimes (Cline, Muse) to register `pohunek mcp` and route approvals | AC-06 |
| AC-16 | Protocol-backend approvals and questions become routed asks | AC-06, #1561 |

**Phase 2: permissions.**

| Id | Content | Depends on |
|---|---|---|
| AC-17 | Rights model: capabilities, profile ceiling, defaults, enforcement on the worker path, audit | AC-03 |
| AC-18 | Parent link and session tools in `pohunek mcp` with narrowing on spawn | AC-17, AC-04 |
| AC-19 | Manager answers granted in agent settings | AC-18 |
| AC-20 | Caller classification on the public socket by kernel peer | AC-17 |

---

## 11. Assumptions

1. The operator is one person on their own machines; team relay is out of scope here
   (`docs/architecture.md:2204-2207`).
2. Ask text is untrusted data and may be in any language; the channel never translates.
3. The value of a secret is never needed by the model itself; when it is, the design is the wrong tool.
4. Core keeps no default secret backend on a host without a keychain; such a host needs an explicit
   backend choice (no silent fallback).

---

## 12. Owner decisions (round ask-v1, accepted 2026-11-06)

The twelve questions of round ask-v1 are answered and the answers are accepted as the decisions of this
RFC; section 0.1 lists where each answer changed the design. Later decisions amend this RFC.

| # | Question | Owner's answer |
|---|---|---|
| Q1 | How does an agent ask? | Pohunek MCP tool plus `pohunek ask`; native permissions answered in 4.7 |
| Q2 | Which runtime goes first? | Claude Code |
| Q3 | What happens when nobody answers? | Configured expiry, the agent is told; safe defaults for simple choices; plus `important` asks without expiry |
| Q4 | May an operator refine a question? | Yes, as a final outcome with a note |
| Q5 | May a manager answer its workers' questions? | Only choice/text, only when granted, and the grant lives in agent settings |
| Q6 | Where are secrets stored? | plugin-vault if installed, then the system keychain, then a password manager |
| Q7 | How does a secret reach a command? | In-place `pohunek secret exec` with masked output, plus next-launch `secret_env` |
| Q8 | May a secret be entered remotely? | Yes, from any owner device over the overlay (conditions in 6.5) |
| Q9 | Should a pending question block typed input? | Yes, for Pohunek-handled questions |
| Q10 | Rights as a guardrail until isolation? | Yes; questions first, permissions after; VMs future scope |
| Q11 | Default rights | Read itself, ask the operator, report to its manager |
| Q12 | MCP or A2A? | MCP now, A2A gateway for outside agents later |

---

## 13. Measurements

All run with `env -i` (no `POHUNEK_*`), synthetic values, no model, no
vault or keychain touched. Linux 7.2.11-arch1-1, `kernel.yama.ptrace_scope = 1`.

| Id | Question | Result |
|---|---|---|
| M1 | Can a same-UID process that is not an ancestor read another process's environment? | Yes: a reader in its own POSIX session found `SYNTH_TOKEN=synthetic` in the target's `/proc/<pid>/environ` (count 1, exit 0) |
| M2 | Does a `setsid` double fork stay a descendant of the launching root? | No: the child was reparented to `systemd` (pid 2698), `descendant_of_root=no` |
| Tools | Which vault tools exist on the owner's machine (`command -v` only) | `op`, `secret-tool`, `kwallet-query`, `systemd-creds`, `sops` present; `bw`, `bws`, `pass`, `gopass`, `age`, `agent-keychain` absent |

Not measured (needs a real agent or model): Claude's answering `PreToolUse` in an interactive session,
Codex progress-reset behaviour, Muse MCP child environment, Cline hook answering.

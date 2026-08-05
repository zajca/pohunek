# Implementation Plan: First-Class Hermes Agent Integration

Status: proposed

Planning baseline: `main` at
`15ebebb20a6ad6302f95b324e3136b28c17e8feb` on 2026-07-27.

Normative design:
[`hermes-agent-integration.md`](./hermes-agent-integration.md).

## 1. Purpose

This plan turns the Hermes RFC into three independently releasable production
milestones. Workstreams remain dependency-ordered and can be reviewed in
coherent commits, but arbitrary partial workstreams are not release boundaries.
Each milestone is complete only when its Definition of Done subset in section
18 passes; the full RFC is complete when all three subsets pass.

The plan is intentionally explicit about protocol ripples, worker
compatibility, plugin policy, real-Hermes validation, UI/web parity, knowledge
materialization, and release sequencing. Adding only `AgentKind::Hermes` or a
plugin scaffold does not satisfy it.

## 2. Fixed implementation decisions

The implementation must preserve these decisions from the RFC:

1. Hermes is both a managed Pohunek runtime and a Pohunek operator.
2. The managed runtime launches explicitly as `hermes chat`.
3. Native resume uses the exact recorded reference with
   `hermes chat --resume <reference>`.
4. Hermes fork is unsupported until Hermes exposes documented native
   semantics.
5. Resume and fork capabilities are modeled independently.
6. Terminal screen, incremental output, and bounded wait are provider-neutral
   public APIs.
7. The Hermes plugin invokes the Rust CLI JSON surface; it does not implement
   Pohunek transport or NetBird.
8. Prompt/input text crosses the CLI process boundary through standard input.
9. Raw attach is not exposed as a Hermes tool.
10. Plugin access mode and host allowlist are explicit required installation
    inputs, and they are the only operator-configurable policy fields besides
    the CLI path/range, limits, and schema version. Permitted agents come from
    runtime inventory and the metadata surface is a fixed compiled schema;
    neither is expressible in the policy.
11. Plugin installation is local and per Hermes profile/home.
12. Plugin assets are embedded in the Pohunek CLI release and are installed
    atomically without a network download.
13. Lifecycle hooks are local, bounded, best effort, and never fail a Hermes
    turn.
14. Hermes `state.db` is never read or modified.
15. Notification policy becomes provider-keyed rather than gaining another
    fixed field.
16. Public and private protocol versions are bumped deliberately; no pre-1.0
    backward shape shim is added.
17. Deterministic tests are supplemented by a pinned real-Hermes compatibility
    job that requires no model provider. Turn-dependent terminal fixtures are
    recorded goldens refreshed by a documented operator command; no fake
    provider server is built.
18. Only the local Hermes terminal backend is supported.
19. Native identity prefers the ordered worker-private claim path and falls
    back locally to the public `session.report_native_id` method, which is a
    necessary fallback and which M1 extends with the same ordering fields so
    the contract is uniform across providers. A valid continuation-session
    claim supersedes the immutable launch identity.
20. Bounded waits occupy dedicated public control connections, and the
    required timeout is their only guaranteed termination bound. The maximum
    wait is deliberately short so a waiter abandoned by a killed client frees
    its slot within seconds.
21. Plugin policy is a delegated-tool guardrail, not a same-user sandbox. The
    daemon authoritatively rejects exactly `session.stop`, `session.resume`,
    `session.remove`, `session.fork`, `session.resize`,
    `session.set_metadata`, `session.rename`, and `session.input` when they
    target the caller's origin session. It deliberately allows
    `session.report_agent`, `session.release_agent`, and
    `session.report_native_id`; the last is the necessary local fallback when
    the owner-private worker claim cannot be delivered.
22. Pohunek-owned policy lives outside the plugin directory and outside the
    immutable asset checksum set.
23. M1 performs the single public protocol break, carrying range negotiation,
    the observation APIs, the provider-keyed notification policy, and
    forward-compatible `AgentKind` deserialization. M2 and M3 are purely
    additive and perform no public bump.
24. An unknown `AgentKind` wire value deserializes into a neutral,
    presentation-only variant that is never launchable, is rejected by every
    mutating path, is never persisted, and is represented identically in Rust
    and TypeScript.

Changing one of these decisions requires updating the RFC first.

## 3. Delivery shape

### 3.1 Milestones and release boundaries

Implementation follows the repository milestone workflow:

1. write a transient `NEXT.md` from this plan for exactly one of these
   independently valuable milestones:
   - **M1 — provider-neutral foundations:** decouple resume from fork; add
     `session.screen`, `session.output`, and `session.wait`; add private
     control-plane observation; complete CLI JSON parity and stdin input; add
     the exact eight-method daemon origin-session guard with lifecycle-report
     exceptions; harden the public identity report; and
     perform the single public protocol break — range negotiation, the
     provider-keyed notification policy, and forward-compatible `AgentKind`
     deserialization — together with the private worker bump;
   - **M2 — first-class Hermes runtime:** add `AgentKind::Hermes` additively,
     the compiled local-backend adapter, detection manifest, and
     GUI/web/assistant ripples, with no public protocol bump;
   - **M3 — Hermes operator plugin:** add the installer, real plugin, hooks,
     typed tools, Pohunek-owned policy, and generated skill;
2. create a fresh `zajca/<ticket-or-topic>/hermes-integration` worktree from the
   then-current `main`;
3. implement the selected milestone's workstreams below in dependency order;
4. review the entire branch against `NEXT.md` and this RFC;
5. run every gate relevant to that milestone, including the full repository
   gate sets where its changes participate;
6. merge only when the selected milestone's section 18 subset is complete;
7. remove the transient branch/worktree and replace `NEXT.md` after landing.

M1 is valuable even if Hermes never ships and may release independently. M2 may
release on top of M1 as first-class managed-runtime support without the
operator plugin. M3 completes the RFC. If a milestone is split into stacked
review branches, only the completed milestone boundary is releasable.

### 3.2 Expected commit boundaries

The following are review boundaries inside the three milestone boundaries, not
authorization to release incomplete work:

1. protocol contracts and generated TypeScript;
2. private worker observation and compatibility;
3. daemon session APIs and Hermes runtime adapter;
4. Rust SDK and JSON CLI parity;
5. Hermes installer, plugin hooks, and policy;
6. Hermes tools and generated skill;
7. GUI/web/notification parity;
8. end-to-end tests, docs, packaging, and release notes.

Protocol and generated-code changes should remain together. Hand-authored
knowledge sources and generated plugin skill assets should remain together.

### 3.3 Sizing and review budget

The baseline contains 36 Rust files referencing `AgentKind`, with roughly 196
enum-variant occurrences: 91 Codex, 61 Claude, and 44 Shell. The Rust workspace
is about 119,000 lines. This plan spans 12 workstreams and roughly 50 named test
scenarios, plus a new Python plugin, a skill-generator change, and GUI/web
ripples.

That breadth is why the release boundary is split into M1, M2, and M3 instead
of one all-or-nothing branch. Sizing is a review and sequencing input, not
permission to omit a ripple or weaken a milestone's tests.

## 4. Workstream dependency graph

```text
Upstream compatibility lock
          |
          +--------------------------+
          |                          |
          v                          v
Public protocol contract     Hermes PTY/hook fixtures
          |                          |
          +------------+-------------+
                       |
             Private worker support
                       |
                 Daemon APIs
                       |
              Rust SDK + JSON CLI
                    /      \
                   v        v
        Hermes runtime     CLI-side installer
              |                 |
              +--------+--------+
                       |
             Hermes hooks + tools
                       |
               Generated skill
                       |
             GUI/web/docs/release
                       |
          Full deterministic + real E2E
```

The runtime adapter can be coded while worker observation is in progress, but
plugin tool completion depends on the final public methods and JSON CLI.

## 5. Workstream 0: lock the Hermes compatibility baseline

### 5.1 Goal

Replace assumptions about Hermes CLI, plugin registration, hook ordering, and
PTY behavior with versioned evidence before adapter constants or plugin code
are frozen.

### 5.2 Tasks

1. Select the current supported Hermes release at implementation time.
2. Record its version and source checksum in the test/CI dependency lock used
   by the real-Hermes suite.
3. Create an isolated test home and Python environment; do not use or inspect
   the developer's real `HERMES_HOME`.
4. Capture and review bounded outputs for:
   - `hermes --version`;
   - `hermes --help`;
   - `hermes chat --help`;
   - profile commands and argument ordering;
   - plugin list/enable/disable behavior;
   - plugin discovery paths;
   - tool and bundled-skill registration;
   - every hook callback signature used by the integration.
5. Write a black-box PTY fixture driver that starts the real Hermes executable
   under a pseudo-terminal with a controlled provider/test mode.
6. Capture redacted fixtures for:
   - startup and prompt ready;
   - short input;
   - multiline input;
   - working/tool-running state;
   - approval blocked state;
   - successful turn completion;
   - interrupted turn;
   - graceful exit;
   - resumed native session;
   - alternate-screen and classic modes, if both exist.
7. Measure hook call ordering and verify whether hooks remain synchronous.
8. Confirm that `on_session_start` is absent on resume and `pre_llm_call`
   reasserts the native session ID.
9. Confirm the maximum safe hook deadline that is materially below visible
   turn latency; encode a named default and a lower test override.
10. Store only sanitized fixtures. Run the repository secret scanner over
    them.
11. Trigger context compaction and confirm that Hermes creates a continuation
    session with a new native ID during the same process/runtime, and that the
    following `pre_llm_call` exposes that new ID.
12. Record which terminal backend the pinned release selects by default and
    verify that the integration rejects or diagnoses every non-local backend as
    unsupported.

### 5.3 Implementation locations

Expected additions or changes:

- a Hermes compatibility fixture directory under the existing test fixture
  conventions;
- a focused test helper under `crates/session-worker/tests/`,
  `crates/daemon/tests/`, or `crates/cli/tests/`, depending on ownership;
- CI setup for an isolated pinned Hermes environment;
- no production dependency on the fixture driver.

Do not add real tokens or rely on a developer's configured model provider. Do
not build a fake provider server either: the CI suite is deliberately
model-free and runs only what needs no model turn — version and CLI shape,
`hermes plugins list`/`enable`/`disable`, successful `register_tool`,
`register_skill`, and `register_hook` registration with the expected tool and
skill names present, target resolution, and `integration install`/`status`/
`doctor`/`uninstall` against an isolated profile.

Turn-dependent terminal fixtures — prompt-ready, working, approval-blocked,
completion, interruption, resumed session, and alternate-screen — are recorded
goldens captured from a real Hermes run and refreshed by a documented operator
command, not reproduced in CI. This keeps the gate stable in a repository that
already has flaky PTY and socket tests under load, while still catching the
failure the gate exists for: an upstream plugin or CLI API change, which is
detectable without a model.

### 5.4 Completion checks

- The exact supported Hermes version and install source are reproducible.
- PTY input framing is proven by a real Hermes process.
- Every used hook signature and ordering is captured.
- Continuation-session lineage and `pre_llm_call` reassertion are captured.
- The pinned default terminal backend is recorded and local-backend-only
  support is enforced.
- Plugin and skill registration succeeds in an isolated profile.
- No fixture contains prompt secrets, provider tokens, personal paths, or
  uncontrolled conversation data.

## 6. Workstream 1: public protocol contract

### 6.1 Goal

Define all provider and model-friendly observation behavior once in the Rust
wire contract, then regenerate TypeScript before implementing handlers.

### 6.2 Rust protocol changes

Update at least:

- `crates/protocol/src/session.rs`;
- `crates/protocol/src/method.rs`;
- `crates/protocol/src/error.rs`;
- `crates/protocol/src/notification.rs`;
- `crates/protocol/src/capabilities.rs`;
- `crates/protocol/src/limits.rs`;
- `crates/protocol/src/version.rs`;
- `crates/protocol/src/lib.rs`;
- relevant protocol tests and fixtures.

Tasks:

1. In M1, make `AgentKind` forward compatible: an unknown wire value
   deserializes into a neutral, presentation-only variant. It must never be
   launchable, must be rejected by `session.new` and every other mutating path
   with a typed error, must never be persisted, and must be represented
   identically in Rust and TypeScript. In M2, add `AgentKind::Hermes` with wire
   value `"hermes"` as a purely additive change.
2. Add request/result types for:
   - `session.screen`;
   - `session.output`;
   - `session.wait`.
3. Add runtime identity, terminal watermark, cursor, gap, and wait-reason types.
4. Require runtime identity where an output cursor could otherwise cross a
   runtime generation.
5. Add typed capability indicators for terminal read, output read, and bounded
   wait if capabilities are advertised publicly.
6. Add the error codes enumerated by the RFC.
7. In M1, replace fixed notification-provider fields with
   `providers: BTreeMap<String, NotificationKindPolicy>` or the project's
   canonical deterministic map type.
8. Define strict deserialization and unknown-key behavior consistently in Rust
   and TypeScript. `AgentKind` is the single deliberate exception, per task 1;
   strictness elsewhere is unchanged.
8a. In M1, extend `SessionReportNativeIdParams` with runtime identity, PID plus
    process-start identity, a monotonic sequence, and a bounded expiry, so the
    public identity report can carry the same ordering contract as the private
    active identity claim.
9. Derive the output byte and serialized screen-response ceilings from
   `MAX_CONTROL_LINE_BYTES`. Account for base64's four-thirds expansion and
   reserve envelope/JSON-escaping headroom, following the named,
   rationale-commented `MAX_SESSION_DIFF_BYTES` precedent.
10. In M1, perform the single public protocol bump and replace exact-only
    negotiation with explicit client/server minimum and maximum supported
    versions. Update overlap, no-overlap, and legacy exact-envelope rejection
    fixtures; do not describe a nonexistent public current/previous window. M2
    and M3 perform no public bump, because every breaking shape lands in M1.
11. Update every exhaustive method/enum mapping and compile-time export.

### 6.3 Wire-shape tests

Add:

- JSON round-trip for `AgentKind::Hermes`;
- exact golden JSON for screen/output/wait requests and results;
- output gap and runtime-change cases;
- maximum valid and just-over-limit values, including output base64 and
  serialized-screen control-line boundaries;
- zero/empty/invalid wait predicates;
- every wait reason;
- provider-keyed notification policy;
- unknown provider and unknown field behavior;
- overlapping/non-overlapping range negotiation and legacy exact-envelope
  rejection;
- error-envelope field redaction.

No handler work starts until these shapes are reviewed, because Python tools,
Rust SDK, generated TypeScript, and docs depend on them.

### 6.4 TypeScript generation

Run:

```text
cargo xtask ts generate
cargo xtask ts check
```

Review changes under `web/shared/src/generated/`, especially:

- `AgentKind.ts`;
- session method param/result types;
- terminal/output/wait types;
- errors/constants/method maps;
- `NotificationPolicy.ts`;
- barrel exports.

Do not hand-edit generated TypeScript.

### 6.5 Completion checks

- Rust and TypeScript encode the exact same shapes.
- Old clients reject the new version predictably rather than partially
  accepting Hermes data.
- Notification policy has no Codex/Claude-specific top-level fields.
- Wire payload limits are documented and covered at boundaries.

## 7. Workstream 2: private worker observation

### 7.1 Goal

Expose the terminal model and retained output through bounded,
runtime-identified private operations without weakening attach semantics.

### 7.2 Required guideline review

Before editing Rust, read the repository's vendored Rust guideline index and
at least:

- `.agents/rust-guidelines/11_universal_guidelines.md`;
- the application/error-handling guidance for the worker binary;
- library/correctness guidance applicable to protocol and terminal data.

Keep or update guideline-compliance markers accurately in every edited Rust
file.

### 7.3 Worker protocol changes

Update at least:

- `crates/worker-protocol/src/control.rs`;
- `crates/worker-protocol/src/data.rs`;
- `crates/worker-protocol/src/codec.rs`;
- `crates/worker-protocol/src/version.rs`;
- `crates/worker-protocol/src/lib.rs`;
- protocol tests.

Tasks:

1. Add a negotiated `Capability::ControlPlaneObservation` for one-shot
   snapshot/output reads. Keep the existing `Capability::TerminalSnapshot` and
   `Capability::AtomicReplay` names and data-stream attach semantics unchanged.
2. Add a typed terminal-snapshot request/response carrying runtime identity.
3. Add bounded output replay parameters or an equivalent one-shot output-read
   operation.
4. Make replay framing chunk every payload below the existing maximum data
   frame size.
5. Include retained-history start and runtime-end offsets.
6. Detect and report a requested cursor older than retained history.
7. Reject a cursor/runtime mismatch.
8. Bump the private protocol version.
9. Preserve the current-and-previous version compatibility window.
10. Ensure private error messages do not embed PTY data.

### 7.4 Worker implementation changes

Update at least:

- `crates/session-worker/src/server.rs`;
- `crates/session-worker/src/output.rs`;
- `crates/session-worker/src/config.rs`;
- `crates/session-worker/src/error.rs`;
- focused tests.

Tasks:

1. Read a `TerminalSnapshot` from the existing terminal tracker without taking
   attach ownership.
2. Bind the snapshot to worker/runtime identity.
3. Enforce configured dimension/serialization bounds.
4. Read retained output by byte cursor.
5. Return the newest bounded tail when no cursor is provided.
6. Long-wait only when the cursor is at the current end and the runtime is
   live.
7. Wake readers on output, runtime exit, shutdown, or cancellation.
8. Chunk replay that exceeds a single frame.
9. Keep the existing total output-history retention cap unchanged unless tests
   prove a separate configurable change is required.
10. Avoid holding the PTY write/input lock while serializing output.

### 7.5 Worker tests

Cover:

- main-screen snapshot;
- alternate-screen snapshot;
- cursor/title/progress fields;
- Unicode, invalid UTF-8 PTY bytes, and wide characters;
- maximum dimensions;
- no-output and exited-runtime reads;
- omitted cursor tail;
- exact cursor;
- old cursor with gap;
- `max_bytes` pagination;
- output larger than one private frame;
- output at the full retained-history cap;
- waiter wake on output;
- waiter timeout;
- waiter cancellation;
- a waiter abandoned by a killed client frees its slot within the configured
  maximum wait;
- runtime identity mismatch;
- current daemon/current worker;
- current daemon/previous worker feature unavailable;
- previous daemon/current worker compatibility;
- daemon disconnect without worker/PTY loss.

### 7.6 Completion checks

- A worker can serve a screen and output concurrently with a human attach.
- Observation never changes terminal dimensions or attach ownership.
- A replay larger than one frame is correctly chunked.
- Older workers continue running and return a typed unsupported capability.

## 8. Workstream 3: daemon session APIs

### 8.1 Goal

Route observation safely, implement race-free bounded wait, and preserve the
logical-session/runtime model across all states.

### 8.2 Configuration

Add named, documented settings in the daemon's established config path:

- maximum bytes per output response;
- maximum output wait;
- maximum session wait;
- maximum serialized terminal dimensions and response size;
- maximum global waiters;
- maximum waiters per session.

Use typed validation and fail fast on invalid configuration. Platform defaults
must be named constants with rationale comments, not repeated literals. Default
the two wait ceilings short, in the 5-10 second range; their rationale comment
records that a client killed mid-wait holds its slot until the timeout expires
because the sequential dispatch loop cannot observe the disconnect, so a low
ceiling is what bounds the availability dip. Derive
the output byte and serialized screen-response limits from
`MAX_CONTROL_LINE_BYTES`; account for base64 expansion and response-envelope
headroom exactly as required by RFC section 10.4.

### 8.3 Handler and registry changes

Update at least:

- `crates/daemon/src/api/handler/session.rs`;
- `crates/daemon/src/api/handler/mod.rs`;
- `crates/daemon/src/session/mod.rs`;
- `crates/daemon/src/session/reconcile.rs`;
- `crates/daemon/src/runtime/client.rs`;
- `crates/daemon/src/capabilities.rs`;
- `crates/daemon/src/error.rs`;
- session/event tests.

Tasks:

1. Dispatch `session.screen`, `session.output`, and `session.wait`.
2. Resolve the logical session and current managed worker.
3. Reject observed external sessions with the correct typed terminal error.
4. Negotiate required worker capability before forwarding.
5. Verify returned worker/runtime identity against the registry.
6. Map private output gaps and feature errors to stable public errors.
7. Add a per-session notification primitive that wakes on:
   - public session metadata/state changes;
   - activity changes;
   - runtime replacement;
   - terminal watermark advances;
   - output offset advances;
   - session removal;
   - daemon shutdown.
8. Implement snapshot/register/recheck ordering to close the lost-wakeup race.
9. Enforce waiter limits without holding a global write lock while waiting.
10. Treat required `timeout_ms` as the only guaranteed waiter termination
    bound. Document and implement SDK/CLI use of a dedicated control connection
    for `session.wait` and `session.output` with `wait_ms`; do not claim that
    client disconnect is observed while sequential dispatch awaits a handler.
11. Return a normal redacted `SessionInfo` in wait results.
12. Add structured payload-free logs and duration/count metrics.
12a. Enforce the extended `session.report_native_id` ordering contract with the
     same rejection rules already applied to a private active identity claim:
     stale runtime generation, mismatched PID start identity, lower or equal
     sequence after a newer report, provider mismatch, expired report, and a
     report targeting another logical session. Define and test the behavior for
     a report that omits the new fields; do not silently accept it as
     last-write-wins.
12b. Update `crates/daemon/src/integration/assets/codex/pohunek-agent-state.sh`
     and `.../claude/pohunek-agent-state.sh` to send the new ordering fields on
     the public path, and cover both providers with tests. The hardened shape is
     uniform, not Hermes-specific.
13. Enforce the origin-session guard in the daemon from `POHUNEK_SESSION_ID`
    plus `POHUNEK_DAEMON_ID`, using the existing self-feeding attach guard as
    the identity reference. Its complete denied set is `session.stop`,
    `session.resume`, `session.remove`, `session.fork`, `session.resize`,
    `session.set_metadata`, `session.rename`, and `session.input`. Explicitly
    allow `session.report_agent`, `session.release_agent`, and
    `session.report_native_id` for the origin session; the public native-id
    method is the necessary local fallback for an unavailable owner-private
    worker claim. Keep the plugin check as defence in depth and do not describe
    this as a broader mutation policy.
14. In M3, when a valid higher-sequence active identity claim reports a Hermes
    continuation session, persist it as the resume reference and prefer it to
    the immutable launch identity.

### 8.4 Daemon tests

Cover:

- screen/output success through a real worker subprocess;
- no worker, stale worker, and observed external session;
- previous worker capability unavailable;
- runtime swap between request and response;
- daemon restart reconciliation followed by screen/output;
- wait condition already true;
- each wake reason;
- registration race;
- timeout, daemon-shutdown cancellation, and dedicated-connection occupancy;
- global/per-session waiter exhaustion, including the corresponding waiting
  connection cap;
- session removal during wait;
- output gap at retention boundary;
- no PTY payload in logs/errors;
- all request bounds, including exact control-line boundary payloads;
- each of the eight guarded origin-session methods is denied through direct
  CLI/API bypass as well as the plugin, while `session.report_agent`,
  `session.release_agent`, and the necessary public fallback
  `session.report_native_id` remain allowed;
- continuation identity supersedes launch identity, while stale, expired, and
  lower-sequence claims cannot roll it back.

### 8.5 Completion checks

- Public observation is race-safe and bounded.
- Wait does not busy-poll.
- Bounded waits use dedicated connections and terminate by result, required
  timeout, or daemon shutdown without promising disconnect cancellation.
- Daemon restart preserves observation of a surviving worker.
- Sensitive terminal content appears only in the explicit response.

## 9. Workstream 4: Hermes runtime adapter

### 9.1 Goal

Make Hermes a first-class compiled agent with honest capabilities and
profile/detection/inventory parity.

### 9.2 Decouple resume and fork

Update at least:

- `crates/daemon/src/agent/mod.rs`;
- `crates/daemon/src/agent/profile.rs`;
- `crates/daemon/src/session/resume.rs`;
- agent/profile tests.

Tasks:

1. Replace the current coupling between flag resume and Claude fork.
2. Define independent compiled resume and fork capability types.
3. Preserve current Codex and Claude behavior exactly.
4. Make shell resume/fork support explicit rather than implicit.
5. Allow profiles to disable inherited fork support.
6. Reject attempts to enable unsupported fork semantics in configuration.
7. Return `agent_fork_unsupported` for Hermes before creating a worktree,
   worker, or logical child.
8. Ensure capability presentation in CLI/GUI/web is data-driven.

### 9.3 Hermes adapter

Add:

- `crates/daemon/src/agent/hermes.rs`;
- `crates/daemon/src/detect/manifests/hermes.toml`;
- focused adapter/detection fixtures.

Tasks:

1. Launch `hermes chat`.
2. Resume with the exact recorded reference and no ambient `--continue`.
3. Encode the PTY input framing proven in workstream 0.
4. Add executable/Python-entrypoint process matchers.
5. Add bounded screen fallback signatures.
6. Identify working, blocked, idle, and fatal-startup evidence.
7. Set hook/native identity evidence above screen fallback priority.
8. Expire stale hook evidence according to the existing evidence model.
9. Add Hermes version/runtime probe to inventory.
10. Permit `base = "hermes"` profiles with validation.

### 9.4 Exhaustive provider ripples

Search and review every exhaustive use:

```text
rg 'AgentKind|Codex|Claude|codex|claude' \
  crates web docs scripts
```

At minimum review:

- daemon runtime/inventory/detection/session code;
- client presentation helpers;
- CLI parsers and labels;
- GUI core state, filters, and commands;
- Iced GUI views;
- web generated types, reducers, filters, routes, components, and fixtures;
- notification policy and projector;
- assistant runtime selection;
- setup/doctor output;
- scripts that offer agent choices;
- docs and knowledge sources.

Do not mechanically add Hermes to a branch whose semantics should instead be
capability-driven.

### 9.5 Runtime tests

Cover:

- exact launch argv;
- exact resume argv with spaces/special characters kept as one argument;
- absent native reference;
- Hermes fork unsupported and side-effect free;
- named Hermes profile argv;
- executable override validation;
- multiline input;
- approval screen input rejection/behavior;
- process match true/false positives;
- screen fallback fixtures;
- hook evidence precedence/expiry;
- inventory installed/missing/unsupported version;
- daemon restart with the same live Hermes worker;
- native resume under the same logical session with a new runtime generation.

### 9.6 Completion checks

- Hermes has the same durable worker ownership as other managed agents.
- Resume never uses an inferred "last" Hermes session.
- Fork failure creates no child session or worktree.
- Existing Codex/Claude behavior and tests remain green.

## 10. Workstream 5: Rust SDK and JSON CLI parity

### 10.1 Goal

Create the stable process API consumed by the plugin and complete missing
human/automation command parity.

### 10.2 Client SDK

Update at least:

- `crates/client/src/lib.rs`;
- `crates/client/src/error.rs`;
- transport/cancellation tests.

Add typed methods for:

- session screen;
- session output;
- session wait;
- any existing public methods not currently exposed consistently to the CLI,
  including resume, resize, metadata, and runtime inventory.

Methods accept typed host/session/runtime/cursor values, propagate
cancellation locally, and preserve typed protocol errors. A bounded
`session.wait` or waiting `session.output` opens a dedicated transport
connection so it cannot stall a shared SDK/GUI connection; closing it does not
promise prompt daemon-side waiter cancellation before the required timeout.

### 10.3 CLI parser and commands

Update at least:

- `crates/cli/src/lib.rs`;
- `crates/cli/src/commands/session.rs`;
- `crates/cli/src/commands/host.rs`;
- `crates/cli/src/client.rs`;
- `crates/cli/src/error.rs`;
- command/parser/integration tests.

Add or standardize:

- `pohunek session screen`;
- `pohunek session output`;
- `pohunek session wait`;
- `pohunek session resume`;
- `pohunek session resize`;
- `pohunek session metadata`;
- runtime inventory command parity;
- `--stdin`/`--input-stdin` for new and input;
- `--json` on every plugin-used command.

Tasks:

1. Define one standard JSON success/error envelope.
2. Include CLI and public protocol versions in the envelope.
3. Emit one JSON document to stdout and diagnostics only to stderr.
4. Give parse/usage errors stable JSON codes.
5. Reject mixed positional/option/stdin input.
6. Bound stdin reads using configured/protocol limits.
7. Do not echo stdin payloads in diagnostics.
8. Preserve binary output as base64 in JSON.
9. Provide an explicit human text projection outside JSON mode.
10. Resolve exact names deterministically and return candidates on ambiguity.
11. Propagate SIGINT/SIGTERM and process timeouts to client cancellation.
12. Ensure remote targets continue to use direct NetBird transport.

### 10.4 CLI contract tests

Use subprocess tests, not only command-function unit tests:

- exactly one stdout JSON value;
- clean stdout on every typed failure;
- stderr separation;
- exit code mapping;
- stdin multiline and control-character rejection;
- max stdin/output limits;
- local and fixture remote target;
- ambiguous session name;
- runtime changed and output gap;
- wait timeout and local CLI cancellation, with the daemon waiter remaining
  bounded by its required timeout;
- compatibility version fields;
- no prompt/output in logs.

### 10.5 Completion checks

- The plugin needs no human-output parser.
- Every plugin operation maps to one fixed CLI subcommand.
- Arbitrary text never appears in process arguments.
- Existing human CLI behavior remains coherent and documented.

## 11. Workstream 6: notification-policy generalization

### 11.1 Goal

Support Hermes notifications without repeating a provider-specific schema
change for the next agent.

### 11.2 Implementation

Update at least:

- `crates/protocol/src/notification.rs`;
- `crates/daemon/src/notifications/policy.rs`;
- `crates/daemon/src/notifications/projector.rs`;
- `crates/client/src/notifications.rs`;
- `crates/cli/src/commands/notifications.rs`;
- GUI/web policy state and views;
- generated TypeScript and fixtures.

Tasks:

1. Replace fixed provider fields with the provider-keyed map.
2. Preserve deterministic serialization order.
3. Define the base-policy fallback for a missing provider key.
4. Validate supported notification kinds independent of provider.
5. Add `hermes` to default policy materialization.
6. Render known providers from runtime inventory plus configured keys.
7. Update human and JSON CLI policy views.
8. Add upgrade notes; do not parse the old pre-1.0 shape.
9. Ensure hook-sourced attention uses sanitized fixed messages.

### 11.3 Tests

- default policy for Codex, Claude, and Hermes;
- missing provider fallback;
- unknown key behavior;
- deterministic JSON;
- CLI get/set;
- GUI/web reducer;
- hook attention projection and resolution;
- no notification payload leakage.

### 11.4 Completion checks

- No notification dispatch match is limited to Codex/Claude.
- Adding a future provider does not require another top-level schema field.

## 12. Workstream 7: CLI-side Hermes installer

### 12.1 Goal

Install, configure, diagnose, update, and remove the real plugin in one selected
Hermes profile without unsafe filesystem behavior or daemon-side shell-outs.

### 12.2 Module and asset layout

Create a focused CLI-internal module, for example:

```text
crates/cli/src/hermes_integration/
  mod.rs
  target.rs
  policy.rs
  assets.rs
  install.rs
  doctor.rs
  error.rs

crates/cli/src/hermes_integration/assets/pohunek/
  plugin.yaml
  __init__.py
  cli.py
  hooks.py
  policy.py
  redact.py
  tools.py
  skills/pohunek/SKILL.md   # generated
  .pohunek-ownership.json   # Pohunek-specific, not a Hermes manifest
```

`plugin.yaml` contains `name`, `version`, `description`, `provides_tools`, and
`provides_hooks`, plus only supported optional `requires_env`/`kind` fields.
Hermes calls `register(ctx)` from `__init__.py` exactly once. Registration uses
`ctx.register_tool(...)`, `ctx.register_hook(...)`, and
`ctx.register_skill("pohunek", ...)`; tool handlers accept `args: dict` and
`**kwargs` and return a JSON string. The generated skill is exposed as
`pohunek:pohunek`.

Hermes discovers the directory automatically below
`<HERMES_HOME>/plugins/`, with at most one category level, and the installer
enables it explicitly with `hermes plugins enable pohunek`. Assets are embedded
into the `pohunek` binary with an explicit Pohunek ownership manifest and
checksums; no runtime download is allowed.

### 12.3 CLI command changes

Update:

- `crates/cli/src/commands/integration.rs`;
- `crates/cli/src/lib.rs`;
- `crates/cli/src/error.rs`;
- integration command tests.

Support:

- install;
- status;
- doctor;
- update;
- uninstall;
- profile target or custom absolute home;
- access mode;
- repeated host allowlist;
- JSON/non-interactive output.

Hermes installation dispatches in the CLI. Existing Codex/Claude RPC behavior
continues unchanged unless a separate refactor is justified and fully tested.

For this delivery, `status`, `doctor`, `update`, and `uninstall` are explicitly
Hermes-only. Codex and Claude retain their existing install-only surface, and
the other verbs return a typed unsupported-action error for those agents. This
accepted pre-1.0 debt keeps the profile-owned Hermes lifecycle safe without
expanding unrelated provider installers in M3.

### 12.4 Target safety

Implement a typed target resolver:

1. Resolve the selected profile through supported Hermes CLI semantics.
2. Treat `HERMES_HOME` as input only when explicitly selected by the command,
   never as a hidden daemon default.
3. Require absolute custom paths.
4. Canonicalize existing ancestors.
5. Validate current-user ownership.
6. Reject group/world-writable unsafe ancestors.
7. Reject symlink escape.
8. Reject filesystem root, the user's home itself, repository/workspace root,
   and other broad destructive targets.
9. Create directories with owner-private permissions.
10. Redact absolute homes in JSON/error output where not needed for recovery.

Use the project's platform abstractions and typed errors. Do not use shell
commands for filesystem mutation.

### 12.5 Policy file

Hermes has no per-plugin configuration mechanism. Define a Pohunek-owned,
versioned serde model containing:

- schema version;
- absolute validated Pohunek CLI path;
- required CLI protocol range;
- access mode;
- allowed hosts;
- bounded tool timeout;
- bounded output/screen limits;
- bounded concurrent tool invocations.

Required security fields have no silent defaults. Named safe maxima are
compiled into the plugin and installer. The model carries no agent/profile list
and no metadata key list: permitted agents are whatever runtime inventory
returns, and metadata is a fixed compiled schema. Do not add a configurable
field for either.

Store the policy outside the Hermes plugin directory under the Pohunek state
directory, keyed by the canonical resolved Hermes home. Record its absolute
path in an installed plugin asset. The mutable policy is owner-private and is
explicitly excluded from the ownership marker's immutable asset checksums, so
`integration update --access-mode ...` does not make the plugin appear
modified. The installer never hand-edits general Hermes YAML.

### 12.6 Atomic install/update

Implement:

- sibling temporary staging directory;
- file mode setting;
- per-asset checksum validation;
- ownership marker;
- staged `plugin.yaml` schema and `__init__.py` import/syntax validation;
- atomic rename;
- managed previous-version backup only when needed for recovery;
- rollback if Hermes enablement fails;
- collision detection;
- modified-managed-file detection;
- idempotent reinstall;
- explicit update of policy and CLI path.

Never delete an unknown plugin directory. Never recursively remove a target
identified only by an unresolved variable or glob.

### 12.7 Hermes CLI invocation

Create a typed subprocess wrapper:

- fixed Hermes executable;
- fixed profile/plugin subcommands;
- argv array, no shell;
- minimal environment;
- bounded stdout/stderr and timeout;
- redaction;
- exact exit-status handling.

Use it to list/enable/disable the plugin and verify registration through the
pinned Hermes CLI surface.

### 12.8 Doctor checks

Doctor returns structured checks for:

- Hermes executable/version;
- resolved profile/home safety;
- plugin directory and ownership marker;
- checksums/modification;
- enabled status;
- policy schema and permissions;
- Pohunek CLI path/version compatibility;
- tool registration;
- bundled skill registration;
- managed-session hook dry run against a temporary local socket;
- host allowlist syntax;
- access mode;
- stale backup/staging directories.

Doctor does not connect to every allowed remote host unless the user asks for a
network check.

### 12.9 Uninstall

Implement marker-driven uninstall:

1. resolve and validate the same target;
2. disable through Hermes;
3. verify marker/checksums;
4. require confirmation for modified managed files;
5. remove only manifest-listed files;
6. remove empty managed directories and policy;
7. preserve all Hermes state and unrelated plugins/config;
8. report backup/recovery status.

### 12.10 Installer tests

Use temporary homes and a controlled Hermes executable:

- default/named/custom home resolution;
- relative path rejected;
- root/home/workspace broad target rejected;
- symlink escape;
- wrong owner/unsafe mode where the platform permits;
- fresh install;
- idempotent install;
- update, including policy-only update without an asset-checksum failure;
- enable failure rollback;
- unmanaged name collision;
- modified managed asset;
- invalid policy;
- incompatible CLI/Hermes version;
- uninstall clean/modified;
- no state database access;
- no secret/path leakage.

Also run install/status/doctor/uninstall against the pinned real Hermes profile.

### 12.11 Completion checks

- A profile can be fully integrated and removed without manual YAML edits.
- Other profiles and user plugins are byte-for-byte unchanged.
- A failed install leaves either the prior valid plugin or no plugin, never a
  half-installed one.

## 13. Workstream 8: Hermes plugin hooks and policy runtime

### 13.1 Goal

Load a real plugin that registers safely, reports managed lifecycle evidence,
and enforces the configured delegated-tool guardrails before any Pohunek
subprocess starts. These checks do not claim to sandbox same-user shell or
file-write capabilities.

### 13.2 Plugin bootstrap

Implement the real Hermes entrypoint:

1. read the managed policy once;
2. validate schema and owner-private permissions where Python can do so
   portably;
3. verify the configured Pohunek CLI version;
4. derive the origin managed session from trusted environment;
5. register only tools allowed by access mode;
6. register lifecycle hooks only for managed-session reporting;
7. register the skill only after required tools are available;
8. fail closed with concise diagnostics.

The plugin must not modify `sys.path` globally beyond Hermes's supported plugin
loading mechanism and must not add third-party runtime dependencies that are
absent from the supported Hermes installation.

### 13.3 Policy enforcement

Before CLI invocation:

- resolve host against the installed allowlist;
- resolve tool against access mode;
- reject `session.stop`, `session.resume`, `session.remove`, `session.fork`,
  `session.resize`, `session.set_metadata`, `session.rename`, and
  `session.input` when targeting the origin session;
- allow lifecycle calls `session.report_agent`, `session.release_agent`, and
  the necessary public fallback `session.report_native_id` to report the
  origin session;
- accept only agents returned by runtime inventory, from the compiled bound
  rather than a policy list;
- enforce timeout/result/input bounds;
- reject arbitrary endpoints, executable paths, environment maps, and extra
  arguments;
- create/reuse an idempotency key only within supported operation semantics.

Return stable plugin error codes matching the RFC.

These checks are defence in depth for the delegated plugin surface. The exact
eight-method daemon origin-session guard from workstream 3 remains authoritative
even when Hermes bypasses the plugin and invokes the CLI directly; it is not a
broader mutation policy and does not block the three lifecycle reports.

### 13.4 Lifecycle reporter

Implement a small local reporter:

- initialize immutable endpoint/runtime metadata once;
- obtain PID and process-start identity through the same supported Linux
  mechanism as existing Pohunek hooks;
- prefer the worker-private identity report and retain the hardened public
  `session.report_native_id` method as the necessary local fallback when the
  private endpoint is unavailable;
- use local public daemon methods for activity/attention;
- increment a monotonic sequence;
- apply a short configured socket deadline;
- perform no subprocess/network/database access;
- swallow and count errors;
- write no terminal output.

Reuse the approach in
`crates/daemon/src/integration/assets/claude/pohunek-agent-state.sh`: its
embedded Python already opens `POHUNEK_WORKER_SOCKET_PATH`, applies a socket
timeout, derives process start identity, and sends `identity_report`. Adapt that
reference implementation to the supported Hermes plugin API instead of
recreating the transport and identity logic.

Native identity uses the same ordered claim contract over both transports. The
worker-private endpoint is preferred; the hardened public
`session.report_native_id` method is the necessary local fallback. A
higher-sequence valid continuation-session reference supersedes the immutable
launch identity for the next resume.

Map hooks exactly as specified in RFC section 9.8. Ensure
`on_session_end` does not report process exit.

### 13.5 Hook tests

Cover:

- plugin installed but process outside Pohunek: no report;
- new managed session identity;
- resumed session identity via `pre_llm_call`;
- context compaction creates a continuation session whose new native ID is
  reasserted by `pre_llm_call`, becomes the resume reference, and is not
  overwritten by a stale/lower-sequence claim;
- working/idle transitions;
- approval blocked/resolved;
- interrupted/error/completed end;
- finalize release;
- daemon outage;
- worker-private endpoint outage;
- stale runtime/PID identity;
- duplicate/out-of-order sequence;
- socket timeout;
- SIGKILL/no-finalize fallback;
- hook exception never escapes;
- no prompt/tool content in report.

Measure hook latency under endpoint failure and enforce a regression ceiling in
the isolated environment.

### 13.6 Completion checks

- Hermes turns remain usable when Pohunek is stopped.
- Native ID becomes available only when reporting works; activity degrades to
  process/screen fallback when it does not.
- Policy rejects denied operations without spawning `pohunek`.

## 14. Workstream 9: typed Hermes tools

### 14.1 Goal

Expose the complete, policy-bounded session lifecycle through explicit Hermes
tools.

### 14.2 Shared CLI runner

Implement one internal runner:

1. choose the installer-recorded absolute CLI path;
2. build a fixed argv list;
3. set a minimal environment;
4. pass untrusted text on stdin;
5. start with closed inherited descriptors;
6. enforce timeout and output caps;
7. terminate/kill on cancellation according to platform rules;
8. parse exactly one JSON document;
9. validate CLI protocol version;
10. redact bounded stderr;
11. map typed errors;
12. emit payload-free duration/status metrics.

No tool implements its own subprocess parsing.

### 14.3 Read tools

Implement and test:

- `pohunek_hosts`;
- `pohunek_sessions`;
- `pohunek_session_get`;
- `pohunek_session_screen`;
- `pohunek_session_output`;
- `pohunek_session_wait`;
- `pohunek_session_diff`.

Design result objects for model use:

- retain full stable IDs and cursor metadata;
- normalize terminal text through one tested normalizer;
- report UTF-8 replacement/gap/truncation;
- limit list counts and diff/output sizes;
- distinguish timeout from no change and terminal state;
- include a concise next-action hint only when mechanically derivable.

Never reinterpret terminal text as trusted instructions.

### 14.4 Manage tools

Implement and test:

- `pohunek_session_start`;
- `pohunek_session_send`;
- `pohunek_session_resume`;
- `pohunek_session_fork`;
- `pohunek_session_resize`;
- `pohunek_session_rename`;
- `pohunek_session_set_metadata`.

Tasks:

- use stdin for initial/send input;
- expose structured project/worktree/agent profile selection, not raw argv;
- resolve exact unique names before mutation;
- pass idempotency keys where supported;
- return logical and runtime IDs;
- propagate Hermes fork unsupported as data;
- restrict metadata to the allowlisted schema;
- reject dimensions and input outside limits before invoking the CLI.

### 14.5 Full-access tools

Implement and test:

- `pohunek_session_stop`;
- `pohunek_session_remove`.

They register only in `full` mode. As two members of the exact eight-method
origin guard, they cannot target the origin session. They do not bypass daemon
preconditions or accept a force flag from the model.

### 14.6 Control-loop tests

Against a real daemon and worker:

1. list a session;
2. inspect and screen it;
3. send input;
4. wait for output/activity;
5. read incremental output;
6. handle one output gap;
7. detect runtime change;
8. resume a supported session;
9. receive Hermes fork unsupported;
10. stop/remove only in full mode.

Run the same safe subset against a direct loopback remote fixture exercising
the remote transport path.

### 14.7 Completion checks

- Every RFC tool is real and connected to the actual CLI/API.
- No placeholder, mock-only implementation, or raw protocol passthrough ships.
- A bounded send/wait/screen loop can drive a peer session to idle, blocked, or
  terminal state.

## 15. Workstream 10: generated Hermes skill and knowledge

### 15.1 Goal

Teach Hermes the full Pohunek operating model from the same reviewed knowledge
source used by the Universal Pohunek Assistant.

### 15.2 Knowledge source changes

Update or add focused English sources under `docs/knowledge/` covering:

- Hermes as a managed runtime;
- native resume and unsupported fork;
- session screen/output/wait;
- plugin install/update/uninstall/doctor;
- access modes, host allowlist, and the exact eight-method self-target rule;
- all tool names and safe control loop;
- output cursor/gap/runtime-change recovery;
- remote direct-NetBird behavior;
- troubleshooting and typed errors.

Update `docs/knowledge/assistant/source-map.md` for every changed code/API/doc
surface.

### 15.3 Generator changes

Update `crates/xtask` and/or `crates/knowledge` so:

1. the Hermes skill is rendered with valid Hermes frontmatter;
2. required Pohunek tools are declared;
3. tool availability/access-mode caveats are included;
4. the output path matches the embedded Pohunek ownership manifest;
5. generation is deterministic;
6. generated checksums feed the Pohunek ownership manifest;
7. docs checks fail on stale output;
8. secret/backtick/path validation applies to the new asset.

Do not maintain a second hand-copied skill body in the Python plugin tree.

### 15.4 Behavior evaluations

Extend assistant/knowledge evaluations with scenarios:

- choose Hermes explicitly;
- start and observe a Hermes session;
- resume with native reference;
- explain unsupported Hermes fork;
- operate a peer session with send/wait/screen;
- recover from output gap/runtime change;
- refuse a disallowed host/destructive action or any of the eight guarded
  self-target methods while preserving the three lifecycle-report exceptions;
- avoid raw attach for model control;
- ask a human to attach when terminal interaction is not safely expressible.

### 15.5 Completion checks

- Generated skill matches registered tool names exactly.
- `cargo xtask docs check` detects a changed tool/API without source updates.
- Hermes can discover the skill in the pinned real profile.

## 16. Workstream 11: GUI, web, assistant, and packaging parity

### 16.1 GUI core and native GUI

Review/update at least:

- `crates/gui-core/src/state.rs`;
- `crates/gui-core/src/message.rs`;
- `crates/gui-core/src/sdk.rs`;
- `crates/gui/src/view/session.rs`;
- `crates/gui/src/view/modals.rs`;
- related tests.

Tasks:

- render Hermes in picker/list/detail/filter;
- drive resume/fork actions from capabilities;
- expose screen/output where the current UI has equivalent terminal detail;
- render provider-keyed notification policy;
- preserve unknown-agent neutral fallback;
- avoid I/O/business logic in the Iced view crate.

### 16.2 Universal assistant selection

Update assistant runtime selection so:

- `--agent hermes` is valid;
- a configured Hermes-based Pohunek profile is valid;
- auto-selection can use Hermes when higher-priority configured runtimes are
  unavailable;
- existing preference order is not silently changed without an explicit
  documented decision;
- capability validation rejects a runtime lacking required assistant behavior.

Update relevant code under:

- `crates/daemon/src/assistant.rs`;
- `crates/cli/src/commands/assistant/`;
- `crates/gui-core/src/assistant.rs`;
- assistant tests and knowledge.

### 16.3 Web SDK and client

Review/update:

- `web/sdk/src/client.ts`;
- `web/sdk/src/error.ts`;
- SDK tests and mock daemon;
- `web/client-core/src/`;
- `web/frontend/src/components/AgentBadge.svelte`;
- session/new/filter/detail components;
- notification policy UI;
- fixtures and Playwright scenarios;
- backend real-daemon E2E.

Tasks:

- expose typed screen/output/wait SDK calls;
- support Hermes enum/labels;
- make action availability capability-driven;
- handle output gap/runtime change;
- update fixture daemon methods;
- update provider-keyed notification policy;
- ensure unknown future providers do not crash reducers/views.

### 16.4 Scripts and setup

Review `scripts/`, setup, doctor, desktop launchers, and examples for hard-coded
Codex/Claude choices. Add Hermes only where semantics apply.

### 16.5 Release packaging

Ensure:

- the CLI binary embeds all plugin assets and generated skill;
- checksums are reproducible;
- release smoke can install into an isolated Hermes profile;
- packaging does not require source-tree asset paths at runtime;
- licenses/notices cover shipped code and do not copy unlicensed Hermes marks;
- the worker binary is built and installed beside the daemon as required;
- release notes state protocol/notification-policy/downgrade implications.

### 16.6 Completion checks

- Every client can display a Hermes session.
- Unsupported Hermes fork is shown consistently.
- Web/Rust generated contracts agree.
- Release artifacts install a working plugin without the source tree.

## 17. Workstream 12: documentation, migration, and operations

### 17.1 User and architecture documentation

Update:

- `README.md`;
- `docs/architecture.md`;
- `docs/public-api.md`;
- `docs/README.md`;
- relevant roadmap/phase records;
- configuration reference;
- integration setup/troubleshooting docs;
- session operations runbook;
- release/migration notes.

Document:

- supported Hermes version policy;
- first-class runtime launch/resume/fork behavior;
- profile isolation;
- plugin commands and access policy;
- no `state.db` access;
- session screen/output/wait wire contracts;
- JSON CLI contract;
- lifecycle hook fallback;
- direct remote transport;
- upgrade order and unsupported downgrade;
- doctor/error recovery.

### 17.2 Public API examples

Add redacted examples for:

- `session.screen`;
- initial and cursor-based `session.output`;
- output gap;
- `session.wait` wake and timeout;
- Hermes new/resume/fork-unsupported;
- provider-keyed notification policy;
- JSON CLI stdin input.

Examples must match golden fixtures or be generated/checked from the protocol
types.

### 17.3 Upgrade and rollback runbook

Document this release order:

1. inventory every local and NetBird-reachable daemon plus every CLI, GUI, web
   backend/SDK, and plugin client that must cross the public protocol boundary;
2. stop or drain cross-host automation so a newly upgraded client is not sent
   to a legacy exact-version daemon, or vice versa;
3. on each host, install the new `pohunek-sessiond` beside `pohunekd`, then the
   matching daemon and local clients;
4. restart/reconcile that daemon and verify old live workers under the private
   current/previous compatibility window;
5. upgrade all remaining hosts and clients before re-enabling cross-host
   operations; verify public min/max range negotiation on every host pair;
6. update/install the Hermes plugin per selected profile;
7. run integration doctor;
8. launch one canary Hermes session;
9. verify native ID, continuation replacement, screen/output, wait, and resume;
10. then enable remote/full policies as explicitly desired.

Steps 1-5 apply exactly once, for M1's exact-version-to-range-negotiation
transition, which has no mixed-fleet public compatibility window: a partially
upgraded fleet is intentionally offline for cross-version public calls until
both endpoints are upgraded. From M1 onward peers negotiate the highest
overlapping version, so M2 and M3 need only an ordinary rollout and no
coordinated boundary. Steps 6-10 apply when M3 installs and validates the
plugin after M2.

Document rollback limitations:

- disabling/uninstalling the plugin is safe and does not remove sessions;
- existing non-Hermes workers continue under the compatibility window;
- binary downgrade after persisting Hermes enum values or the new notification
  policy is unsupported;
- recovery is upgrade-forward, not an old-shape compatibility shim.

### 17.4 Completion checks

- A new operator can install and diagnose the integration from docs alone.
- Every new public method/error is in `docs/public-api.md`.
- Upgrade and downgrade constraints are explicit.
- Knowledge and generated skill are current.

## 18. Complete Definition of Done

The checkboxes below are the union required to complete the full RFC.
Section 18.10 defines the subset that gates each independently releasable
milestone; no milestone may claim items assigned to a later milestone.

### 18.1 Runtime

- [ ] `AgentKind::Hermes` is present in all Rust and TypeScript surfaces.
- [ ] `hermes chat` launches in a real Pohunek PTY.
- [ ] Multiline input works using validated framing.
- [ ] Native Hermes ID is reported without reading `state.db`.
- [ ] Daemon restart preserves the live worker, PTY, terminal, output, and
      native ID.
- [ ] Resume uses the recorded native ID under the same logical session.
- [ ] Resume creates a new runtime ID/generation.
- [ ] Hermes fork returns `agent_fork_unsupported` with no side effects.
- [ ] Hermes-based Pohunek profiles validate and run.
- [ ] Process/screen fallback works when the plugin is absent.
- [ ] Runtime inventory and doctor report Hermes accurately.

### 18.2 Observation protocol

- [ ] `session.screen` returns bounded rendered terminal state.
- [ ] `session.output` supports tail, cursor, pagination, gap, wait, and runtime
      mismatch.
- [ ] Output larger than one frame is chunked correctly.
- [ ] `session.wait` is race-free, non-polling, uses a dedicated connection,
      and terminates by result, required timeout, or daemon shutdown without a
      disconnect-cancellation guarantee.
- [ ] External sessions return a typed no-managed-terminal error.
- [ ] Previous workers remain usable and report observation unsupported.
- [ ] Public protocol and private worker versions are bumped and tested.
- [ ] Rust/TypeScript types and `docs/public-api.md` agree.

### 18.3 CLI and SDK

- [ ] Rust SDK exposes every new operation.
- [ ] CLI exposes JSON parity for every plugin-used operation.
- [ ] JSON stdout contains exactly one document.
- [ ] Typed errors are machine-readable.
- [ ] New/input prompt text uses stdin and never argv.
- [ ] Remote host calls use direct NetBird transport.
- [ ] Local client cancellation and configured daemon limits are enforced
      without claiming disconnect-cancellation of daemon waiters.

### 18.4 Plugin and policy

- [ ] A real Hermes plugin loads in the pinned supported release.
- [ ] Install is local, profile-aware, atomic, idempotent, and owner-private.
- [ ] Update and uninstall preserve unrelated Hermes files/state.
- [ ] Name collision and modified assets fail safely.
- [ ] Plugin enablement uses supported Hermes APIs/CLI, not YAML text surgery.
- [ ] Access mode is explicit.
- [ ] Host allowlist is explicit.
- [ ] Agent/profile and metadata inputs are constrained.
- [ ] The daemon authoritatively rejects `session.stop`, `session.resume`,
      `session.remove`, `session.fork`, `session.resize`,
      `session.set_metadata`, `session.rename`, and `session.input` for the
      origin session, and the plugin repeats that exact check before subprocess
      start as defence in depth. `session.report_agent`,
      `session.release_agent`, and the necessary public fallback
      `session.report_native_id` remain allowed.
- [ ] Stop/remove register only in `full`.
- [ ] CLI version incompatibility disables tools safely.
- [ ] Doctor covers files, policy, versions, tools, skill, and hook dry run.

### 18.5 Hooks

- [ ] New and resumed native IDs are reported.
- [ ] Context-compaction continuation IDs supersede the immutable launch
      identity through ordered `pre_llm_call` reassertion and are used by the
      next resume.
- [ ] Working, blocked, idle, interruption, error, and finalize mappings match
      the RFC.
- [ ] `on_session_end` is not treated as process exit.
- [ ] Hook failure never fails a Hermes turn.
- [ ] Hooks perform no subprocess, network, or database access.
- [ ] Hook deadline and latency regression are tested.
- [ ] Stale/out-of-order/cross-runtime reports are rejected.
- [ ] Prompt/tool/output payloads never enter reports or logs.

### 18.6 Tools and skill

- [ ] Every read tool is implemented against the real CLI.
- [ ] Every manage tool is implemented against the real CLI.
- [ ] Stop/remove work only in `full`.
- [ ] No raw attach or arbitrary-method tool exists.
- [ ] Send/wait/screen control loop reaches a deterministic terminal/activity
      outcome.
- [ ] Output gap and runtime change are recoverable.
- [ ] Tool output is bounded, normalized, and cursor-preserving.
- [ ] Bundled skill is generated from `docs/knowledge`.
- [ ] Skill tool requirements match registered tools.
- [ ] Pinned Hermes discovers tools and skill.

### 18.7 Notifications and clients

- [ ] Notification policy is provider-keyed, landed in M1.
- [ ] `AgentKind::Hermes` is added in M2 with no public protocol bump, and an
      unknown value renders neutrally on an older M1 peer.
- [ ] Hermes attention is sanitized and projected.
- [ ] CLI, GUI, web SDK, client core, frontend, and fixtures support Hermes.
- [ ] Resume/fork actions are capability-driven.
- [ ] Unknown future agents do not crash clients.
- [ ] Universal assistant can select Hermes explicitly and by documented
      fallback.

### 18.8 Security and operations

- [ ] No secret, `.env`, key, certificate, or Hermes database is read.
- [ ] Installer path traversal, symlink escape, unsafe owner/mode, and broad
      target tests pass.
- [ ] Subprocesses use argv, fixed allowlists, minimal environment, timeout,
      output cap, and redaction.
- [ ] Terminal/prompt payloads are absent from logs/errors/notifications.
- [ ] Remote/full policy must be explicitly enabled.
- [ ] Documentation and tests treat plugin policy as a delegated-tool
      guardrail, not a sandbox against same-user shell/file-write bypass.
- [ ] Upgrade and unsupported downgrade are documented.
- [ ] Release binary embeds working plugin assets.

### 18.9 Tests and documentation

- [ ] Deterministic unit/integration tests cover happy, error, and edge paths.
- [ ] Real daemon + real worker E2E covers runtime and tools.
- [ ] Pinned real-Hermes smoke covers plugin, hooks, skill, input, and resume.
- [ ] Rust full gates pass.
- [ ] Web full gates pass.
- [ ] Real-daemon web suite passes.
- [ ] `cargo xtask ts check` passes.
- [ ] `cargo xtask docs check` passes.
- [ ] README, architecture, public API, knowledge, runbooks, and release notes
      are current.
- [ ] Diff review finds no unrelated changes or dead-code removal.

### 18.10 Independently releasable milestone subsets

**M1 — provider-neutral foundations**

- [ ] Resume and fork are independent for existing agents, with no
      Codex/Claude regression.
- [ ] Every item in sections 18.2 and 18.3 passes, including derived
      control-line bounds and the dedicated-connection wait contract.
- [ ] The daemon rejects all eight guarded origin-session methods —
      `session.stop`, `session.resume`, `session.remove`, `session.fork`,
      `session.resize`, `session.set_metadata`, `session.rename`, and
      `session.input` — through direct CLI/API bypass. Lifecycle
      `session.report_agent`, `session.release_agent`, and the necessary public
      fallback `session.report_native_id` remain allowed, and the existing
      self-feeding attach behavior remains green.
- [ ] The single public break is complete: range negotiation, the provider-keyed
      notification policy, and forward-compatible `AgentKind` all land in M1, an
      unknown wire value round-trips to the neutral variant, and that variant is
      rejected by every mutating path.
- [ ] `session.report_native_id` carries runtime identity, PID start identity,
      sequence, and expiry; the daemon enforces the same rejection rules as the
      private claim path; both existing hook scripts send the new fields.
- [ ] Public range negotiation, the private compatibility window, generated
      TypeScript, `docs/public-api.md`, and relevant knowledge are current.
- [ ] Relevant deterministic, real-daemon/worker, Rust, web, TypeScript, and
      docs gates pass.

**M2 — first-class Hermes runtime**

- [ ] The launch, PTY input, daemon-restart durability, profile, fallback,
      inventory, and unsupported-fork items in section 18.1 pass with local
      terminal backend only.
- [ ] Resume capability and exact argv are implemented and tested against an
      injected valid native reference; without M3 lifecycle reporting, a real
      session with no reference returns the typed missing-reference error.
- [ ] `AgentKind::Hermes` is added additively with no public protocol bump, and
      an M1 peer that predates the value still renders it neutrally.
- [ ] Every non-plugin client item in section 18.7 passes.
- [ ] Process/screen fallback remains bounded when lifecycle integration is
      absent.
- [ ] Relevant deterministic, real-Hermes PTY, Rust, web, TypeScript, docs, and
      release gates pass.

**M3 — Hermes operator plugin**

- [ ] Every item in sections 18.4, 18.5, and 18.6 passes, including the real
      `plugin.yaml`/`__init__.py` API, continuation-session identity, and
      generated `pohunek:pohunek` skill.
- [ ] Policy is Pohunek-owned outside the plugin checksum set, and policy-file
      tampering is diagnosed without being represented as a same-user sandbox.
- [ ] Plugin-sourced notification items in section 18.7 and all applicable
      security/operations items in section 18.8 pass.
- [ ] Every full-RFC test/documentation item in section 18.9 passes; therefore
      M3 completion also proves the union of M1, M2, and M3.

## 19. Test matrix

| Layer | Deterministic coverage | Real coverage |
|---|---|---|
| Protocol | serde/golden/version/error tests | daemon/client negotiation |
| Worker | in-process PTY/output fixtures | spawned `pohunek-sessiond` |
| Daemon | registry/watch/handler tests | real daemon + worker |
| Hermes adapter | argv/input/detection fixtures | recorded real-Hermes PTY goldens |
| CLI | parser + subprocess JSON tests | local/remote daemon calls |
| Installer | temporary homes + controlled executable | pinned Hermes profile |
| Hooks | socket/order/failure tests | registration in pinned Hermes; ordering from goldens |
| Tools | policy/runner/result tests | real plugin -> CLI -> daemon |
| Skill | generator/eval/drift tests | real Hermes discovery (model-free) |
| GUI/web | reducer/component/fixture tests | Playwright + real-daemon E2E |
| Security | property/boundary/redaction tests | release smoke in isolated home |

The controlled executable used for deterministic failure injection does not
replace the pinned real-Hermes suite. The real column is model-free: no row
depends on a live model turn in CI, and every turn-dependent terminal state is
covered by a recorded golden refreshed out of band.

## 20. Required verification commands

Run the narrowest relevant loop after each workstream, then the full gates
before completion.

Rust:

```bash
cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --all-features
cargo build -p pohunek-session-worker --bin pohunek-sessiond
cargo test --workspace --all-features
cargo build --workspace --release
cargo xtask ts check
cargo xtask docs check
```

Web:

```bash
cd web
bun install --frozen-lockfile
bun run typecheck
bun run lint
bun test
bunx playwright install --with-deps chromium
bun run test:e2e
```

Real daemon:

```bash
cargo build -p pohunek-daemon -p pohunek-session-worker
POHUNEK_E2E=1 \
POHUNEK_DAEMON_BIN=/absolute/path/to/target/debug/pohunekd \
  bun test sdk/test/e2e.test.ts backend/test/real-daemon.e2e.test.ts
```

Hermes compatibility:

```text
Two commands are added by workstream 0. Both must create an isolated
environment/profile and must not use the operator's real Hermes configuration.

1. The model-free compatibility suite. CI gates on this one: version and CLI
   shape, plugin list/enable/disable, tool/skill/hook registration, target
   resolution, and integration install/status/doctor/uninstall.
2. The golden refresh command, run by the operator against a real provider when
   turn-dependent terminal fixtures need updating. CI never runs it.
```

The implementation must add both as stable repository commands and document the
CI-gated one in `AGENTS.md`. The final report records exactly
which commands ran and any environmental skips; it must not claim green for an
unrun gate.

## 21. Review checklist

### 21.1 Architecture review

- Does any Hermes-specific behavior leak into the daemon where a
  provider-neutral protocol belongs?
- Does any Python code duplicate Pohunek transport or NetBird?
- Does the worker remain the sole live PTY/process owner?
- Is raw attach still human-only?
- Are resume and fork semantically separate?
- Is Hermes private state untouched?

### 21.2 Security review

- Can model input influence executable, argv shape, endpoint, environment, or
  filesystem target?
- Does the delegated plugin surface reject a host outside policy without
  claiming to constrain same-user shell/file-write bypass?
- Can `manage` stop/remove?
- Does the daemon reject exactly `session.stop`, `session.resume`,
  `session.remove`, `session.fork`, `session.resize`,
  `session.set_metadata`, `session.rename`, and `session.input` for the origin
  session even when Hermes invokes the CLI directly, while allowing
  `session.report_agent`, `session.release_agent`, and the necessary public
  fallback `session.report_native_id`?
- Can policy-file tampering raise delegated capability, and is that limitation
  diagnosed and documented rather than misrepresented as a sandbox?
- Can terminal/prompt content reach logs, errors, or notifications?
- Can install/update/uninstall escape the profile or overwrite user files?
- Do retries duplicate a mutating request?

### 21.3 Compatibility review

- Are all public version match arms and generated types updated?
- Does public min/max range negotiation cover overlap, no overlap, legacy
  rejection, and remote-host diagnostics?
- Does current/previous private worker compatibility behave as documented?
- Are old configuration-shape and downgrade limitations explicit?
- Does plugin/CLI incompatibility fail closed?

### 21.4 Product review

- Can Hermes discover how to operate Pohunek without loading all docs eagerly?
- Can it observe progress without raw attach or busy polling?
- Are blocked, timeout, output gap, runtime change, and unsupported fork
  understandable?
- Do human CLI/GUI/web surfaces match the same capability model?

## 22. Risk register and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Hermes plugin API changes | plugin fails to load | pinned real-Hermes CI, explicit supported-release checks, fail-closed registration |
| Hooks block the turn | poor Hermes latency | no subprocess/network, short socket deadline, regression timing test |
| PTY input differs by Hermes UI mode | lost or malformed prompts | real PTY fixtures, explicit per-mode rule, no heuristic guessing |
| Output replay exceeds frame limit | disconnect or memory spike | bounded reads, multi-frame chunking, >1 MiB and retention-cap tests |
| Wait lost wakeup | stalled model loop | snapshot/register/recheck algorithm and race test |
| Prompt injection triggers delegated mutation | unintended session action | explicit access/host/agent guardrails, no arbitrary plugin command, full-only delegated destruction, daemon safety checks |
| Same-user Hermes tampers with policy or bypasses the plugin | delegated scope escalation | document that policy is not a sandbox, keep policy outside plugin assets, diagnose permissions/schema, rely on daemon safety and the exact eight-method origin guard from decision 21, including its lifecycle-report exceptions |
| Plugin terminates its own process | missing result/corruption | reject origin-targeted `session.stop` and `session.remove` as two members of the exact eight-method guard; do not block lifecycle reports |
| PATH/plugin hijack | arbitrary code execution | recorded canonical executables, owner/mode checks, embedded checksums |
| Hermes profile collision | user data overwrite | ownership marker, collision error, marker-driven uninstall |
| CLI/protocol mismatch | misinterpreted tools | versioned JSON envelope and fail-closed plugin |
| The single M1 protocol transition breaks mixed versions | fleet-wide local/remote outage, once | explicit M1 runbook, inventory all clients/hosts, range negotiation afterwards so M2/M3 and future providers never repeat it, drain cross-host calls, coordinated upgrades |
| New worker feature breaks old live workers | session disruption | current/previous capability negotiation |
| Notification schema breaks config | lost policy | explicit pre-1.0 upgrade note, deterministic new schema, no silent shim |
| Real-Hermes tests need credentials | unreliable CI | model-free CI suite (version/CLI shape, plugin enable, tool/skill/hook registration, installer); turn-dependent fixtures are recorded goldens, no provider server built |
| Abandoned waiter exhausts waiter caps | temporary `session_waiter_limit_reached` for legitimate callers | short maximum wait so slots free within seconds, caps documented as an availability bound, slot-release test |
| Sensitive PTY content leaks | confidentiality loss | explicit-only bounded APIs, payload-free logs/errors/notifications |

## 23. Release readiness

Before tagging the release:

1. Verify release binaries, not debug/source-tree execution.
2. Install the plugin from the release `pohunek` binary into a fresh isolated
   Hermes profile.
3. Run doctor.
4. Launch a real Hermes managed session.
5. Confirm native ID and activity reporting.
6. Read screen and incremental output.
7. Resume the same native Hermes session under the same logical Pohunek
   session.
8. Run a real Hermes tool against a peer fixture session.
9. Confirm manage/full/host policy denials, denial of each of `session.stop`,
   `session.resume`, `session.remove`, `session.fork`, `session.resize`,
   `session.set_metadata`, `session.rename`, and `session.input` for the origin
   session, and continued delivery of `session.report_agent`,
   `session.release_agent`, and the necessary public fallback
   `session.report_native_id`.
10. Update and uninstall the plugin, verifying unrelated profile state is
    unchanged.
11. Verify a previous-version live worker remains reconciled after daemon
    upgrade.
12. Archive only redacted compatibility/gate results.

## 24. Post-release validation

Post-release checks are validation, not deferred required implementation:

- monitor hook timeout/fallback rate;
- monitor screen/output/wait latency and waiter limits;
- monitor typed plugin incompatibility and policy-denial counts;
- verify no payload-bearing fields entered structured logs;
- track upstream Hermes hook/plugin API releases;
- update the supported Hermes version range only after the pinned compatibility
  suite passes.

Any future MCP adapter, external Hermes session observation, or newly documented
Hermes native fork is a separate RFC. None should be smuggled into maintenance
of this integration without revisiting its trust and lifecycle semantics.

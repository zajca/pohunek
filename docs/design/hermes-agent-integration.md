# Design: First-Class Hermes Agent Integration

Status: proposed (RFC)

Research baseline: 2026-07-27

Companion implementation plan:
[`hermes-agent-integration-plan.md`](hermes-agent-integration-plan.md).

## 1. Executive decision

Pohunek will integrate Hermes in two complementary roles:

1. **Hermes as a managed agent runtime.** A Pohunek session can launch,
   supervise, resume, inspect, and classify a Hermes process with the same
   durable-session guarantees as Codex and Claude Code.
2. **Hermes as a Pohunek operator.** A per-profile Hermes plugin exposes a
   typed Pohunek tool set, lifecycle hooks, and a bundled skill so that Hermes
   can safely create and operate Pohunek sessions on the local or explicitly
   allowed remote hosts.

The integration is not implemented as a Hermes-specific protocol in the
daemon. The public Pohunek protocol remains the source of truth. Missing
provider-neutral session-observation methods are added to that protocol and
are then consumed by the Rust CLI, the Hermes plugin, the web SDK, and future
agent integrations.

The Hermes plugin uses a stable JSON CLI surface rather than implementing the
Pohunek Unix/TCP transport and NetBird host resolution again in Python. It
invokes `pohunek` with an argument vector, never through a shell, sends
untrusted text through standard input, and parses exactly one JSON response.

The primary Hermes integration is a native plugin, not an MCP server. A plugin
is the only one of the two mechanisms that can deliver all three required
capabilities together:

- lifecycle hooks inside the Hermes process;
- typed tools available to the model;
- a bundled Pohunek skill with progressive disclosure.

The provider-neutral protocol additions intentionally keep a future
`pohunek mcp serve` adapter possible, but an MCP server is not part of this
milestone.

## 2. Objective

After this design is implemented, an operator can:

- select `hermes` wherever Pohunek accepts a built-in agent;
- launch Hermes in a real PTY owned by `pohunek-sessiond`;
- retain the same logical Pohunek session across daemon restarts and native
  Hermes resumes;
- inspect Hermes runtime identity, activity, current terminal screen, and
  incremental PTY output;
- install a version-matched Pohunek plugin into a selected Hermes profile;
- ask Hermes to list, inspect, start, send input to, wait for, resume, and
  otherwise manage Pohunek sessions through typed tools;
- constrain that plugin by host allowlist and mutation policy;
- obtain typed, actionable errors rather than scraped human CLI output;
- upgrade or remove the managed plugin without overwriting unrelated Hermes
  configuration or user-authored plugins.

## 3. Success criteria

The integration is successful only when all of the following are true:

1. `AgentKind::Hermes` is supported across the Rust protocol, SDK, daemon, CLI,
   GUI core, GUI, generated TypeScript, web client, testkit, fixtures, and
   documentation.
2. A managed Hermes session survives a `pohunekd` restart without losing its
   worker, PTY, runtime generation, output history, or native Hermes identity.
3. `session.resume` launches Hermes with the recorded native session reference
   and creates a new Pohunek runtime under the same logical session.
4. `session.fork` returns a stable typed unsupported-capability error for
   Hermes until Hermes documents a native fork operation.
5. Hermes lifecycle hooks improve native identity and activity state but are
   not required for session correctness; process and screen detection remain
   bounded fallbacks.
6. A Hermes tool can observe a session without using raw interactive attach,
   and can advance a control loop without polling unbounded logs.
7. Plugin installation is explicit, profile-aware, idempotent, atomic, and
   owner-private.
8. Read, ordinary mutation, destructive mutation, and remote-host access are
   independently configured on the plugin's delegated tool surface, while the
   daemon independently and authoritatively protects the origin session.
9. All new wire methods and errors have Rust and TypeScript parity and are
   documented in `docs/public-api.md`.
10. Deterministic tests cover the runtime, tools, hooks, resume behavior, and
    failure paths; a pinned real-Hermes suite covers plugin loading, tool/skill
    registration, and the installer without requiring a model provider, and
    turn-dependent terminal fixtures are recorded goldens.
11. The assistant knowledge bundle and its source map describe the shipped
    behavior, and `cargo xtask docs check` detects drift.
12. The complete Rust and web gate sets in `AGENTS.md` pass.

## 4. Scope

### 4.1 In scope

- first-class `hermes` agent identity;
- Hermes process launch, input, detection, native resume, and lifecycle
  reporting;
- Hermes support in agent profiles;
- Hermes runtime inventory and host inspection;
- provider-neutral terminal screen, incremental output, and bounded wait APIs;
- missing JSON CLI parity required by an agent client;
- a per-Hermes-profile Pohunek plugin;
- typed read, manage, and destructive tool groups;
- a generated, bundled Pohunek skill;
- plugin installation, diagnosis, update, and uninstall;
- local and direct NetBird remote session operation;
- notification-policy support for Hermes;
- GUI and web representation of Hermes sessions;
- protocol/versioning, security, telemetry, documentation, and release
  packaging;
- deterministic and real-Hermes validation.

### 4.2 Explicit non-goals

- a central Pohunek service;
- SSH transport;
- multi-user authorization or tenant isolation;
- a daemon-hosted Hermes API;
- parsing or modifying Hermes's private `state.db`;
- importing arbitrary existing Hermes sessions as writable Pohunek sessions;
- observing every external Hermes process across every possible
  `HERMES_HOME`;
- emulating a Hermes fork by copying a worktree or transcript;
- giving an LLM raw interactive attach ownership;
- exposing an arbitrary public-protocol passthrough tool;
- installing the plugin into every Hermes profile automatically;
- supporting Hermes terminal backends other than the local backend; Docker,
  SSH, Singularity, Modal, and Daytona execution fall outside Pohunek's
  worktree, diff, and local-process ownership model;
- an MCP server in this milestone;
- a compatibility shim for pre-1.0 protocol or configuration shapes.

External-session observation can be designed separately. Managed Hermes
sessions are complete without it because Pohunek owns the PTY, worker, and
runtime lifecycle from launch.

## 5. Terminology

- **Logical session**: the durable Pohunek session record identified by
  `session_id`.
- **Runtime**: one process generation within a logical session, identified by
  `runtime_id` and `runtime_generation`.
- **Native session reference**: a Hermes session ID or title accepted by
  `hermes chat --resume`.
- **Hermes profile**: an isolated Hermes home containing its own configuration,
  state, skills, and plugins.
- **Managed plugin**: a plugin directory installed by Pohunek and carrying a
  Pohunek ownership marker and asset version.
- **Origin session**: the Pohunek session in which the current Hermes plugin is
  running, when applicable.
- **Mutation policy**: the installed plugin's `read_only`, `manage`, or `full`
  access mode.
- **Terminal watermark**: a monotonic terminal-screen revision counter.
- **Output offset**: a monotonic byte offset within one Pohunek runtime's PTY
  output stream.

## 6. Verified Hermes capabilities

This design relies only on public Hermes behavior documented as of the research
baseline.

### 6.1 Profiles and state

Hermes stores sessions in the selected profile's `state.db`. The default home
is `~/.hermes`; named profiles have isolated homes under
`~/.hermes/profiles/<name>`. A custom `HERMES_HOME` can relocate the active
home.

Pohunek treats the Hermes database as private implementation detail. It stores
only the native reference reported by Hermes and passes that reference back to
Hermes on resume.

### 6.2 Native resume

Hermes accepts `--resume`/`-r`, `--continue`, and `--pass-session-id`. The
Pohunek adapter uses the explicit, stable command form:

```text
hermes chat
hermes chat --resume <native-session-reference>
```

Pohunek does not use `--continue`, because it is implicit ambient state rather
than the reference recorded on the logical session. It does not use
`--pass-session-id`, because a Pohunek session ID is not a Hermes session ID.

Hermes does not currently document a native fork-session operation. Pohunek
therefore reports fork as unsupported rather than pretending that a new
worktree or copied transcript is equivalent.

### 6.3 Plugins

Hermes plugins can register:

- tools;
- lifecycle and tool hooks;
- slash commands;
- Hermes CLI commands;
- bundled skills.

Plugins live below the selected `HERMES_HOME/plugins` directory and must be
enabled in that profile. This makes a per-profile managed plugin the correct
distribution unit.

### 6.4 Hook semantics

The design observes these important Hermes semantics:

- `on_session_start` fires only for a new native session, not a continuation;
- `pre_llm_call` fires for each turn and therefore reasserts identity and
  activity after resume;
- `post_llm_call` fires only after a successful model turn;
- `on_session_end` fires after a conversation run, not at process exit;
- `on_session_finalize` is the process/CLI teardown signal;
- approval hooks surround interactive approval requests and responses.

Hooks are evidence, not the source of truth. They may be skipped by abrupt
termination, plugin failure, or a future Hermes behavioral change.

### 6.5 Hook latency constraint

Hermes currently invokes plugin lifecycle hooks synchronously on latency-
sensitive paths. The Pohunek hook implementation therefore:

- performs no subprocess invocation;
- performs no network request;
- does not read `state.db`;
- does not parse general configuration on each call;
- caps any local socket attempt with a short configured deadline;
- treats every report as best effort;
- never writes diagnostics to the terminal;
- never raises a reporting failure into the Hermes turn.

The implementation plan includes a compatibility test that records the pinned
Hermes behavior so an upstream change is visible.

## 7. Current Pohunek gaps

The current `main` branch provides most required primitives, but not the full
integration surface:

- `AgentKind` contains only `shell`, `codex`, and `claude`;
- compiled agent adapters couple resume style with Claude's fork flag;
- the integration installer accepts only Codex and Claude;
- runtime inventory, labels, filters, and notification policy switch
  exhaustively over the existing providers;
- the worker tracks a structured terminal screen, but the public API cannot
  read it;
- public attach exposes raw bytes, but it is an interactive, stateful stream
  unsuitable as an LLM tool;
- the CLI does not expose JSON parity for every public session operation;
- session input has no standard-input form suitable for untrusted multiline
  model text;
- there is no bounded session wait primitive;
- the notification policy has provider-specific Codex and Claude fields rather
  than a provider-keyed collection;
- the assistant materialization has no Hermes-specific operating guide.

These are product gaps, not reasons to put Hermes-specific behavior into the
daemon. The solution extends provider-neutral contracts first.

## 8. Target architecture

```text
Hermes process in a Pohunek PTY
  |
  +-- Pohunek Hermes plugin
  |     +-- lifecycle hooks ------> worker-private identity report
  |     |                          \-> public daemon activity/attention report
  |     +-- bundled Pohunek skill
  |     \-- typed Pohunek tools
  |            |
  |            \-- exec argv + JSON + stdin
  |                    |
  |                    v
  +---------------> pohunek CLI
                         |
                         +-- local Unix socket
                         \-- direct NetBird TCP
                                  |
                                  v
                               pohunekd
                                  |
                                  +-- logical registry
                                  +-- bounded wait/watch state
                                  \-- pohunek-sessiond
                                         +-- PTY/process
                                         +-- terminal screen
                                         \-- output history
```

There are three intentionally separate contracts:

1. **Pohunek public protocol**: authoritative session operations and
   observation, shared by all clients.
2. **Pohunek CLI JSON surface**: a stable process boundary used by the Hermes
   plugin; it maps one-to-one to typed SDK operations.
3. **Hermes plugin API**: Hermes-native tool and hook registration. It contains
   policy and presentation, not session lifecycle logic.

## 9. Hermes as a managed runtime

The integration supports only Hermes's local terminal backend. Pohunek owns the
PTY, process, worktree, and diff on the same host; selecting Docker, SSH,
Singularity, Modal, or Daytona would invalidate those ownership assumptions and
is rejected or diagnosed as unsupported.

### 9.1 Agent identity

The public enum gains:

```rust
AgentKind::Hermes
```

Its wire value, CLI value, profile base value, display identifier, process
manifest key, and notification-provider key are all the lowercase string
`"hermes"`.

The new enum variant is propagated exhaustively. Adding it is a purely additive
change that requires no public protocol bump, because M1 already makes
`AgentKind` forward compatible: an unknown wire value deserializes into a
neutral variant instead of failing (see section 20.1).

The neutral variant is presentation-only. It is never launchable, `session.new`
and every other mutating path reject it with a typed error, it is never
persisted as a way to smuggle an unknown agent into the store, and Rust and
TypeScript agree on its representation.

### 9.2 Compiled adapter

`HermesAdapter` owns:

- executable: `hermes`;
- initial fixed argument: `chat`;
- native reference kind: Hermes session ID or title;
- resume operation: append `--resume <reference>`;
- fork capability: unsupported;
- input framing: a validated Hermes-specific rule set;
- process and screen detection manifests;
- lifecycle-hook installation metadata.

The adapter does not inspect `state.db`, infer the last session, or derive a
native ID from terminal output when an authoritative hook report is available.

### 9.3 Resume and fork capabilities

The current resume representation must stop implying that every flag-based
resume supports Claude's `--fork-session`.

Compiled adapters expose independent capabilities:

```text
resume:
  unsupported
  flag(argument, reference_kind)
  subcommand(name, reference_kind)

fork:
  unsupported
  resume_with_extra_flag(argument)
```

The exact Rust types follow existing naming conventions, but the separation is
normative.

Built-in behavior becomes:

| Base agent | Resume | Fork |
|---|---|---|
| Shell | unsupported | unsupported |
| Codex | adapter-defined native resume | existing supported behavior |
| Claude | existing resume behavior | `--fork-session` |
| Hermes | `chat --resume <reference>` | unsupported |

Agent profiles inherit the compiled base's fork capability. A profile may
disable inherited fork support, but it cannot enable a fork mode that the base
adapter does not implement. This prevents a configuration string from turning
arbitrary arguments into a purported semantic fork.

### 9.4 Profile support

`base = "hermes"` becomes valid in an agent profile. A Hermes-based profile can
configure the same safe launch properties as other profiles:

- executable override;
- fixed arguments such as a specific Hermes profile;
- environment additions;
- working-directory policy;
- input timing overrides within validated bounds;
- resume override when the wrapper preserves Hermes semantics;
- detection aliases.

A profile cannot silently change its base identity or enable unsupported fork.
Profile validation fails fast with a typed path-specific error.

For a named Hermes profile, a typical Pohunek agent profile may use:

```toml
[agents.hermes-work]
base = "hermes"
program = "hermes"
args = ["-p", "work", "chat"]
```

The actual Hermes CLI ordering is verified against the supported version and
captured by adapter tests before this example is added to public user
documentation.

### 9.5 Input framing

Hermes is PTY/TUI-first, so Pohunek writes input as terminal input rather than
as a process argument. Before the adapter constants are finalized, the
implementation records black-box PTY fixtures for:

- a short prompt;
- a multiline prompt;
- text containing terminal control characters;
- a prompt larger than one terminal screen;
- classic and any alternate Hermes TUI mode shipped in the supported release;
- a busy agent receiving queued input;
- an approval prompt.

The resulting adapter explicitly declares:

- whether bracketed paste is required;
- the paste start/end sequences;
- whether submit is carriage return, line feed, or an application key;
- the minimum bounded delay between paste and submit;
- the maximum accepted payload size;
- whether input can be submitted while an approval dialog is active.

If supported Hermes interfaces require different rules, they are separate
validated profiles or detection variants. Pohunek does not guess at runtime
from arbitrary screen text.

Control characters that could escape the paste envelope are rejected or
encoded according to the existing session-input safety contract. The JSON CLI
accepts prompt text through standard input so it never appears in `argv` or the
process list.

### 9.6 Process detection

Hermes gains a compiled detection manifest. It recognizes:

- the native `hermes` executable;
- the supported Python entrypoint form used by the official package;
- a profile wrapper only when its configured executable/arguments match the
  profile definition;
- the same-user process and process-start identity required by existing
  runtime matching.

Process detection alone establishes that the runtime is alive and plausibly
Hermes. It does not manufacture a native session reference.

Screen signatures are bounded fallbacks for activity classification. Fixtures
must cover stable semantic regions rather than branding alone:

- prompt-ready/idle;
- generating or tool-running/working;
- approval-needed/blocked;
- fatal startup/configuration error;
- session-selection UI, if displayed;
- alternate-screen and classic modes.

Authoritative hooks outrank screen heuristics. Stale hook evidence expires and
falls back to the process/screen classifier.

### 9.7 Native identity

The Hermes plugin reports the callback's native `session_id` as the native
reference. It also reports:

- provider `hermes`;
- current process ID;
- process start identity;
- a monotonic report sequence;
- a bounded expiry;
- the current Pohunek `runtime_id`, when inherited in the environment.

The worker-private report path remains the preferred native-identity path. The
public `session.report_native_id` method is a genuine fallback, because M1
extends it to carry the same ordering fields the private path already carries —
runtime identity, PID plus process-start identity, a monotonic sequence, and a
bounded expiry — and the daemon applies to it the same rejection rules it
applies to a private active identity claim. The ordering contract is therefore
uniform across every provider, not Hermes-specific. If both endpoints are
unavailable, identity degrades to process/screen detection and native resume
remains unavailable until a valid report succeeds. The plugin never connects to
a remote host for lifecycle reporting.

`on_session_start` supplies the first identity for a new session.
`pre_llm_call` reasserts the identity on every turn, including a resumed native
session. A missing first hook therefore repairs itself on the next turn.

Hermes context compaction creates a new continuation session with a new native
session ID during the same Pohunek runtime generation. When `pre_llm_call`
reports that continuation, the latest valid sequenced active identity claim
replaces the native reference used by the next `session.resume`. It takes
precedence over the immutable launch identity; the latter remains only a
fallback when no valid continuation claim has been accepted. This prevents a
later resume from returning to the pre-compaction branch.

### 9.8 Activity and lifecycle mapping

Hermes hooks map to Pohunek state as follows:

| Hermes hook | Pohunek evidence |
|---|---|
| `on_session_start` | native identity asserted; no process-exit inference |
| `pre_llm_call` | identity asserted; activity `working` |
| `pre_approval_request` | activity `blocked`; `approval_required` attention |
| `post_approval_response` | activity `working`; matching attention resolved |
| successful `post_llm_call` | activity `idle`; `turn_completed` attention |
| interrupted `on_session_end` | activity `idle`; interruption event |
| failed `on_session_end` | activity `idle`; sanitized error attention |
| completed `on_session_end` | activity `idle`; no process-exit inference |
| `on_session_finalize` | identity lease released; process watcher remains authoritative |

The hook never includes raw user prompts, assistant output, tool arguments,
secrets, or full exception strings in Pohunek notifications. User-facing
messages use fixed templates plus safe session identifiers.

`on_session_end` is deliberately not mapped to runtime exit or logical session
completion. The process watcher and worker lifecycle remain authoritative for
those transitions.

### 9.9 Runtime inventory

Host inspection reports:

- whether the configured Hermes executable resolves;
- version output, redacted and bounded;
- the compiled adapter identifier;
- whether a configured Hermes-based Pohunek profile is valid;
- whether lifecycle integration is installed for a selected local Hermes
  profile, when explicitly diagnosed.

General `host.inspect` does not scan every Hermes profile or read profile
databases. Profile-specific plugin diagnosis is an explicit local integration
command.

## 10. Provider-neutral session observation

Hermes cannot operate a session safely by repeatedly attaching to a raw PTY.
Three public methods are added for all managed agents.

### 10.1 `session.screen`

Request:

```json
{
  "method": "session.screen",
  "params": {
    "session_id": "sess_..."
  }
}
```

Result:

```json
{
  "session_id": "sess_...",
  "worker_id": "worker_...",
  "runtime_id": "runtime_...",
  "runtime_generation": 2,
  "watermark": 1842,
  "dimensions": {
    "cols": 120,
    "rows": 40
  },
  "cursor": {
    "row": 39,
    "col": 2,
    "visible": true
  },
  "alternate_screen": true,
  "title": "Hermes",
  "progress": null,
  "visible_lines": [
    "..."
  ]
}
```

Normative behavior:

- the result is a point-in-time rendered terminal view, not a transcript;
- lines contain plain UTF-8 text with terminal control sequences removed;
- trailing blank cells are trimmed, while row order and meaningful leading
  spacing are preserved;
- the configured maximum screen dimensions and serialized-response limit are
  enforced before writing the control line;
- the response is bound to one runtime generation;
- a stale or unavailable worker returns a typed
  `session_terminal_unavailable` error;
- an observed external session without a Pohunek-owned PTY returns
  `session_has_no_managed_terminal`;
- reading the screen does not acquire attach ownership or affect terminal size.

The daemon obtains the snapshot through a new private worker capability. It is
not added to every inspect response, because terminal content is more sensitive
and potentially much larger than metadata.

The result is a public projection of the existing
`terminal::TerminalSnapshot` and its worker-protocol twin: watermark,
dimensions, cursor position/visibility, alternate-screen state, title,
progress, and visible text are already tracked. This method adds bounded
control-plane access to that state; it does not introduce a second terminal
model.

### 10.2 `session.output`

Request:

```json
{
  "method": "session.output",
  "params": {
    "session_id": "sess_...",
    "after_offset": 8192,
    "max_bytes": 65536,
    "wait_ms": 5000
  }
}
```

Result:

```json
{
  "session_id": "sess_...",
  "runtime_id": "runtime_...",
  "runtime_generation": 2,
  "history_start_offset": 4096,
  "start_offset": 8192,
  "next_offset": 12490,
  "runtime_end_offset": 12490,
  "data_base64": "...",
  "gap": null,
  "has_more": false,
  "timed_out": false
}
```

Normative behavior:

- offsets are monotonic bytes within one runtime generation;
- an omitted `after_offset` returns the newest retained tail bounded by
  `max_bytes`, not the entire history;
- an explicit offset before retained history returns available data plus a
  structured `gap` containing the missing range;
- a runtime-generation mismatch returns `session_runtime_changed` with the
  current runtime identity, rather than silently reusing an offset;
- `max_bytes` is required to be within daemon-configured limits;
- `wait_ms` is optional, bounded by daemon configuration, and waits only when
  the caller is already at the current end;
- the payload is base64 because PTY output is an arbitrary byte stream;
- `has_more` tells the caller to continue immediately from `next_offset`;
- the daemon and worker stream data in frames below the private/public frame
  limits, including replay larger than one frame;
- the method never transfers attach ownership and never writes to the PTY.

The CLI provides both the wire-preserving JSON result and an explicit
human-oriented text projection. The Hermes plugin consumes the binary result,
decodes UTF-8 lossily only for model presentation, strips terminal escapes
with the shared terminal normalizer, and retains all offset/gap metadata.

### 10.3 `session.wait`

Request:

```json
{
  "method": "session.wait",
  "params": {
    "session_id": "sess_...",
    "runtime_id": "runtime_...",
    "after_updated_at": "2026-07-27T12:00:00Z",
    "after_terminal_watermark": 1842,
    "after_output_offset": 12490,
    "states": ["exited", "stopped", "failed"],
    "activities": ["idle", "blocked"],
    "timeout_ms": 8000
  }
}
```

Result:

```json
{
  "reason": "activity_matched",
  "session": {},
  "terminal_watermark": 1851,
  "output_offset": 12720
}
```

Normative behavior:

- `timeout_ms` is required, non-zero, and capped by daemon configuration;
- the method returns immediately if any requested condition is already true;
- otherwise it returns on the first state match, activity match, metadata
  update, terminal-watermark advance, output-offset advance, runtime change, or
  timeout;
- omitted cursors do not create implicit conditions;
- empty `states` or `activities` are rejected rather than treated as
  match-all;
- the result reason is one of:
  `state_matched`, `activity_matched`, `session_updated`,
  `terminal_changed`, `output_advanced`, `runtime_changed`, or `timeout`;
- the returned session is a normal redacted public session snapshot;
- registration is race-free: the daemon snapshots, registers the watcher, and
  rechecks before sleeping;
- the required `timeout_ms` is the only guaranteed waiter termination bound;
- a bounded wait occupies its control connection because the daemon dispatches
  one request at a time per connection, so every client MUST issue
  `session.wait`, and `session.output` with `wait_ms`, on a dedicated
  connection;
- disconnect is not a cancellation guarantee because the sequential dispatch
  loop does not observe EOF while the handler is awaiting the result;
- daemon shutdown cancels with the normal typed transport/shutdown error;
- the method does not hold a registry write lock while waiting.

This method is a bounded long poll, not a new central event service. Existing
subscriptions remain the efficient interface for long-lived GUI and web
clients.

### 10.4 Configuration

Observation limits live in the daemon's established configuration module and
have named, documented platform defaults:

- maximum bytes per `session.output` response;
- maximum `session.output` wait;
- maximum `session.wait` duration;
- maximum serialized terminal rows, columns, and response size;
- maximum concurrent waiters globally and per session on the owner-local
  daemon.

The maximum `session.wait` duration and the maximum `session.output` wait have
short defaults in the 5-10 second range. They are named constants whose
rationale comment records why the ceiling is low: a client killed mid-wait
holds its waiter slot until the timeout expires, because the sequential
dispatch loop cannot observe the disconnect. A short ceiling is what bounds the
resulting availability dip, and callers are expected to re-issue a bounded wait
rather than request a long one.

The `session.output` byte ceiling and `session.screen` serialized-size ceiling
are derived from `MAX_CONTROL_LINE_BYTES`, following the existing
`MAX_SESSION_DIFF_BYTES` precedent. The output derivation accounts for base64's
four-thirds expansion, and both derivations reserve space for the response
envelope and JSON escaping. They are named constants with rationale comments,
not independent literals.

Because every bounded wait occupies one dedicated control connection, the
global and per-session waiter caps also bound concurrently waiting
connections. Clients must not use those connections for subscriptions or
unrelated SDK/GUI traffic.

The public response advertises a stable typed error when a requested value
exceeds a limit. No limit is copied as an unexplained literal across handlers,
the CLI, or tests.

### 10.5 Private worker protocol

The worker protocol gains:

- a `ControlPlaneObservation` capability for one-shot snapshot and output
  reads; the existing `TerminalSnapshot` and `AtomicReplay` capabilities cover
  attach data-stream behavior and are not renamed;
- a request/response operation for the current terminal snapshot;
- a bounded replay/output operation or equivalent stream parameters;
- explicit chunking for data larger than one frame;
- runtime identity on every observation response.

The daemon negotiates the capability. During the existing current-and-previous
private protocol compatibility window, an older worker returns
`worker_feature_unavailable` for screen/output observation while all existing
operations continue to work.

## 11. JSON CLI contract

The Hermes plugin treats the CLI as a versioned process API. Human-readable
output is never parsed.

### 11.1 Global rules

Every command used by the plugin supports `--json` and follows these rules:

- exactly one JSON document is written to standard output;
- logs and diagnostics use standard error;
- success exits with code `0`;
- a typed operational error exits non-zero and emits the standard Pohunek
  error envelope as JSON;
- usage/configuration errors also have stable error codes in JSON mode;
- untrusted strings are accepted through standard input when practical;
- output is bounded by requested limits;
- SIGTERM and timeout cancellation stop the local in-flight SDK request; a
  daemon-side waiter still terminates only by result, required timeout, or
  daemon shutdown;
- the CLI never logs prompt or PTY payloads.

The envelope contains protocol/client version metadata so the plugin can report
`pohunek_cli_incompatible` instead of misparsing an unknown shape.

### 11.2 Required command parity

The Rust CLI gains or standardizes JSON forms for:

- host list and inspection;
- session new;
- session list and inspect;
- session screen;
- session output;
- session wait;
- session input;
- session stop;
- session resume;
- session fork;
- session remove;
- session resize;
- session rename;
- session metadata update;
- session diff;
- runtime inventory.

`session new` and `session input` gain mutually exclusive standard-input forms.
JSON mode rejects ambiguous combinations of positional input, an input option,
and standard input.

### 11.3 Target resolution

Plugin calls pass a structured host target and a full session ID. They do not
depend on fuzzy interactive selectors. User-facing tools may accept a unique
session name, but they first resolve it through session list and fail with
candidate IDs when ambiguous.

Remote calls use the existing direct NetBird resolution in the SDK/CLI. The
plugin never opens SSH and never implements host discovery itself.

## 12. Hermes plugin

### 12.1 Package identity

The installed plugin name is `pohunek`. Its directory contains:

- `plugin.yaml` with `name`, `version`, `description`, `provides_tools`, and
  `provides_hooks`, plus only supported optional manifest fields;
- `__init__.py` with `register(ctx)`, the entrypoint Hermes calls once at
  startup;
- focused modules for tools, hooks, policy, CLI invocation, and result
  redaction;
- read-only `skills/pohunek/SKILL.md`, registered with
  `ctx.register_skill("pohunek", ...)` and exposed by Hermes as the namespaced
  skill `pohunek:pohunek`;
- a Pohunek ownership marker with asset checksum and installation version.

Tools use `ctx.register_tool(...)`; their handlers accept `args: dict` plus
`**kwargs` and return one JSON string. Hooks use
`ctx.register_hook("<hook_name>", callback)`. Hermes discovers the plugin
automatically below the selected `HERMES_HOME/plugins` directory, including at
most one category level, and the installer explicitly enables it with
`hermes plugins enable pohunek`.

The plugin is shipped as embedded, versioned assets in the `pohunek` CLI
release. It does not download code at install time.

### 12.2 Tool design rules

Each tool:

- maps to a fixed allowlisted CLI command;
- builds an argument vector without a shell;
- validates input before starting a process;
- applies a configured timeout;
- sends prompt/input payloads through standard input;
- caps standard output and standard error capture;
- parses one JSON document;
- maps the Pohunek error envelope to a concise Hermes tool error;
- includes stable session/runtime IDs in successful results;
- returns enough cursors for the next observation call;
- never accepts a raw public-protocol method name;
- never accepts arbitrary CLI arguments;
- never returns secrets or unbounded PTY content.

### 12.3 Tool inventory

The plugin registers these tools:

| Tool | Access mode | Purpose |
|---|---|---|
| `pohunek_hosts` | read-only | List local and explicitly reachable configured hosts |
| `pohunek_sessions` | read-only | List/filter sessions |
| `pohunek_session_get` | read-only | Inspect one session |
| `pohunek_session_screen` | read-only | Read the current rendered terminal screen |
| `pohunek_session_output` | read-only | Read bounded incremental output with a cursor |
| `pohunek_session_wait` | read-only | Wait for bounded state/activity/output change |
| `pohunek_session_diff` | read-only | Read the session worktree diff |
| `pohunek_session_start` | manage | Start a session with an allowlisted agent/profile |
| `pohunek_session_send` | manage | Send terminal input through standard input |
| `pohunek_session_resume` | manage | Resume a resumable logical session |
| `pohunek_session_fork` | manage | Fork only when the source adapter supports native fork |
| `pohunek_session_resize` | manage | Resize a managed PTY |
| `pohunek_session_rename` | manage | Rename a logical session |
| `pohunek_session_set_metadata` | manage | Update allowlisted metadata keys |
| `pohunek_session_stop` | full | Stop a live runtime |
| `pohunek_session_remove` | full | Remove an eligible logical session |

Runtime inventory is returned by the host-inspection tool rather than exposed
as a separate model tool unless Hermes tool-schema limits make that result too
large.

### 12.4 Common tool parameters

All session tools use:

```text
host:
  a configured Pohunek host identifier, defaulting to "local" only when
  "local" is in the installed allowlist

session:
  a full session ID, or an exact unique name resolved before mutation
```

All mutating tools include an idempotency key generated by the plugin and
retained across one retry. If the current public method is not idempotent, the
protocol gains request deduplication before the plugin retries it.

No tool exposes `--yes`, `force`, a raw executable, a socket path, a NetBird
address, a hook endpoint, or an arbitrary environment map to the model.

### 12.5 Read results

Read results are concise structured objects, not prose. Session screen and
output results include:

- logical session ID;
- runtime ID and generation;
- state and activity;
- terminal watermark or output cursor;
- gap/truncation indicators;
- bounded plain-text presentation;
- a suggested next cursor where relevant.

The raw base64 stream is not sent to the model unless invalid UTF-8 cannot be
represented safely and the caller explicitly requests the binary diagnostic
form. The normal tool schema exposes plain text plus encoding-loss metadata.

### 12.6 Safe control loop

The bundled skill teaches this default loop:

1. inspect the target;
2. read the current screen;
3. send one bounded input;
4. wait for activity, terminal, output, or terminal state;
5. re-issue the bounded wait when it returns `timeout` and the work is still
   expected to progress;
6. read the screen/output using returned cursors;
7. repeat with a bounded attempt budget;
8. stop and report a blocked/error state rather than looping indefinitely.

The maximum wait is deliberately short (section 10.4), so re-issuing is the
contract rather than a limitation to work around by asking for a longer wait.
The plugin enforces maximum timeout and result limits even when the model asks
for larger values.

### 12.7 No raw attach tool

`session.attach` remains a human terminal operation. An LLM does not receive
the bidirectional raw stream because:

- attach ownership and terminal resizing are interactive concepts;
- feeding an LLM's own rendered output back into its context can create an
  amplification loop;
- raw control sequences are unsafe and token-inefficient;
- bounded screen/output/wait methods provide the required control semantics.

## 13. Plugin delegated-capability policy

This policy configures and audits the tools delegated through the Pohunek
plugin. It is a guardrail against accidental or model-error destruction, not an
authorization sandbox for a Hermes process that runs as the same OS user and
retains shell or file-write tools. Section 21.2 states the resulting trust
boundary explicitly.

### 13.1 Explicit access mode

Plugin installation requires one explicit access mode:

| Mode | Allowed |
|---|---|
| `read_only` | all read tools |
| `manage` | read tools plus start, send, resume, supported fork, resize, rename, and metadata |
| `full` | all tools, including stop and remove |

There is no implicit full-access default. In interactive installation the CLI
explains the modes and requires confirmation. In JSON/non-interactive
installation, omitting the mode is an error.

`pohunek_session_remove` still obeys the daemon's normal state and cleanup
preconditions. `full` does not bypass worktree or live-session protections.

### 13.2 Host allowlist

Installation also requires an explicit host allowlist:

- `local` means the local Unix-socket daemon;
- named remote hosts use the existing Pohunek/NetBird resolver;
- `*` is accepted only with a separate explicit confirmation and is stored
  literally;
- IP addresses and arbitrary socket paths are not accepted as tool input;
- a host outside the list is rejected before the CLI is invoked.

This is particularly important when Hermes receives messages through a
gateway. Hermes channel pairing authenticates who may talk to Hermes; the
Pohunek allowlist constrains what that Hermes instance may operate.

### 13.3 Self-target protection

When running inside a managed Pohunek session, the plugin derives its origin
from trusted `POHUNEK_*` environment values.

For every caller running inside a managed session:

- reading the origin session is allowed;
- starting a child/peer session is allowed;
- mutating, stopping, removing, resuming, forking from, resizing, renaming, or
  changing metadata on the origin session is rejected;
- no argument accepted by the delegated plugin tool can bypass the rejection.

The daemon is the authoritative enforcement point. It receives the caller's
origin identity from `POHUNEK_SESSION_ID` and `POHUNEK_DAEMON_ID` through the
same mechanism used by the self-feeding attach guard, and rejects prohibited
origin-session mutations regardless of which Pohunek client surface issued
them. The plugin repeats the check before subprocess launch as defence in
depth.

The Pohunek-owned plugin policy cannot disable this daemon guard. A human CLI
outside that session has no managed-session origin marker and retains the
normal ability to mutate, stop, or remove it.

### 13.4 Agent/profile allowlist

`pohunek_session_start` accepts only compiled agents and Pohunek agent profiles
returned by runtime inventory. That bound is compiled, not
operator-configurable: the policy carries no agent list, because the daemon
already validates every requested agent and runtime inventory already bounds
the set. The tool never accepts a raw executable, wrapper command, or
environment map from the model.

### 13.5 Metadata schema

The metadata tool exposes named public fields from a fixed compiled schema. The
schema is not operator-configurable; there is no policy list of permitted keys.
The tool does not accept an arbitrary serialized metadata object. Provider
tokens, environment values, hook endpoints, socket paths, and private worker
identifiers are never writeable through it.

### 13.6 Logging and redaction

Plugin and CLI logs include:

- operation name;
- duration;
- host identifier;
- success/error code;
- redacted logical session ID where useful;
- byte counts, not payloads.

They exclude:

- prompts and terminal input;
- terminal output and screen contents;
- model responses;
- tool arguments that may contain code;
- environment variables;
- Hermes or provider tokens;
- raw exception text from subprocesses.

Captured stderr is bounded and passed through the existing redaction policy
before appearing in a tool error.

## 14. Plugin lifecycle hooks

### 14.1 Activation

Lifecycle reporting activates only when both are true:

- the Pohunek plugin is enabled in the current Hermes profile;
- the process inherited the Pohunek managed-session environment marker.

An ordinary Hermes process outside Pohunek does not report to a daemon merely
because the plugin is installed.

### 14.2 Reporting paths

At plugin initialization, immutable reporting configuration is read once and
validated:

- managed-session marker;
- session/runtime identity;
- worker-private hook endpoint and capability;
- local daemon endpoint for activity, attention, and fallback identity;
- configured socket deadline.

Per-hook execution only constructs a small fixed-shape message and attempts a
local socket write.

Native identity prefers the ordered worker-private path and falls back to the
local public `session.report_native_id` method, which carries the same ordering
fields (section 9.7). Activity and attention use the public local daemon report
methods because they affect the logical registry and notification policy. Both
paths are local only. If an endpoint is unavailable, its report is dropped; the
worker's terminal and process evidence continue to function.

### 14.3 Ordering and expiry

Reports carry:

- runtime ID;
- process ID and start identity;
- monotonically increasing sequence;
- observation timestamp;
- bounded lease expiry where the protocol requires it.

The daemon rejects:

- a report for an old runtime generation;
- a PID whose start identity does not match;
- a lower/equal sequence after a newer report;
- a provider mismatch;
- an expired report;
- a report attempting to target another logical session.

### 14.4 Failure behavior

Hook failure:

- never aborts a Hermes request;
- never changes Hermes stdout/stderr;
- never retries synchronously more than the configured local attempt;
- increments only a local bounded diagnostic counter;
- is visible in `pohunek integration doctor --agent hermes`;
- falls back to normal process/screen detection.

## 15. Installation and profile management

### 15.1 Command surface

Canonical installation:

```text
pohunek integration install --agent hermes \
  --hermes-profile default \
  --access-mode manage \
  --allow-host local
```

Custom relocated home:

```text
pohunek integration install --agent hermes \
  --hermes-home /absolute/owner/private/hermes-home \
  --access-mode read_only \
  --allow-host local
```

`--hermes-profile` and `--hermes-home` are mutually exclusive. JSON and
non-interactive mode require the target, access mode, and host allowlist
explicitly.

Related commands:

```text
pohunek integration status --agent hermes --hermes-profile <name> --json
pohunek integration doctor --agent hermes --hermes-profile <name> --json
pohunek integration update --agent hermes --hermes-profile <name>
pohunek integration uninstall --agent hermes --hermes-profile <name>
```

For this delivery, `status`, `doctor`, `update`, and `uninstall` are
Hermes-only; using those verbs with `--agent codex` or `--agent claude` returns
a typed unsupported-action error. This asymmetry is accepted pre-1.0 debt:
Codex and Claude currently expose only `integration install`, while the Hermes
profile-owned plugin needs lifecycle management to preserve user files safely.
A later provider-neutral integration-management RFC may unify the verbs.

Installation is local-only. To install on a remote machine, the operator runs
the command on that machine. The command does not write a remote home directory
through the daemon protocol.

### 15.2 Why installation is CLI-side

The active `HERMES_HOME`, named profile, Hermes executable, and profile CLI
semantics belong to the user's shell/client surface. Architecture also keeps
provider shell-outs out of the daemon.

Hermes plugin materialization and `hermes plugins enable pohunek` therefore run
in the Rust CLI process. This matches the existing command semantics:
`integration install` is already deliberately local-only regardless of
`--host`. Existing daemon-side Codex/Claude integration behavior can remain
until separately refactored.

The CLI uses an internal typed installer module so command parsing,
filesystem validation, asset rendering, Hermes invocation, diagnosis, and
uninstall are testable without a live daemon.

### 15.3 Target resolution

The installer:

1. resolves the requested profile/home without reading `state.db`;
2. canonicalizes the existing parent;
3. rejects a relative path;
4. rejects symlink escape from the selected Hermes home;
5. verifies the directory is owned by the current user;
6. rejects group/world-writable unsafe parents;
7. creates only the required owner-private plugin directories;
8. refuses to operate on a root/home/workspace-wide target;
9. never prints the full environment or configuration.

A named profile is resolved through supported Hermes CLI/profile conventions,
not by guessing when the installed Hermes version reports a different layout.

### 15.4 Atomic materialization

The managed plugin is written to a sibling temporary directory with owner-only
permissions, validated, fsynced where the platform abstraction supports it,
and atomically renamed into place.

If `plugins/pohunek` exists:

- an intact Pohunek ownership marker permits idempotent update;
- an unknown directory or missing marker causes
  `hermes_plugin_name_collision`;
- user-authored files inside an unmanaged directory are never overwritten;
- a modified managed asset causes diagnosis to fail and update to require
  explicit confirmation before replacement.

The marker contains no secrets. It records:

- installer format version;
- Pohunek version;
- plugin asset version;
- checksums for immutable plugin assets only;
- selected policy shape version.

The mutable policy file is outside both the plugin directory and this
checksummed asset set.

### 15.5 Enabling and configuration

The CLI invokes the Hermes executable directly with an argument vector to
enable the plugin in the selected profile. It does not edit YAML by ad-hoc text
replacement. Hermes has no per-plugin configuration mechanism; the policy is a
Pohunek-owned artifact.

The policy is stored outside the Hermes plugin directory in the Pohunek state
directory, keyed by the canonical resolved Hermes home. An installed,
checksummed plugin asset records the resolved policy path. The policy itself is
owner-private, mutable without replacing plugin assets, and contains:

- access mode;
- allowed hosts;
- operation/output/wait limits within compiled maximums;
- configuration schema version.

Those are the only operator-configurable fields. Permitted agents come from
runtime inventory (section 13.4) and the metadata surface is a fixed compiled
schema (section 13.5); neither is expressible in the policy.

No secrets are stored there. Required policy fields have no silent defaults;
safe implementation-level maximums remain named constants in the plugin.

### 15.6 Upgrade compatibility

The Pohunek-owned compatibility metadata and policy declare the supported
Pohunek CLI protocol range; `plugin.yaml` is limited to Hermes-supported
fields. Before registering mutating tools, plugin initialization checks:

- the `pohunek` executable resolves from the installed absolute path or a
  validated configured path;
- `pohunek version --json` is compatible;
- the policy file is valid;
- the selected tool set matches the access mode.

On incompatibility, lifecycle hooks remain best effort, read/mutation tools are
not registered, and Hermes exposes a concise diagnostic directing the operator
to `pohunek integration doctor`. The plugin does not optimistically call an
unknown CLI.

### 15.7 Uninstall

Uninstall:

- disables the plugin through the Hermes CLI;
- removes only files listed by the valid ownership marker;
- removes the managed policy file;
- leaves Hermes sessions, `state.db`, other skills/plugins, and unrelated
  configuration untouched;
- reports modified managed files and requires explicit confirmation before
  deleting them;
- reports whether removal is recoverable from the installer backup, if a
  backup was created.

## 16. Bundled Pohunek skill

### 16.1 Purpose

Tool descriptions explain individual calls. The bundled `pohunek` skill
teaches the operating model and safe multi-step workflows:

- hosts, logical sessions, runtimes, workers, projects, and worktrees;
- durable daemon/worker ownership;
- managed versus observed sessions;
- agent profiles;
- start, observe, send, wait, stop, resume, fork, and remove semantics;
- terminal screen versus output history;
- cursor/gap/runtime-generation handling;
- typed error recovery;
- access-mode, host, and self-target restrictions;
- secret and terminal-content handling;
- when to ask a human to attach.

### 16.2 Source of truth

The skill is generated from the hand-authored `docs/knowledge/` source through
`cargo xtask docs`. Hermes-specific frontmatter and tool requirements are
rendered by the generator. The generated plugin asset is checked into the
release asset location expected by the installer.

`docs/knowledge/assistant/source-map.md` maps:

- every Hermes runtime behavior;
- every new public protocol method;
- every tool;
- installation and security policy;
- the generated skill asset.

`cargo xtask docs check` fails on stale generated content, missing source-map
entries, forbidden secrets, or a tool/API mismatch.

### 16.3 Skill registration

The plugin registers the bundled skill only when:

- the compatible Pohunek CLI is present;
- the policy file is valid;
- at least the read-only tool set registered successfully.

The skill metadata declares the required Pohunek tools so Hermes does not load
instructions for unavailable capabilities. The body explains that destructive
tools may be absent by policy and must not be emulated through a shell command.

## 17. Notifications

The public notification policy replaces fixed `codex` and `claude` fields with
a provider-keyed map:

```json
{
  "providers": {
    "codex": {},
    "claude": {},
    "hermes": {}
  }
}
```

This is a deliberate pre-1.0 schema change, not a compatibility shim. It avoids
another wire-shape change for every future provider.

Rules:

- missing provider entry uses the documented base policy;
- unknown provider keys are preserved or rejected according to the protocol's
  existing strictness decision, consistently in Rust and TypeScript;
- provider-specific notification kinds remain typed;
- Hermes hook reports use sanitized fixed templates;
- notification dispatch does not call Hermes or any provider executable;
- GUI/web policy editors render provider rows from runtime inventory plus known
  configured keys.

The migration and protocol-version impact are covered in section 20.

## 18. GUI and web behavior

Hermes is shown anywhere built-in agents are shown:

- new-session agent picker;
- runtime inventory;
- list filters;
- session details;
- activity and attention labels;
- resume/fork action availability;
- integration status/doctor results;
- notification policy;
- fixture and demo data.

UI behavior is capability-driven:

- resume appears only when the session has a native reference and its adapter
  supports resume;
- fork is disabled for Hermes with the typed unsupported reason;
- screen/output views use the provider-neutral APIs;
- no UI infers support from the string `"hermes"`;
- unknown future agents receive a neutral label/icon rather than crashing an
  exhaustive client switch; from M1 this is backed by the protocol's neutral
  `AgentKind` variant (section 20.1) rather than by client discipline alone.

The Hermes logo or third-party trademark asset is not copied without a
license-compatible source. A neutral terminal/agent glyph is sufficient.

## 19. Error model

New stable public or CLI errors include at least:

- `agent_fork_unsupported`;
- `session_terminal_unavailable`;
- `session_has_no_managed_terminal`;
- `session_runtime_changed`;
- `session_output_limit_exceeded`;
- `session_wait_limit_exceeded`;
- `session_waiter_limit_reached`;
- `worker_feature_unavailable`;
- `hermes_not_installed`;
- `hermes_version_unsupported`;
- `hermes_profile_required`;
- `hermes_profile_not_found`;
- `hermes_home_unsafe`;
- `hermes_plugin_name_collision`;
- `hermes_plugin_modified`;
- `hermes_plugin_not_enabled`;
- `hermes_plugin_incompatible`;
- `pohunek_cli_incompatible`;
- `plugin_access_denied`;
- `plugin_host_denied`;
- `plugin_self_target_denied`;
- `plugin_agent_denied`.

Errors have typed fields for recovery, such as current runtime identity,
supported capability, configured maximum, allowed host identifiers, or the
diagnostic command. They do not include tokens, prompts, terminal payloads,
unredacted subprocess output, or private hook paths.

## 20. Protocol and persistence compatibility

### 20.1 Public protocol

Delivery uses exactly one public protocol transition, in M1. It is the last
fleet-wide break this integration causes.

M1 carries every change that cannot be additive:

- the minimum/maximum negotiation envelope;
- `session.screen`, `session.output`, and `session.wait` with their result
  types, capability fields, and typed errors;
- the provider-keyed notification policy;
- forward-compatible `AgentKind` deserialization, where an unknown wire value
  becomes a neutral variant instead of failing.

M2 and M3 are then purely additive and perform no public bump.
`AgentKind::Hermes` is a new wire value that older M1 peers already tolerate as
the neutral variant (section 9.1), and the plugin surface adds no wire shape at
all.

The reason for concentrating the break is the cost of the current negotiation,
which requires exact version equality. A daemon bump hard-fails every client
that has not been upgraded, including the Rust CLI, native GUI, web
backend/SDK, plugin CLI calls, and clients reaching remote hosts over NetBird.
Likewise, an upgraded client cannot talk to a non-upgraded remote daemon.
Because remote hosts must be visited to be upgraded, each such boundary is an
operational event across the whole fleet, not a Hermes-only feature gate. One
boundary is therefore materially cheaper than two.

The envelope introduced in M1 is what prevents a repeat. M1 cannot interoperate
with the legacy exact-version envelope, but from M1 onward peers negotiate the
highest overlapping version, so a future provider or additive method never
forces another lockstep upgrade. Tests cover overlap, no overlap, legacy
rejection, neutral-variant round-tripping, rejection of the neutral variant on
mutating paths, and diagnostics.

All version match arms, range-negotiation fixtures, client compatibility tests,
generated TypeScript types, web fixtures, and public API documentation are
updated in the same change.

Because Pohunek is pre-1.0, no old notification-policy shape is preserved.
Upgrade notes state that every client and every local or remote daemon must
cross the single M1 range-negotiation boundary in coordinated order, and that
no coordinated upgrade is required for M2 or M3.

### 20.2 Private worker protocol

The private worker protocol version is bumped for terminal-read/output
capabilities. The existing current-and-previous compatibility contract remains:

- current daemon + previous worker can perform existing lifecycle operations;
- current daemon reports observation as unavailable on the previous worker;
- current worker accepts the previous daemon without emitting unsupported
  unsolicited shapes;
- rolling daemon restarts do not kill healthy previous-version workers merely
  because screen reading is unavailable.

### 20.3 On-disk session data

Existing session records continue to decode because their enum values remain
valid. New Hermes records require the new binary. Downgrading a host after it
has persisted Hermes sessions is unsupported and is documented.

No code parses or migrates Hermes's `state.db`.

### 20.4 Plugin policy data

The managed plugin policy has its own schema version. An unsupported policy
version disables tool registration and produces a doctor error; it is never
silently interpreted with guessed defaults.

## 21. Security model

### 21.1 Trust boundary

Pohunek remains single-operator software. Local operations rely on owner-only
socket/file permissions. Remote operations use direct owner-controlled
NetBird/WireGuard connectivity. The Hermes plugin does not create a new
listener or central service.

### 21.2 Prompt injection and delegated authority

Hermes may receive untrusted repository content, terminal output, or gateway
messages. The plugin policy constrains only the delegated Pohunek tool surface.
It is not a sandbox and does not constrain Hermes when that same process has a
shell or file-write capability as the same OS user. Such a Hermes can rewrite
the Pohunek-owned policy or bypass the plugin by invoking commands such as
`pohunek session rm` directly.

The policy is still useful: it prevents accidental or model-error destruction,
makes delegated capability explicit, and produces an auditable configuration.
Within that scope:

- plugin installation is explicit;
- host scope is explicit;
- mutation scope is explicit;
- destructive operations require `full`;
- the daemon authoritatively restricts origin-session mutation, with a
  plugin-side defence-in-depth check;
- tool arguments cannot select arbitrary commands or endpoints;
- tool output is data, not executable instructions;
- the skill tells Hermes to treat terminal/repository text as untrusted;
- ordinary Pohunek daemon safety checks remain authoritative.

Protecting against a hostile or prompt-injected agent with same-user shell or
file-write access requires an external sandbox or a more restrictive Hermes
execution environment and is outside this integration.

### 21.3 Filesystem safety

Install/update/uninstall never:

- follow an unresolved symlink out of the selected Hermes home;
- recursively target a broad directory;
- overwrite an unowned plugin;
- write group/world-readable policy;
- read `.env`, key, certificate, token, or Hermes session database files;
- include user home paths in remote tool results.

### 21.4 Process safety

Subprocess execution:

- uses a fixed executable and fixed subcommand allowlist;
- uses no shell;
- has bounded time, stdout, and stderr;
- closes inherited file descriptors except standard streams;
- supplies a minimal environment required for profile and Pohunek target
  resolution;
- does not inherit a model-controlled `PATH`, `HERMES_HOME`, or socket path;
- cancels child processes on plugin cancellation.

### 21.5 Data minimization

Screen and output are potentially sensitive. They are:

- returned only on explicit tool/API calls;
- bounded;
- never added to generic inspect/list events;
- never logged;
- never included in notification text;
- subject to the same local/NetBird trust boundary as attach;
- normalized before entering model context.

## 22. Observability

Daemon structured logs add safe events for:

- Hermes adapter selection and executable probe;
- hook report accepted/rejected by reason;
- screen/output/wait request duration and byte/row counts;
- waiter timeout, daemon-shutdown cancellation, and limit rejection;
- plugin integration status checks when requested.

CLI/plugin structured diagnostics add:

- tool/command name;
- host;
- duration;
- exit status and typed error code;
- request/response byte counts.

Performance metrics cover:

- hook reporting latency and timeout count;
- terminal snapshot latency;
- output wait duration;
- concurrent waiter count;
- plugin CLI invocation latency;
- Hermes detection fallback rate.

Payloads and secrets are excluded as specified in section 13.6.

## 23. Performance and resource bounds

- Hooks have a short local deadline and allocate only small fixed-shape
  messages.
- `session.screen` performs one bounded snapshot and serialization.
- `session.output` returns at most the configured byte limit and chunks private
  frames.
- `session.wait` uses notification/watch primitives, not a polling loop or one
  thread per waiter.
- Every `session.wait` and waiting `session.output` occupies its dedicated
  sequential control connection until data, timeout, or daemon shutdown.
- Waiters are capped globally and per session, which also caps concurrently
  occupied waiting connections.
- The short maximum wait (section 10.4) bounds how long a waiter abandoned by a
  killed client can occupy a slot, because disconnect itself is not observable.
- Plugin subprocess concurrency is capped per Hermes process.
- Tool results are capped before they enter Hermes context.
- The default 10 MB (`10_000_000` bytes) worker history cap remains a
  configurable retention limit, not a permitted single response size; the
  configured maximum is 256 MiB.

## 24. Failure and recovery behavior

### 24.1 Hermes executable missing

Runtime inventory reports Hermes unavailable. Starting a Hermes session fails
before creating a durable live runtime. Existing non-Hermes sessions are
unaffected.

### 24.2 Hook/plugin missing

The managed Hermes runtime remains valid. Process/screen detection supplies
bounded fallback activity, native resume remains unavailable until a native
reference is reported, and doctor explains how to install/enable the plugin.

### 24.3 Daemon restart

The worker retains the PTY, process, terminal model, output history, and native
identity lease. After reconciliation, screen/output APIs and hook reports bind
to the same runtime.

### 24.4 Worker restart or loss

The normal durable-session reconciliation rules apply. Output cursors tied to
the old runtime return `session_runtime_changed` or terminal unavailable; they
are never silently applied to a new runtime.

### 24.5 CLI/plugin version mismatch

Tools are not registered. Lifecycle reporting remains best effort if its
private protocol is compatible. Doctor reports installed and required
versions.

### 24.6 Remote host unavailable

The tool returns the existing typed connection/host error. It does not fall
back to SSH, another host, or local operation.

### 24.7 Output gap

The output result identifies the missing offset range and starts at retained
history. The skill instructs Hermes to read the current screen and continue
from `next_offset`, not to assume unseen output.

### 24.8 Native reference absent

Resume returns the existing typed missing-reference error with instructions to
run/inspect the session with lifecycle integration. Pohunek never substitutes
Hermes `--continue`.

### 24.9 Hermes hook API changes

Plugin registration fails closed, tools remain disabled if initialization is
incomplete, and a pinned real-Hermes compatibility test exposes the upstream
change before release.

### 24.10 Native reference replaced by a continuation session

Hermes context compaction may replace the active native session ID without
replacing the Pohunek runtime. The next `pre_llm_call` reports the continuation
ID as a higher-sequence active identity claim. Once the worker and daemon
validate its runtime, PID start identity, sequence, and lease, that reference
supersedes the immutable launch identity for future resume.

If the report is missed, the last validated reference remains visible but may
point to the pre-compaction branch. The next successful `pre_llm_call`
reassertion repairs it. Pohunek never guesses the continuation from
`state.db`, screen text, or the public last-write-wins identity method.

## 25. End-to-end user flows

### 25.1 Launch and resume Hermes

```text
pohunek session new --agent hermes --cwd /work/project
# Hermes plugin reports native ID h_abc123.
pohunek session inspect sess_...
pohunek session resume sess_...
```

The resumed process is launched as:

```text
hermes chat --resume h_abc123
```

The logical session ID is unchanged. Runtime ID and generation advance.

### 25.2 Install operator tools

```text
pohunek integration install --agent hermes \
  --hermes-profile work \
  --access-mode manage \
  --allow-host local \
  --allow-host desktop

pohunek integration doctor --agent hermes \
  --hermes-profile work --json
```

The work profile loads the version-matched plugin and skill. Other Hermes
profiles remain unchanged.

### 25.3 Hermes operates a peer session

Hermes:

1. calls `pohunek_sessions` on `desktop`;
2. calls `pohunek_session_get`;
3. calls `pohunek_session_screen`;
4. calls `pohunek_session_send` with input through stdin;
5. calls `pohunek_session_wait` with the returned watermark/output cursor;
6. reads the changed screen;
7. reports completion or a typed blocked state.

No raw attach is acquired.

### 25.4 Destructive operation denied

A plugin installed with `manage` calls `pohunek_session_remove`. The plugin
rejects it locally with `plugin_access_denied`; it does not invoke the CLI.

### 25.5 Origin-session mutation denied

Hermes running inside `sess_origin` attempts to send input to `sess_origin`.
The plugin returns `plugin_self_target_denied`. Reading its screen for
diagnostics remains allowed.

## 26. Alternatives considered

### 26.1 Hermes only as another `AgentKind`

Rejected. Launch/resume alone would not let Hermes operate sessions and would
leave activity/native identity weaker than the available plugin API permits.

### 26.2 Hermes plugin directly implements NDJSON and NetBird

Rejected. It would duplicate version negotiation, target resolution, transport
security, typed errors, cancellation, and future protocol changes in Python.

### 26.3 MCP as the primary integration

Rejected for this milestone. MCP can expose tools but cannot replace in-process
Hermes lifecycle hooks and bundled plugin skill registration without an
additional integration package and process.

### 26.4 Parse Hermes `state.db`

Rejected. The database is a private profile-scoped implementation detail and
may contain sensitive conversation data. Hooks provide the required native
identity.

### 26.5 Use raw attach as a model tool

Rejected due to ownership, feedback-loop, control-sequence, and unbounded
context risks.

### 26.6 Add a `hermes` notification field beside `codex` and `claude`

Rejected. A provider-keyed map is the correct pre-1.0 contract and prevents a
wire-shape change for every new provider.

### 26.7 Treat Hermes resume as fork

Rejected. Resume and fork have different logical semantics. Pohunek reports the
unsupported operation honestly.

### 26.8 Install globally into all profiles

Rejected. Hermes profiles are intentionally isolated and may have different
trust, gateway, and tool policies.

## 27. Documentation changes required with implementation

At minimum, implementation updates:

- `README.md`;
- `docs/architecture.md`;
- `docs/public-api.md`;
- the relevant roadmap/phase document if the milestone changes;
- CLI, session, runtime, integration, safety, and troubleshooting knowledge
  sources under `docs/knowledge/`;
- `docs/knowledge/assistant/source-map.md`;
- generated Hermes skill assets;
- configuration examples;
- release/upgrade notes for protocol and notification-policy changes;
- `AGENTS.md` if crate boundaries or build commands change.

No behavior described by the assistant bundle may land stale.

## 28. Acceptance summary

The complete RFC is the union of three independently releasable milestones:
provider-neutral observation/CLI foundations, first-class Hermes runtime and
client parity, and the Hermes plugin/operator capability. Each milestone must
meet its own Definition of Done in the implementation plan; an enum, adapter,
plugin scaffold, or tool-schema-only slice is never releasable. Full RFC
completion requires the runtime, observation protocol, CLI parity, real plugin,
delegated-capability policy, daemon origin guard, skill, UI/web ripples,
compatibility handling, documentation, and tests described here.

## 29. References

Primary Hermes documentation:

- [Plugins](https://hermes-agent.nousresearch.com/docs/user-guide/features/plugins)
- [Hooks](https://hermes-agent.nousresearch.com/docs/user-guide/features/hooks)
- [Sessions](https://hermes-agent.nousresearch.com/docs/user-guide/sessions)
- [CLI commands](https://hermes-agent.nousresearch.com/docs/reference/cli-commands)
- [Profiles](https://hermes-agent.nousresearch.com/docs/user-guide/profiles)
- [Skills](https://hermes-agent.nousresearch.com/docs/user-guide/features/skills)
- [MCP](https://hermes-agent.nousresearch.com/docs/user-guide/features/mcp)
- [Hermes Agent repository](https://github.com/NousResearch/hermes-agent)
- [Upstream hook latency issue](https://github.com/NousResearch/hermes-agent/issues/10048)

Authoritative Pohunek context:

- [`../architecture.md`](../architecture.md)
- [`../public-api.md`](../public-api.md)
- [`universal-assistant.md`](universal-assistant.md)
- [`agent-lifecycle-detection-plan-2026-07-05.md`](agent-lifecycle-detection-plan-2026-07-05.md)
- [`durable-session-workers-rfc.md`](durable-session-workers-rfc.md)

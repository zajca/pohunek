# NEXT STEP — Milestone 10: `--json` everywhere + error/recovery-hint polish

This file describes, in detail, the immediate next step. It is a handoff for
whoever picks up the work (you, a subagent, or a fresh session).

## Goal of milestone 10 (closes Phase 1)

Two polish steps that make the CLI safe to automate and pleasant to debug. No new
runtime capabilities, no new wire types — this is the last Phase 1 milestone
before the Phase 2 remote transport.

1. **`--json` everywhere.** Stable, machine-readable output on every read and
   automation command, not just the few that have it today. Under `--json` the
   command prints exactly one JSON document to stdout (nothing human on stdout)
   and a non-zero exit on failure carries a structured error.
2. **Error / recovery-hint polish.** Clear typed diagnostics for the two failure
   modes a user actually hits at this stage — a **missing agent binary** and a
   **protocol version mismatch** — and the human CLI must surface the
   `ProtocolError.recover` hint that today it silently drops.

### Definition of done (testable)

1. **`--json` coverage is complete.** Every read/automation command accepts
   `--json` and emits valid JSON that round-trips through `serde`:
   - already done: `doctor`, `health`, `status`, `session list`, `session inspect`
     (`crates/cli/src/main.rs` — each has `#[arg(long)] json: bool`);
   - add it to: `session new`, `session stop`, `session input`, and
     `integration install` (today these only render human text via
     `render_new_human` / `render_install_human` and the stop/input result
     structs).
   *Check:* for each command, `--json` output parses as JSON and deserializes
   back into the corresponding protocol result type; `attach` (raw stream) and
   `daemon start` are explicitly excluded.
2. **`--json` is clean and structured on failure.** Under `--json`, stdout
   carries only the JSON document (no human lines leak onto stdout), and a failing
   command prints a structured error object (`{class, code, msg, recover?}`)
   and exits non-zero, so a script can branch on `code`. *Check:* a forced
   failure (e.g. `session inspect missing --json`) prints parseable error JSON
   with the stable `code` and exits non-zero.
3. **Missing agent binary is a clear typed diagnostic.** `session new --agent
   claude` when `claude` is not on `PATH` fails with a typed error that names the
   missing binary, uses a stable code (e.g. `agent_binary_missing`), and carries
   a `recover` hint (e.g. "install the claude CLI and ensure it is on PATH; see
   `zagentmesh doctor`"). Today a spawn failure maps to the generic
   `spawn_failed` with the raw OS error and no hint
   (`session/mod.rs::pty_error_to_protocol`). *Check:* with `claude` absent,
   `session new --agent claude` returns `agent_binary_missing` and the CLI prints
   the recovery hint; `doctor` still flags the same gap.
4. **Version mismatch is surfaced clearly.** The daemon already enforces version
   negotiation at dispatch (`crates/daemon/src/api/handler.rs:109,133` call
   `negotiate(request.v, PROTOCOL_VERSION)`), returning the typed
   `daemon/version_mismatch` error (both versions named, recover hint set —
   `crates/protocol/src/error.rs::version_mismatch`). The remaining work is
   CLI-side: render that error and its hint clearly (see DoD #5). *Check:* a unit
   test asserting the rendered message names both versions and the upgrade hint.
5. **The human CLI renders `recover` hints.** `ProtocolError`'s `Display` is
   `"{class}/{code}: {msg}"` and omits `recover`
   (`crates/protocol/src/error.rs`), so the CLI's
   `eprintln!("zagentmesh: {err}")` (`crates/cli/src/main.rs`) drops every
   recovery hint today. The CLI must print the `recover` line when present (for
   both human and `--json` paths). *Check:* a `version_mismatch` and an
   `agent_binary_missing` surfaced through the CLI both show their hint.
6. `cargo build`, `cargo clippy --all-targets --workspace -- -D warnings`, and
   `cargo test --workspace` stay clean.

### Explicitly OUT of scope (do NOT build here)

- **Remote transport / NetBird discovery** → **Phase 2** (the next step; the
  product's unique value). `--host` already parses but execution stays local
  (`ensure_local_host` in `crates/cli/src/main.rs`).
- **SQLite `state.db` + `user_version` migrations** → deferred backlog (see the
  M9 decision, preserved in `docs/plan-phase-1.md` "Deferred: SQLite Schema").
- **New session lifecycle features** (worktree cleanup-on-stop, merge flows,
  etc.). Worktree bindings intentionally outlive a stopped session today; do not
  change that here.

---

## Where we are now (done, verified)

Milestones 1–9 are complete. `cargo build`, `cargo clippy --all-targets
--workspace -- -D warnings`, and `cargo test --workspace` = **291 passed** (the
integration `health_socket` suite has one pre-existing parallel-socket flake,
`stale_socket_is_recovered_on_bind`, that passes single-threaded — unrelated to
this work).

- `crates/protocol` — typed control envelopes; full session lifecycle + attach
  types; `AgentKind`; `AgentActivity` + `SessionInfo.activity`; `StateSource`;
  `agent_state`; `session.input`; M7 `session.report_native_id` +
  `integration.install`; M8 worktree fields + `SessionWarning`. Errors are typed
  (`ProtocolError { class, code, msg, recover }`) with canonical constructors
  (`version_mismatch`, `method_not_found`, `bad_request`). Version negotiation:
  `negotiate()` / `PROTOCOL_VERSION` (`version.rs`).
- `crates/daemon` (`zagentmeshd`) — Unix-socket control server; full `session.*`
  lifecycle; attach bridge; `subscribe` event stream; in-memory `SessionRegistry`
  + per-session detection (state engine); agent adapters; session-id hook +
  resume; worktree-per-session; **M9 unified metadata store + append-only event
  log**.
- `crates/cli` (`zagentmesh`) — `doctor`, `daemon start`, `health`/`status`,
  `session new/list/inspect/stop/input`, `attach`, `integration install`. A
  global `--host` arg parses (local-only execution). `--json` exists on `doctor`,
  `health`, `status`, `session list`, `session inspect`.
- **M9 (just landed):**
  - `daemon/src/store/mod.rs` — unified `Store` over one `metadata.jsonl`
    (tagged `Record::{Resume,Worktree}`, one lock, one atomic temp+rename).
  - `daemon/src/events/mod.rs` — append-only `EventLog` under `events/`,
    `spawn_drain` + graceful `shutdown_event_log` flush; never secrets/terminal
    bytes; git stderr is credential-scrubbed before it reaches a `SessionWarning`.

### Seams milestone 10 builds on (already in place)

- `crates/cli/src/main.rs` — the clap `Commands` / `SessionAction` enums (where
  `--json` flags are declared) and `run()` (where each command dispatches). The
  top-level error sink is `eprintln!("zagentmesh: {err}")` in `main()`.
- `crates/cli/src/commands/` — per-command `run` fns and the human renderers
  (`health::run`, `doctor::print_human`, `session::render_new_human`,
  `integration::render_install_human`). Mirror the existing `if json { … } else {
  … }` shape (see `health.rs:35` / `doctor.rs:107`).
- `crates/cli/src/error.rs` — `CliError`; `Protocol(ProtocolError)` is the
  variant whose rendering must grow a `recover` line.
- `crates/protocol/src/error.rs` — `ProtocolError` + `recover` field +
  `version_mismatch`. Add an `agent_binary_missing` constructor here (stable code
  + recover hint) for DoD #3.
- `crates/daemon/src/session/mod.rs` — `pty_error_to_protocol` and the
  `register_pty_session` spawn path: detect a missing-binary spawn failure
  (ENOENT) and map it to the new typed error naming the agent's binary. The agent
  adapters (`daemon/src/agent/`) know each agent's binary name.
- `crates/daemon/src/api/handler.rs:109,133` — version negotiation is already
  enforced; no daemon change needed for DoD #4.

---

## Implementation tasks

1. `crates/protocol/src/error.rs` — add an `agent_binary_missing(binary)`
   constructor (class `runtime`, stable code `agent_binary_missing`, recover
   hint). No wire-shape change (still `{class,code,msg,recover}`).
2. `crates/daemon/src/session/mod.rs` — when the PTY spawn fails because the
   agent binary is absent (ENOENT), return the new typed error naming the
   binary instead of the generic `spawn_failed`.
3. `crates/cli/src/main.rs` + `crates/cli/src/commands/session.rs` +
   `integration.rs` — add `--json` to `session new`, `session stop`,
   `session input`, `integration install`; each prints the result struct as JSON
   under `--json` and the existing human text otherwise.
4. `crates/cli/src/error.rs` + `main.rs` — render `ProtocolError.recover` on a
   separate `hint:` line for human output; under `--json`, emit the structured
   error object and exit non-zero.
5. Keep `attach` and `daemon start` free of `--json` (raw stream / process
   control — JSON is meaningless there); document the exclusion.

---

## Tests (must pass before done)

- CLI unit tests: each `--json` command's output parses and deserializes into its
  protocol result type; a failing `--json` command emits a structured error
  object with the expected stable `code` and exits non-zero.
- protocol: `agent_binary_missing` carries the binary name + a recover hint;
  `version_mismatch` names both versions (already) + hint.
- daemon integration (`crates/daemon/tests/health_socket.rs`): `session new` for
  an agent whose binary is absent from `PATH` returns `agent_binary_missing`
  (reuse the `PathGuard` machinery already in the suite).
- CLI rendering: a `ProtocolError` with a `recover` hint renders the `hint:` line
  (covers both `version_mismatch` and `agent_binary_missing`).
- Keep `cargo build`, `cargo clippy --all-targets --workspace -- -D warnings`,
  and `cargo test --workspace` clean.

---

## After this milestone

Phase 1 is complete. **Phase 2 = remote transport over NetBird** — the product's
unique value, and the reason SQLite was deprioritized: a TCP listener bound only
to the NetBird interface, tokenless discovery (`netbird status --json`), `host
discover`/`list`/`inspect`, and remote session lifecycle, all reusing the Phase 1
control + attach protocol unchanged. Multi-host display is client-side fan-out of
live per-host queries — no shared/replicated state. See
[`docs/phases/02-remote-netbird.md`](docs/phases/02-remote-netbird.md).

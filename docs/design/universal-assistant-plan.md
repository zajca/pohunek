# Implementation Plan: Universal Pohunek Assistant

Status: **mostly implemented** (verified against the code on 2026-06-26).
**P0–P7 and P9 are implemented** in the workspace (full P2 generated-reference
pipeline + CI drift gates, P4 snapshot/redaction, P5 selection/preflight/bootstrap,
P6 prompt composition + `--print-prompt`, P7 `pohunek assistant` CLI + `session.new`
wiring). **Remaining, finish-only:** P8 (hook write hard gate — the only untouched
phase, plus most of the Security checklist), P10 (behavior eval: promote
`crates/xtask/src/eval.rs` from skeleton to a working manual release gate), and P11
(human docs: wire the existing site render into release artifacts). See the
top-level [`ROADMAP.md`](../ROADMAP.md) for how this fits the wider plan.

This is the end-to-end engineering plan for the feature
specified in [`universal-assistant.md`](./universal-assistant.md). It is grounded
in the current codebase (file:line references throughout) and covers everything:
foundations, knowledge bundle, build pipeline, embedding/materialization, the one
new protocol method, snapshot + redaction, agent selection + preflight + daemon
bootstrap, prompt composition, the CLI surface, the hook hard gate, drift/CI,
behavior eval, and human docs outputs.

The design doc is the source of truth for *what* and *why*. This plan is the
source of truth for *how* and *in what order*.

## Current Implementation Progress

Done in the current workspace:

- **P0 Foundations**: cache/runtime assistant paths in CLI and daemon, assistant
  protocol method constants, assistant protocol errors, bundle version/hash
  primitives, and path/hash tests.
- **P1 Knowledge bundle source + schema**: committed `docs/knowledge/` manual
  bundle, shared `crates/knowledge` schema and validator, strict required-field
  and link validation, unknown-frontmatter tolerance, `changed_in` list support,
  unsupported file-type detection, and validation tests.
- **P2 build/embedding skeleton**: CLI clap parser moved behind
  `pohunek_cli::command()`, thin CLI binary wrapper, `.cargo` xtask alias,
  `crates/xtask docs validate/build` skeleton, deterministic manual bundle copy,
  manifest writing, `crates/knowledge/build.rs` manual-only fallback, embedded
  bundle accessors, and `include_dir` wiring.
- **P3 materialization/runtime methods**: safe materializer, per-launch snapshot
  persistence, allowlisted concept index, CLI local/remote materialization
  helpers, `assistant.materialize` protocol and daemon handler, `daemon.doctor`
  protocol/result wrapper and daemon handler, remote version/hash mismatch guard,
  older-daemon `assistant_method_unsupported` mapping, daemon socket coverage, and
  materializer race/symlink safety tests.

Still pending:

- Full P2 generated reference pipeline (`reference/cli`, `reference/protocol`,
  `reference/config`, setup-asset reference), schema reflection, config
  descriptor, and CI drift checks.
- P4 snapshot collector and allowlist redaction.
- P5 agent selection, daemon bootstrap, and read-access preflight.
- P6 prompt composition and `--print-prompt`.
- P7 public `pohunek assistant ...` CLI surface and `session.new` wiring.
- P8 hook hard gate, P9 drift/secret-scan CI, P10 behavior eval, and P11 human
  docs outputs.

---

## 0. Grounding: What Exists Today

Workspace: 4 crates — `crates/protocol`, `crates/netbird`, `crates/daemon`
(`pohunekd`), `crates/cli` (`pohunek`). Single workspace version
(`Cargo.toml:46` `[workspace.package] version`), read via
`env!("CARGO_PKG_VERSION")`.

Relevant existing patterns:

- **CLI clap**: derive style; subcommands are `Commands` enum variants in
  `crates/cli/src/main.rs:44-126`; nested actions like `SessionAction`
  (`main.rs:312-404`). Command modules are async `run*()` fns
  (`commands/mod.rs:15-23` module list); shared `render_json()`
  (`commands/mod.rs:31-33`), `request_id()` (`commands/mod.rs:62-66`).
- **session.new**: `SessionNewParams { …, input: Option<String> }`
  (`crates/protocol/src/session.rs`); built in
  `crates/cli/src/commands/session.rs:399-413`; result `SessionNewResult.applied_input`
  confirms injection (`protocol/src/session.rs:414-424`,
  `daemon/src/api/handler.rs:465-472`). Remote/`--yes` gate:
  `confirmation_decision()` (`session.rs:195-204`).
- **Daemon start**: `commands/daemon.rs:32-51` (`start(detach)`), `locate_daemon()`
  (`daemon.rs:64-79`). **The CLI does NOT auto-start** on connect failure — it
  errors `CliError::DaemonUnreachable` (`client.rs:304-310`).
- **Asset embedding**: `include_str!` table in `commands/setup.rs:45-67`; written
  with `fs::write` + perms (`setup.rs:265-277`). **`include_dir` crate is not a
  dependency. No `build.rs` exists anywhere. No xtask, no `.cargo/config.toml`.**
- **Paths**: `crates/cli/src/paths.rs:16-112` (`Paths::resolve()`), fields
  `runtime_dir`, `socket`, `data_dir`, `log_dir`, `config_home`, `config_dir`.
  **No `cache_dir` (XDG_CACHE_HOME) field.** Daemon has its own
  `crates/daemon/src/paths.rs:28-87` (no code sharing by design).
- **Protocol method pattern**: method-name constants `protocol/src/lib.rs:68-121`;
  params/result structs per method (e.g. `SessionInputParams/Result`
  `protocol/src/session.rs:175-189`); dispatch in `daemon/src/api/handler.rs:142-177`
  with one handler fn per method (e.g. `handle_session_input` `handler.rs:546-555`);
  registry method in `daemon/src/session/mod.rs`. Version negotiation is
  **exact-match** single integer `PROTOCOL_VERSION=1` (`protocol/src/version.rs:14-19,59-68`);
  unknown method → error (open method set).
- **Capabilities**: `HostCapabilities { daemon_version, protocol_version,
  supported_agents, runtimes: Vec<AgentRuntime{agent,available,path}>, git_available,
  worktree_supported }` (`protocol/src/capabilities.rs:25-55`); built fresh in
  `daemon/src/capabilities.rs:26-67`.
- **Agent profiles**: `ResolvedProfile{program,args,env,input_rules,resume,manifest}`,
  `ResolvedAgent{name,base,profile}` (`daemon/src/agent/profile.rs:86-114`);
  owner-secure gate (`profile.rs:132-149`). **No sandbox/container** — agent runs
  with daemon UID in `cwd` (`session/mod.rs:2602-2626`). Initial-input delivery
  after readiness grace (`session/mod.rs:1013-1030,1552-1584`).
- **Snapshot data sources** (all `--json` already): `doctor` (Report,
  `commands/doctor.rs:60-65`, **CLI-local**, no daemon), `health` (daemon RPC),
  `host inspect` (HostCapabilities, daemon RPC), `project list/show/actions`
  (`protocol/src/project.rs:32-253`, daemon RPC), `session list/inspect`
  (SessionInfo `protocol/src/session.rs:325-400`, daemon RPC).
- **Existing redaction**: `origin_url` credentials already redacted
  (`project.rs:41`); profile `[env]` never persisted/crosses wire (C.4 no-secrets,
  `store/mod.rs:26-35`, `session/mod.rs:1867-1868`); error msgs and
  `SessionWarning` documented secret-free (`error.rs:61`, `session.rs:311`).
  **No general "scrub a snapshot" helper exists.**
- **CI**: `.github/workflows/ci.yml` `test` job = fmt + clippy (`-D warnings`) +
  `cargo test --workspace` + release build. `.github/workflows/release.yml` on tag
  `vX.Y.Z` = gate + matrix build + tarball+sha256. **No markdown/link/secret/docs
  tooling.** Tests use `std::process::Command` + `env!("CARGO_BIN_EXE_pohunek")`,
  inline `#[cfg(test)]`; no `assert_cmd`/`insta`.

## 0.1 Resolved Architecture Decisions

These refine the design doc and are **decided** (confirmed with the owner). They
are load-bearing for P2/P3.

1. **Two binaries kept; both embed the bundle.** `pohunek` (CLI) and `pohunekd`
   (daemon) stay separate binaries (current state, `cli/Cargo.toml:9-10`,
   `daemon/Cargo.toml:9-10`). Both embed the same generated bundle: `pohunek` for
   **local** launches and `--print-prompt` (in-process, no round-trip), `pohunekd`
   for **remote** launches (the agent runs on the remote host). The CLI never
   ships bundle bytes over the wire; the remote bundle comes from the remote
   `pohunekd`'s own embed (version-matched to that host).
2. **One new protocol method `assistant.materialize { snapshot } -> { bundle_path,
   snapshot_path, version, content_hash, concepts[] }`.** The CLI composes the
   snapshot and passes the bytes; the daemon extracts its embedded bundle, writes
   the snapshot to its host FS, and returns paths + version + hash + the concept
   index (needed because the CLI composes the TOC and cannot read the remote
   bundle's frontmatter otherwise). Local launches do the same work in-process
   without a round-trip.
3. **Daemon-side `doctor` is implemented now (second new method `daemon.doctor`).**
   Today `doctor` is CLI-local and probes the *local* host (`commands/doctor.rs`),
   which is wrong for a remote target. A `daemon.doctor` method computes the report
   on the host that runs the agent. **The snapshot uses `daemon.doctor` uniformly
   for both local and remote**, so the report always describes the agent's host.
   The existing CLI `doctor` command may converge onto `daemon.doctor` later
   (out of scope here; noted in P4).
4. **CLI clap `Command` is exposed as a library symbol** (`pohunek_cli::command()
   -> clap::Command`). `crates/cli` gains a `lib.rs` (thin `main.rs` keeps the
   bin). Reference generation introspects the command tree in-process and renders
   via `clap-markdown` — no running the built binary. The same `command()` powers
   the runbook-vs-parser CI check and the behavior eval.
5. **`build.rs` (in `crates/knowledge` only) falls back to manual-only.** It
   embeds the staged full bundle when present (`POHUNEK_KNOWLEDGE_BUNDLE` env /
   target path), else the committed `docs/knowledge/` (manual concepts only). The
   marker `reference: generated|manual-only` is recorded and surfaced in
   `--print-prompt`/manifest. Plain `cargo build`/`cargo test` never break; release
   and CI run `docs build` first and the freshness/determinism check enforces a
   full bundle for releases.
6. **New `crates/knowledge` lib** holds the schema, validator, loader, normalizer,
   embed accessor, and materializer, depended on by `cli`, `daemon`, and `xtask`.
   The embed + the single `build.rs` live **only** here; `cli`/`daemon` get the
   embedded bundle transitively. No duplication of security-critical code.
7. **Hybrid reference generation.** Protocol reference is generated by **reflection
   (`schemars`)** over the serde params/result structs (drift-free, cheap because
   they are already serde types). Config reference uses a **hand-registered
   descriptor** in `crates/knowledge` (TOML config has no machine schema; ad-hoc).
   CLI reference is clap introspection (decision 4). A CI cross-check asserts the
   descriptor's config keys exist and the protocol method list matches
   `protocol::method::*`.

---

## 1. Phase Map and Dependencies

```
P0 Foundations ────────────┬─────────────┬───────────────┐
                           v             v               v
P1 Bundle source + schema  P4 Snapshot   P5 Agent select + bootstrap
   |                       + redaction       + read-access preflight
   v                           |                 |
P2 Docs build pipeline         |                 |
   (xtask + generators         |                 |
    + build.rs embed)          |                 |
   |                           |                 |
   v                           |                 |
P3 Materialization + ──────────┴─────────────────┘
   assistant.materialize method
                           |
                           v
P6 Prompt composition + --print-prompt
                           |
                           v
P7 CLI command surface + session.new wiring + output
                           |
              ┌────────────┼────────────┐
              v            v             v
   P8 Hook hard gate   P9 Drift/CI   P10 Behavior eval
                                          |
                                          v
                                   P11 Human docs outputs (site/offline)
```

Critical path: P0 → P1 → P2 → P3 → P6 → P7. P4/P5 run in parallel after P0.
P8/P9/P10/P11 are finishing work after P7.

Parallelizable tracks (separate owners, separate files):
- **Track A (Knowledge)**: P1 + content authoring + P11.
- **Track B (Pipeline)**: P2 + P9.
- **Track C (Runtime)**: P0 + P3 + P5 + P6 + P7 + P8.
- **Track D (Snapshot/Security)**: P4 + redaction + secret scan + P10.

---

## 2. Phase 0 — Foundations

Implementation status: **done** in the current workspace.

Goal: shared primitives the rest depends on. Small, low-risk, lands first.

### Tasks

1. **Cache dir in paths.** `crates/cli/src/paths.rs`: add `cache_dir: PathBuf`
   (`$XDG_CACHE_HOME/pohunek` or `~/.cache/pohunek`) using the existing
   `xdg_or_home_relative` helper (`paths.rs:84-112`); add
   `assistant_bundle_cache_dir()` → `cache_dir/knowledge` and
   `assistant_runtime_dir(session_or_launch_id)` → `runtime_dir/assistant/<id>`.
   Mirror the same additions in `crates/daemon/src/paths.rs:28-87` (daemon needs
   its own cache for remote materialization).
2. **Version + content-hash plumbing.** Define `assistant::BUNDLE_VERSION =
   env!("CARGO_PKG_VERSION")` and a `bundle_content_hash()` (sha256 over the
   embedded bundle bytes, computed once, memoized). Add `sha2` to workspace deps
   (it is not currently a dependency — confirm and add to `Cargo.toml`
   `[workspace.dependencies]`).
3. **Error codes.** In `crates/protocol/src/error.rs` (`ErrorClass`
   `error.rs:18-44`), reserve assistant error codes used later:
   `no_capable_agent`, `bundle_unavailable`, `materialization_failed`,
   `agent_cannot_read_bundle`, `assistant_method_unsupported` (the last is the
   CLI's interpretation of a daemon `method_not_found` for `assistant.materialize`).
4. **Method constants.** Add `pub const ASSISTANT_MATERIALIZE: &str =
   "assistant.materialize";` and `pub const DAEMON_DOCTOR: &str = "daemon.doctor";`
   to `protocol/src/lib.rs:68-121`.

### Tests
- `paths` unit tests for the new dirs (follow existing `paths.rs` test style).
- hash determinism unit test (same bytes → same hash).

### Exit criteria
Workspace compiles; new paths + constants + error codes exist and are tested.

---

## 3. Phase 1 — Knowledge Bundle Source + Local Schema

Implementation status: **done** in the current workspace.

Goal: the committed `docs/knowledge/` tree and the strict local schema validator.
No runtime wiring yet. (Track A can author content in parallel with everything.)

### Tasks

1. **Create `docs/knowledge/`** per the design layout (only hand-authored
   concepts; **no `reference/`** — that is generated in P2):
   `index.md`, `log.md`, `concepts/`, `guides/`, `runbooks/`, `safety/`,
   `assistant/system.md`, `assistant/source-map.md`.
2. **Frontmatter schema type.** New module (lives where the validator and the
   pipeline both reach it — propose a new crate-internal module under the xtask
   or a small shared `crates/knowledge/` lib if reuse across cli+daemon+xtask is
   needed; **decide in P2**). Define `Concept { type, id, title, description,
   source_kind, tags?, intents?, generated_from?, since?, changed_in?, deprecated?,
   citations? }` with serde + the closed `ConceptType` enum (13 variants from the
   doc). `serde_yaml` is **not** a current dependency — add it.
3. **Validator** (pure function, reused by CI in P9): parse every non-reserved
   `.md`, enforce required fields, closed `type`, unique `id`, `since` required
   for behavior-bearing types (`CliCommand`, `ConfigReference`, `ProtocolMethod`,
   `ProtocolEvent`, `Runbook`), relative internal links resolve, reserved
   `index.md`/`log.md` rules.
4. **Author the operational content** (Track A, ongoing): product model, config
   reference (manual prose; structured reference is generated), CLI runbooks,
   safety concepts (`trust-model`, `secrets`, `repo-pohunek`), `system.md`
   (mission), `source-map.md` (the path list from the design doc).

### Tests
- Validator unit tests: a good fixture bundle passes; fixtures with each violation
  (missing field, bad type, dup id, missing `since` on a `CliCommand`, broken
  link) fail with a precise error.

### Exit criteria
`docs/knowledge/` exists with real content; `validate_bundle(dir)` passes on it.

---

## 4. Phase 2 — Docs Build Pipeline (xtask + generators + embed)

Implementation status: **partially done** in the current workspace. The CLI lib
refactor, xtask command skeleton, manual bundle validation/build path,
manifest-writing path, `crates/knowledge/build.rs`, and embedded bundle accessors
are implemented. Full generated reference concepts, `schemars` protocol
reflection, config descriptors, setup-asset reference generation, and CI drift
checks remain pending.

Goal: one build entry point that generates reference concepts, merges with the
committed bundle, normalizes, and makes the merged bundle embeddable. This is the
hardest infra phase; resolve the chicken-egg here.

### Decisions baked in
- **New `crates/xtask` binary** (workspace member) is the generator host. Add a
  `cargo xtask docs build` alias via a new `.cargo/config.toml`
  (`[alias] xtask = "run -p xtask --"`). The eventual `pohunek docs build`
  subcommand simply shells to the same logic or is added in P11.
- **Expose the CLI command tree as a lib.** Refactor `crates/cli` so the clap
  `Cli` builder is reachable as `pohunek_cli::command() -> clap::Command` (add a
  `lib.rs` exposing it; `main.rs` keeps the binary). xtask depends on
  `pohunek-cli` as a lib and walks the `clap::Command` tree to emit
  `reference/cli/*.md` — no running the built binary.
- **Shared knowledge lib.** Create `crates/knowledge` (lib) holding: the
  `Concept`/`ConceptType` schema, the validator (from P1), the bundle loader, the
  normalizer, and the embed accessor. `cli`, `daemon`, and `xtask` all depend on
  it. (This replaces the "decide in P2" note from P1.)

### Tasks

1. **Generators (in xtask, output `target/pohunek-docs/knowledge-bundle/reference/`):**
   - `reference/cli/*` from `pohunek_cli::command()` (walk subcommands, args,
     help) → one `CliCommand` concept per command with `source_kind: generated`,
     `generated_from`, and `since`.
   - `reference/protocol/*` by **`schemars` reflection** over the serde
     params/result structs in `crates/protocol` (drift-free) → `ProtocolMethod`/
     `ProtocolEvent` concepts. Add `schemars` + derive on the relevant protocol
     structs. A CI cross-check asserts the generated method set matches
     `protocol::method::*` constants.
   - `reference/config/*` from a **hand-registered descriptor** of
     `launcher.conf`/`templates.toml`/`actions.toml`/`agents/*.toml` in
     `crates/knowledge` (TOML config has no machine schema). A CI check asserts the
     descriptor's referenced files/keys exist.
   - setup-asset reference from the `SCRIPTS` table (`setup.rs:45-67`).
2. **Merge + normalize:** combine committed `docs/knowledge/` + generated
   `reference/` into `target/pohunek-docs/knowledge-bundle/`; normalize
   (stable file order, canonical frontmatter key order, LF endings) so the bundle
   is **byte-deterministic**; compute `content_hash`; write `manifest.json`
   (`pohunek_version`, `knowledge_schema_version`, `content_hash`, `sources`,
   `generated_at` — pass timestamp in, do not call the clock in deterministic
   code).
3. **Embed via a single `build.rs` + `include_dir` in `crates/knowledge`:** add
   `include_dir` to `knowledge` deps. Its `build.rs`:
   - resolves the merged bundle dir: prefer `POHUNEK_KNOWLEDGE_BUNDLE` env (set by
     `xtask docs build`/release), else `target/pohunek-docs/knowledge-bundle`, else
     **fallback to committed `docs/knowledge/` (manual-only)**;
   - copies it into `$OUT_DIR/knowledge-bundle` (guarantees the path exists);
   - emits `bundle_version`/`bundle_content_hash`/`reference_mode` consts.
   `crates/knowledge` embeds with `static BUNDLE: include_dir::Dir =
   include_dir!("$OUT_DIR/knowledge-bundle");` and exposes it. `cli` and `daemon`
   depend on `knowledge` and get the embedded bundle transitively — **no build.rs
   in `cli` or `daemon`**.
4. **`cargo xtask docs build`** orchestrates: generate → merge → normalize →
   set `POHUNEK_KNOWLEDGE_BUNDLE` → trigger crate build. Release pipeline calls
   this before `cargo build`.

### Tests
- xtask unit/integration: generation is deterministic (run twice → identical
  bytes/hash); manifest fields correct; merged bundle validates against the P1
  schema.
- `build.rs` fallback test: a clean `cargo build` (no `docs build` run) embeds the
  manual-only bundle and compiles.

### Exit criteria
`cargo xtask docs build` produces a deterministic merged bundle; `cargo build`
embeds a bundle (full after docs build, manual-only otherwise); `crates/knowledge`
exposes the embedded `Dir` + version + hash.

---

## 5. Phase 3 — Materialization + `assistant.materialize` Method

Implementation status: **done** in the current workspace.

Goal: turn the embedded bundle into files the agent can read, version-shared, on
the host that runs the agent; add the one protocol method for remote.

### Tasks

1. **Materializer (in `crates/knowledge`):** `materialize(cache_dir, version_hash)
   -> PathBuf` extracts the embedded `Dir` into
   `<cache_dir>/knowledge/<version-hash>/` **once** (idempotent: skip if the dir
   exists and a `.complete` marker is present; write atomically via temp dir +
   rename). `gc(cache_dir, keep=current_version_hash)` removes stale version dirs.
2. **Concept index accessor:** `bundle_index() -> Result<Vec<ConceptMeta>,
   BundleIndexError>` returning only the allowlisted frontmatter fields
   (`type,id,title,description,intents,since,changed_in,deprecated`) from the
   embedded bundle — used to build the TOC.
3. **Local path (CLI):** the CLI calls the materializer in-process (it embeds the
   bundle), into the local `assistant_bundle_cache_dir()`, and writes the
   per-launch snapshot into `assistant_runtime_dir(<launch-id>)/snapshot.json`.
4. **Protocol method `assistant.materialize`:**
   - `protocol/src/assistant.rs` (new): `AssistantMaterializeParams { snapshot:
     String }` and `AssistantMaterializeResult { bundle_path: String, snapshot_path:
     String, version: String, content_hash: String, concepts: Vec<ConceptMeta> }`.
     Follow the `SessionInputParams/Result` shape (`session.rs:175-189`).
   - Handler arm in `daemon/src/api/handler.rs:142-177` →
     `handle_assistant_materialize`: materialize the daemon's embedded bundle into
     the daemon cache, write the provided `snapshot` bytes into a per-launch
     runtime dir on the daemon host, return both paths + version + hash + index.
   - Registry/helper in daemon (new small module, e.g.
     `daemon/src/assistant/mod.rs`) so `handler.rs` stays thin.
4b. **Protocol method `daemon.doctor`:** move the doctor checks so they run on the
   daemon's own host. `DaemonDoctorResult { report: Report }` (reuse/relocate the
   `Report`/`Check`/`Status` types from `commands/doctor.rs:60-65` into a shared
   place — `protocol` or `crates/knowledge` — so both CLI and daemon use one type).
   Handler arm + a `daemon/src/doctor.rs` that performs the host-local probes the
   CLI does today. The snapshot collector (P4) calls this for both local and
   remote. The existing CLI `doctor` command may later call `daemon.doctor` for
   consistency (out of scope; note only).
5. **Version-skew guard:** the result carries `version`/`content_hash`; the CLI
   asserts the remote `version` matches the launch expectation and surfaces a
   mismatch rather than launching silently.
6. **Older-daemon detection:** CLI maps a daemon `method_not_found` for
   `assistant.materialize` to `assistant_method_unsupported` and (remote) fails
   before launch unless `--degraded`.

### Tests
- Materializer: idempotent (second call no-ops), atomic (interrupted extract
  leaves no `.complete`), GC removes stale only.
- Protocol roundtrip test (extend `protocol/tests/roundtrip.rs`).
- Daemon socket test (extend `daemon/tests/health_socket.rs` style):
  `assistant.materialize` returns readable paths + index; snapshot bytes land on
  disk.
- CLI maps unsupported method → structured error.

### Exit criteria
Bundle materializes locally and via the daemon method; paths are readable; index
returned; GC + idempotency proven.

---

## 6. Phase 4 — Snapshot Collector + Allowlist Redaction

Goal: produce the redacted `snapshot.json`. **Security-critical** — allowlist, not
denylist.

### Tasks

1. **Snapshot types (CLI, e.g. `commands/assistant/snapshot.rs`):** typed structs
   for the doc's sections (`assistant`, `paths`, `doctor`, `host`, `projects`,
   `sessions`, `config_scan`, `source_tree`). Each field is an explicitly chosen,
   allowlisted value. **The serializer must be unable to emit an unknown field**
   (no `#[serde(flatten)]` of foreign maps; no passthrough of raw RPC `Value`).
2. **Collection (best-effort, per-item warnings):**
   - `doctor` via the new `daemon.doctor` method (P3 task 4b), used for **both
     local and remote** so the report describes the agent's host; copy only
     allowlisted fields; **redact `Check.detail`** (may leak PATH/env).
   - `host.inspect` → capabilities; **redact `runtimes[].path`** to a bool
     `available` (drop absolute path).
   - `project list/show/actions` → **redact absolute paths** (`repo_root`,
     `git_common_dir`, `cwd`, `worktree_path`); `origin_url` already redacted but
     re-verify; keep ids/labels/action names.
   - `session list` → **redact path fields** (`cwd`, `repo`, `worktree_path`,
     `native_session_path`); keep ids/state/counts.
   - `config_scan` (**new logic** — no enumeration exists today): scan host
     `config_dir` and repo `.pohunek/` for **filenames/existence only** (prompt
     names, `templates.toml`/`actions.toml` presence + parse status, agent profile
     names, hook names). Never read hook bodies or config bodies.
   - `source_tree`: git root/branch/dirty summary + `version_matches_binary`.
3. **Path redaction helper:** turn absolute paths into a stable
   non-identifying form (e.g. `~/…/<basename>` or a labeled token); never emit
   `$HOME`-revealing absolute paths.
4. **Three-line orientation:** derive `daemon=…, project=…, agent=…` for inline
   prompt use.
5. **Remote variant:** compose from remote RPCs, including `daemon.doctor` against
   the remote daemon (so the doctor report describes the remote host); the
   resulting bytes are handed to `assistant.materialize` to persist on the remote
   host.

### Tests
- Allowlist enforcement: a test that constructs a snapshot and asserts the JSON
  contains **only** known keys; a compile-or-runtime guard that adding a field
  requires updating the allowlist test.
- Redaction: profile `[env]` keys/values never appear (seed a profile, assert
  absence); absolute paths are redacted; doctor `detail` redacted.
- Best-effort: a failing source becomes a warning, others still collected.

### Exit criteria
`snapshot.json` is produced, allowlist-built, with all sensitive fields redacted;
remote variant produced from RPC data.

---

## 7. Phase 5 — Agent Selection + Read-Access Preflight + Daemon Bootstrap

Goal: pick the agent, confirm it can read the bundle, and bring up the daemon.

### Tasks

1. **Agent selection** (`commands/assistant/mod.rs`): resolution order from the
   doc — `--agent` > configured default > ranking (`pohunek-assistant` profile >
   `codex` > `claude` > other codex/claude-based profiles). Source availability
   from `HostCapabilities.runtimes` (`host.inspect`). Report selection + reason in
   human and `--json` output.
2. **Read-access preflight:** after materialization, assert the agent's execution
   context can read the knowledge dir + snapshot file. **Today the agent shares the
   daemon UID with no sandbox** (`profile.rs`, `session/mod.rs:2602-2626`), so the
   check is a path-exists + readable assertion on the agent host. Keep the
   sandbox/container branch as a forward guard: if a future profile declares a
   restricted root, verify containment; if unverifiable, fail before
   `session.new` with the path + constraint + remedy.
3. **Daemon bootstrap (NEW):** on local target, if `Client::connect` fails with
   `DaemonUnreachable` and not `--no-start-daemon`, call `daemon::start(detach=true)`
   (`commands/daemon.rs:32-51`) then poll the socket with a bounded timeout; on
   failure return the exact manual command. (This is new behavior — the CLI has no
   auto-start today.)
4. **`pohunek-assistant` profile scaffold:** `pohunek setup` writes a *commented*
   template for the profile; never auto-enabled.

### Tests
- Selection determinism + visibility (unit, over a fabricated `HostCapabilities`).
- Preflight fails before `session.new` when the path is unreadable (simulate with
  a non-existent/locked dir).
- Bootstrap: connect-fail → start → poll → success; and the `--no-start-daemon`
  and start-failure branches.

### Exit criteria
Agent chosen + reported; preflight gates launch; local daemon auto-starts.

---

## 8. Phase 6 — Prompt Composition + `--print-prompt`

Goal: the small navigational prompt; pure and inspectable.

### Tasks

1. **`commands/assistant/prompt.rs` (pure):** `compose(intent, request, concepts,
   bundle_path, snapshot_path, snapshot_orientation, version) -> String`. Sections
   exactly per the design doc: Mission, inline Safety (sourced from
   `safety/*.md` at build into a const so it can't drift from the bundle —
   generate the inline safety block during `docs build`), User Intent,
   Your Knowledge Base (paths + version-skew note), Relevant Concepts (TOC filtered
   by `intents`), Live Snapshot (3-line orientation + file path), Source Map, First
   Step. **Never inline bundle bodies.**
2. **TOC filter:** select `concepts` whose `intents` contains the active intent;
   default intent `help` → root index, no aggressive filter.
3. **`--print-prompt`:** compose and print (prompt + resolved bundle path + TOC +
   snapshot path); never connect for `session.new`. (For remote, still calls
   `assistant.materialize` to get the real remote paths/index, then prints.)

### Tests
- Composition stability (golden string for fixed inputs; the repo has no `insta`,
  so use an inline expected string compare like existing tests).
- Prompt never contains a bundle body or a non-allowlisted frontmatter field.
- TOC reflects intent filtering; `--print-prompt` does not start a session
  (CLI integration test, `tests/` style with `CARGO_BIN_EXE_pohunek`).

### Exit criteria
Deterministic prompt; `--print-prompt` exits without launching.

---

## 9. Phase 7 — CLI Command Surface + session.new Wiring + Output

Goal: the user-facing command, end to end.

### Tasks

1. **Clap:** add `Assistant { #[command(subcommand)] action: AssistantAction }`
   to `Commands` (`main.rs:44-126`) and define `AssistantAction` (default +
   `setup`/`project`/`update`/`debug`/`help` wrappers) with options from the doc
   (`--intent`, `--agent`, `--host`, `--project`, `--repo`, `--branch`,
   `--base-branch`, `--yes`, `--json`, `--print-prompt`, `--no-snapshot`,
   `--degraded`, `--no-start-daemon`). Wrappers only set `intent`. Add parser
   tests mirroring `main.rs:750+`.
2. **Command module `commands/assistant/mod.rs`:** orchestrate
   bootstrap → select agent → materialize (local in-proc / remote method) →
   snapshot → preflight → compose prompt → `session.new` with `input = prompt`
   (reuse `build_new_request` path `session.rs:399-413`). Preserve
   `confirmation_decision` (`session.rs:195-204`) for remote/`--yes`.
3. **`--degraded`:** the only sanctioned no-bundle launch (snapshot + source map
   only); explicit, never a fallback.
4. **Output:** human block (started session, agent, intent, knowledge version,
   snapshot included, attach command) and `--json` `assistant` metadata
   (`render_json`, `commands/mod.rs:31-33`). Check `SessionNewResult.applied_input`
   (`session.rs:414-424`); if not confirmed, surface the warning and do not claim
   the prompt was delivered.

### Tests
- CLI integration (`crates/cli/tests/assistant.rs`): `--print-prompt setup`
  prints + exits; `setup --json` issues `session.new` with composed `input`;
  remote preserves `--yes`; remote-unsupported fails before launch unless
  `--degraded`; output includes attach + version.
- Parser unit tests for every wrapper/flag.

### Exit criteria
`pohunek assistant [intent] [request]` launches a working local assistant in one
command; all flags behave; remote gated correctly.

---

## 10. Phase 8 — Hook Write Hard Gate

Goal: assistant-written hooks require explicit per-file confirmation, independent
of `--yes`.

### Tasks
1. This is primarily an **agent-behavior contract** (the assistant runs as an
   agent editing files), so the gate is enforced at the **safety prompt** + any
   pohunek-mediated hook-write path. Since hook files are plain files the agent
   writes directly, the enforceable controls are:
   - the inline safety block + `safety/repo-pohunek.md` + `safety/secrets.md`
     state the hard rule unambiguously (no `--yes` coverage for hooks);
   - **any pohunek command that installs/writes hooks** (if/when one exists, e.g.
     a future `pohunek hook add`) must prompt per-file regardless of `--yes`;
   - document the quarantine path convention for non-interactive contexts.
2. Audit existing hook-touching code paths (`session/mod.rs` hook runner,
   `project/config.rs`) to confirm none lets an agent enable a hook without a
   filesystem write the user can see; document the boundary.

### Tests
- If a pohunek-mediated hook-write path exists/added: a test that `--yes` does not
  bypass the per-file confirmation.
- Safety concept content test: the hard rule is present in the embedded bundle and
  the inline safety block.

### Exit criteria
The hook hard-gate rule is enforced where pohunek mediates, and unambiguously
stated to the agent everywhere else.

---

## 11. Phase 9 — Drift Checks + CI Integration

Goal: the lean guardrail set, wired into CI.

### Tasks (add to `.github/workflows/ci.yml` `test` job, or a new `docs` job)
1. **Schema validation**: `cargo xtask docs validate` runs the P1 validator over
   `docs/knowledge/` (frontmatter, type, unique id, behavior-bearing `since`,
   relative links, reserved files).
2. **Deterministic generation**: run `docs build` twice; fail if `content_hash`
   differs. Generators must not call the clock/RNG (pass timestamp in).
3. **Reference freshness**: generation runs from current code each build (never
   committed) → cannot be stale; the check is "generation succeeds + deterministic"
   not "committed diff."
4. **Runbook-vs-parser**: validate runbook command examples parse against
   `pohunek_cli::command()` (`try_parse_from`), so the agent is never taught a
   non-existent command.
5. **Source-map paths exist**: assert every path in `assistant/source-map.md`
   exists in the repo.
6. **Secret scan (public-safe rule)**: scan all bundle bodies for secret-like
   patterns (keys, tokens, `[env]` values, credentials). Add `gitleaks` (or a
   custom regex pass in xtask) as a CI step over `target/pohunek-docs/knowledge-bundle/`.
7. **Snapshot allowlist test** + **redaction test** run in normal `cargo test`.

CI placement: doc checks added to `ci.yml` (block PRs). Release determinism/secret
scan also gate `release.yml` before the build job.

### Exit criteria
CI fails loudly on schema violations, non-determinism, broken source-map paths,
hallucination-prone runbooks, and secret-bearing bundle bodies.

---

## 12. Phase 10 — Behavior Eval

Goal: prove the assistant is useful, not just that the prompt assembles. Runs as a
**local/manual release gate**, not blocking per-PR CI.

### Tasks
1. **Fixture states**: seeded environments — "daemon down", "launcher
   misconfigured", "project not registered", "stale setup assets after update".
2. **Harness**: a script/xtask subcommand that, for each fixture, runs the real
   assistant over one runtime (default `codex`), captures the transcript, and
   asserts the expected concrete outcome + **hard-fails on hallucinated commands**
   (cross-check emitted `pohunek …` commands against `pohunek_cli::command()`).
3. **Reporting**: pass/fail per fixture; not wired to block PRs (token cost,
   non-determinism); documented as a pre-release manual gate.

### Exit criteria
A runnable eval that catches knowledge/navigation regressions over the fixtures.

---

## 13. Phase 11 — Human Docs Outputs (site + offline)

Goal: the same bundle rendered for humans.

### Tasks
1. **Site render**: `docs build` renders the merged bundle to
   `target/pohunek-docs/site/` (static HTML; pick a minimal renderer or a simple
   in-house markdown→HTML pass — no heavy SSG needed for a single-user tool).
2. **Offline bundle**: `target/pohunek-docs/offline/` packaged with the release
   tarball (`release.yml` build job), version-stamped.
3. **GitHub readability**: relative links keep `docs/knowledge/` browsable as-is.
4. **Optional `pohunek docs build` subcommand**: thin wrapper over the xtask logic
   for users without the repo (or leave as `scripts/docs/build` per the doc).

### Exit criteria
Site + offline artifacts build from the same manifest; offline ships with release.

---

## 14. Cross-Cutting: Security Checklist

- [ ] Snapshot is allowlist-built; serializer cannot emit unknown fields (P4).
- [ ] Profile `[env]` and process env never collected (P4) — re-verified by test.
- [ ] Absolute paths redacted in snapshot (P4).
- [ ] Bundle bodies secret-scanned in CI (P9); public-safe rule holds.
- [ ] Prompt composer reads only allowlisted frontmatter fields (P6).
- [ ] Hook writes gated independent of `--yes`; quarantine in non-interactive (P8).
- [x] No bundle bytes shipped over the wire; remote bundle from remote binary (P3).
- [ ] Remote launch fails before launch without a readable bundle unless
      `--degraded` (P3/P7).
- [ ] Owner-secure profile gate preserved (`profile.rs:132-149`); assistant never
      weakens name-guard/containment checks.

## 15. Cross-Cutting: Test Matrix

| Area | Unit | CLI integ | Daemon socket | CI gate |
|---|---|---|---|---|
| Bundle schema/validator | ✔ (P1) | | | ✔ |
| Generation determinism | ✔ (P2) | | | ✔ |
| build.rs fallback | ✔ (P2) | | | |
| Materializer idempotent/GC | ✔ (P3) | | | |
| Materializer race/symlink safety | ✔ (P3) | | | |
| `assistant.materialize` | ✔ roundtrip | ✔ helper tests | ✔ (P3) | |
| `daemon.doctor` | ✔ result shape | | ✔ (P3) | |
| Snapshot allowlist/redaction | ✔ (P4) | | | ✔ |
| Agent selection | ✔ (P5) | | | |
| Read-access preflight | ✔ (P5) | ✔ | | |
| Daemon bootstrap | ✔ (P5) | ✔ | | |
| Prompt composition | ✔ (P6) | ✔ print-prompt | | |
| Command surface/flags | ✔ parser | ✔ (P7) | ✔ session.new | |
| Hook hard gate | ✔ (P8) | ✔ if mediated | | |
| Runbook-vs-parser, source-map | | | | ✔ (P9) |
| Secret scan | | | | ✔ (P9) |
| Behavior eval | | | | manual gate (P10) |

## 16. Resolved Decisions and Remaining Risks

All architecture questions are resolved (see §0.1):

- Build chicken-egg → clap `Command` as a lib + xtask generation + single
  `build.rs` in `crates/knowledge` with manual-only fallback.
- Reference generation → **hybrid**: `schemars` reflection for protocol,
  hand-registered descriptor for config, clap introspection for CLI.
- Binaries → two binaries kept, both embed.
- Materialize → one method `assistant.materialize`.
- Doctor → daemon-side `daemon.doctor`, used uniformly for local + remote.
- Shared code → new `crates/knowledge`.

Remaining (implementation) risks to watch:

1. **Binary size** — both binaries embed the markdown bundle. Markdown is small;
   if it grows, compress the embed (decompress at materialize). Not a v1 concern.
2. **`schemars` derive surface** — adding `schemars` derive across protocol structs
   touches `crates/protocol`; keep it behind a feature or minimal so it does not
   bloat the wire crate. Verify it does not change serialized shapes.
3. **`Report` type relocation** — moving `doctor`'s `Report`/`Check`/`Status` into a
   shared crate must not change the existing `pohunek doctor --json` output. Add a
   regression test on the current JSON shape before relocating.
4. **Two new protocol methods** (`assistant.materialize`, `daemon.doctor`) on an
   exact-match protocol version (`version.rs:14-19`): older daemons return
   `method_not_found`. The CLI must map both to clear errors and gate remote
   accordingly (fail-before-launch unless `--degraded`).

## 17. Suggested Sequencing for One or More Implementers

- **Milestone 1 (local assistant works):** P0 → P1 (skeleton content) → P2 →
  P3 (local only) → P4 → P5 → P6 → P7 (local). Demo: `pohunek assistant setup`
  launches locally with a real materialized bundle and redacted snapshot.
- **Milestone 2 (remote + safety):** P3 (`assistant.materialize` method) → P7
  (remote path, `--degraded`) → P8.
- **Milestone 3 (guardrails + docs):** P9 → P10 → P11 → content completion (Track
  A) → flip CI doc checks to blocking.

Definition of Done is satisfied when every box in §14, the §15 matrix, and the
design doc's Definition of Done are green, and Milestones 1–3 are complete.

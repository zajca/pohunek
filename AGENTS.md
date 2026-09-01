# AGENTS.md

Canonical guide for any coding agent working in this repository. Keep it short,
accurate, and current — if you change a command, a crate boundary, or a
convention, update this file in the same change.

## What pohunek is

`pohunek` is a **single-user control plane for durable coding-agent sessions**
across the operator's own machines. A Rust daemon (`pohunekd`) owns the logical
session registry and public API on each host; one isolated
`pohunek-sessiond` worker owns each live PTY and agent process. The Rust CLI
(`pohunek`) drives the daemon locally over a Unix socket and remotely over a
NetBird/WireGuard address. A native Iced GUI (`pohunek-gui`) is an optional
client.

It is pre-1.0 and experimental: wire shapes, config files, and on-disk metadata
may change freely. **Do not add backward-compatibility shims** unless asked.

Authoritative design lives in `docs/architecture.md` (it wins over `idea.md`).
Hard constraints, decided on purpose — respect them in every change:

- **Single operator only.** No multi-user auth, no shared-tenant model. The
  trust boundary is owner-only socket/file permissions plus the NetBird network.
- **No central server.** The CLI talks directly to each host's daemon; each host
  is authoritative for its own PTYs, state, logs, and worktrees.
- **PTY/TUI-first.** Agents run in real terminals (Codex, Claude Code, and the
  pinned local Hermes Agent runtime are first-class). Not a re-rendered control
  plane.
- **Remote transport is direct over NetBird**, never SSH bridging.
- **Providers (Linear/GitHub) are shell-out based** (`gh`, Linear GraphQL) and
  live only in client surfaces (CLI scripts, gui-core), never in the daemon.
- **Protocol:** newline-delimited JSON over a Unix socket (local) and a TCP
  listener on the NetBird interface (remote); attach uses a separate raw-byte
  connection per PTY.

## Repository map

Cargo workspace, edition 2021, MSRV 1.96. Binaries: `pohunek` (CLI),
`pohunekd` (daemon), `pohunek-gui` (GUI).

| Crate | Role |
|-------|------|
| `crates/protocol` | Shared control-protocol envelopes + version negotiation. The wire contract. |
| `crates/client`   | SDK client: typed errors, daemon transport, standalone host discovery with bounded NetBird probing. |
| `crates/daemon`   | Host control plane (`pohunekd`): logical registry, worker reconciliation, public protocol, detection/hooks. |
| `crates/worker-protocol` | Private versioned daemon-to-worker protocol and framing. |
| `crates/session-worker` | Durable per-session PTY owner (`pohunek-sessiond`). |
| `crates/cli`      | CLI (`pohunek`): commands over the local protocol. |
| `crates/prompt`   | Shared prompt rendering for provider launch flows. |
| `crates/knowledge`| Knowledge-bundle primitives for the assistant and offline docs. |
| `crates/terminal` | Shared VT screen tracking and attach compositing primitives. |
| `crates/netbird`  | NetBird status parsing, host resolution, bind-address validation. |
| `crates/paths`    | Shared XDG path and local socket contract for daemon, CLI, and GUI clients. |
| `crates/hostcheck`| Host environment probes shared by `doctor` and the daemon's `doctor` RPC. |
| `crates/logging` | Process-safe size rotation and retention for daemon and per-session worker logs. |
| `crates/gui-core` | Pure, headless state + SDK bridge for the GUI (no Iced dependency; fully unit-testable). |
| `crates/gui`      | Native Iced shell that wraps `gui-core` in `Task`/`Subscription`. |
| `crates/xtask`    | Workspace automation (docs, TypeScript generation, and pinned Hermes compatibility evidence). |
| `web/`            | Bun workspace: generated protocol types, SDK runtime, control-center backend/client core/SPA, and testkit. |

Other top-level: `compat/` (pinned upstream compatibility locks and sanitized
goldens), `docs/` (architecture, roadmap, phases, knowledge source), `scripts/`
(rofi/sway launchers, release helper).

## Build, test, lint — the gates that must pass

CI runs with `RUSTFLAGS=-D warnings` (warnings are errors). Run these before
considering work done; they mirror CI exactly:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features   # must be clean under -D warnings
cargo build -p pohunek-session-worker --bin pohunek-sessiond  # daemon tests spawn it by path
cargo test --workspace --all-features
cargo build --workspace --release                        # release profile must build
cargo xtask docs check                                   # schema/drift/source-map/secrets/runbooks
cargo xtask hermes compatibility --pohunek-bin ABS       # pinned, model-free Hermes CLI/golden gate
```

Web workspace gates:

```bash
cd web
bun install --frozen-lockfile
bun run typecheck
bun run lint
bun test
bunx playwright install --with-deps chromium  # prerequisite for browser e2e
bun run test:e2e
```

The real-daemon web suite is opt-in locally and mandatory in CI after building
`pohunekd`, `pohunek-sessiond`, and `pohunek` (the suite runs the daemon in
subprocess worker mode, which spawns `pohunek-sessiond` from beside `pohunekd`,
and the Hermes plugin e2e drives the real CLI):

```bash
cargo build -p pohunek-daemon -p pohunek-session-worker -p pohunek-cli
POHUNEK_E2E=1 POHUNEK_DAEMON_BIN=/absolute/path/to/target/debug/pohunekd \
  POHUNEK_CLI_BIN=/absolute/path/to/target/debug/pohunek \
  POHUNEK_PYTHON_BIN=/usr/bin/python3 \
  bun test sdk/test/e2e.test.ts backend/test/real-daemon.e2e.test.ts
```

For control-center development, `bun run dev` starts two fixture daemons, the
backend, and the Vite frontend from `web/`; it does not require a Rust daemon or
NetBird. Bun remains the workspace runtime, but `node` must be available on
`PATH` because the orchestrator runs Vite in a Node child process for WebSocket
proxy compatibility; `POHUNEK_NODE_BIN` overrides a nonstandard Node path.

A protocol change is not done until `cargo xtask ts check` passes; regenerate
with `cargo xtask ts generate`.

Useful narrower loops:

```bash
cargo test -p pohunek-gui-core                # one crate
cargo test -p pohunek-cli some_test_name      # one test
cargo clippy -p pohunek-daemon --all-targets  # lint one crate
```

Hermes M3 supports only the pinned local interactive Hermes Agent `0.20.0`
runtime and its explicit profile-owned Pohunek operator plugin. The stable
model-free compatibility gate (`cargo xtask hermes compatibility
--pohunek-bin ABS`) exercises the pinned CLI and plugin surface:
list/enable/disable, supported tool/skill/hook registration, profile/home target
resolution, and Hermes integration install/status/doctor/uninstall. It must use
an isolated profile and must never start a model turn, access an operator profile
or `state.db`, read credentials, or download a runtime. A missing pinned
executable is a failure, never a green skip. Release-archive verification uses
`packaging/smoke-hermes-plugin-release` with an explicitly supplied preinstalled
pinned executable; it proves the extracted CLI embeds the plugin and generated
skill without a source-tree asset path. Hermes PTY goldens are refreshed
explicitly and are never regenerated by CI. The compatibility gate is expected
to fail while any checked-in golden remains `pending`; do not report that gate
green until all required captures or a legitimate alternate-TUI `unsupported`
diagnosis are committed. The executable path must be absolute.
Refresh the evidence with the real pinned Hermes process and PTY against the
repository-owned deterministic model mock:

```bash
cargo xtask hermes refresh-goldens --hermes-bin ABS
```

The mock model endpoint binds only to IPv4 loopback.
It requires no provider credentials and incurs no provider cost. Each of the six
model-bearing classic scenarios starts a new Hermes process and must issue this
exact localhost sequence: five ordered detection GETs to `/api/v1/models`,
`/api/tags`, `/v1/props`, `/props`, and `/version`, each receiving a
deterministic HTTP 404; then exactly one `POST /v1/chat/completions`. Discovery
is not cached across those processes. The isolated config statically pins
`pohunek-compat-v1`, `context_length: 64000`, and `discover_models: false`, so
Hermes does not request `/v1/models` and the mock does not permit that path.
Each isolated home is seeded with a fresh nonempty `models_dev_cache.json`, so
Hermes satisfies that remote metadata lookup locally. Harness-owned proxy
variables route any other HTTP(S) attempt to the loopback mock, which rejects
proxy `CONNECT` and absolute-form external requests fail closed; this remains
an application-level defense, not OS-level network containment. Model-response
evidence follows the pinned streaming response frame as ordered rounded header,
exact content, and rounded footer events across prompt-toolkit redraws.
The `prompt-ready` and `exit` classic scenarios issue no model API requests.
The mock also checks the POST model identifier and last user prompt, plus the
terminal tool for terminal scenarios.
The refresh uses isolated temporary `HOME`, `HERMES_HOME`, XDG, and Python
locations, bounded semantic state waits, and process-group cleanup. Never point
it at or copy from the operator's real Hermes home, and review every refreshed
fixture before committing it.

Extra CI jobs (run if your change touches deps/features): `cargo audit`,
`cargo hack --feature-powerset --workspace clippy --all-targets`,
`cargo udeps`. Note `knowledge` gates its protocol bridge behind a `protocol`
feature — `--all-features` only covers the everything-on case.

## Coding conventions (project-specific)

- **Rust guidelines are mandatory.** This repo follows the Microsoft Pragmatic
  Rust Guidelines, vendored in-repo at **`.agents/rust-guidelines/`**. Before
  writing or modifying any `.rs` file, read the files that apply to your task
  from that directory and apply them; `.agents/rust-guidelines/SKILL.md` is the
  index of which file to read when. Start with `11_universal_guidelines.md` (all
  Rust work); add `02_application_*` (CLI/desktop, error handling), `03_correctness_*`,
  `06`/`12`/`13`/`14`/`15` (library design) as the task warrants. Key points:
  `M-CANONICAL-DOCS` doc format, short names, documented magic values,
  `#[expect(..., reason = "...")]` over `#[allow]`. Files that fully comply carry
  a `// Rust guideline compliant <date>` marker — keep it accurate when you edit.
- **Errors:** typed errors with `thiserror` per crate (`CoreError`, `GitHubError`,
  `ConfigError`, …). Handle specific variants; avoid bare catch-alls. Library
  crates set `#![forbid(unsafe_code)]`; binaries deny with localized opt-in.
- **Config fails fast.** Validate required config at load and return a typed
  error; do not invent silent defaults for required values (sensible documented
  platform defaults like `notify-send` are fine, as named constants).
- **No hardcoded magic values.** Use named constants with a rationale comment.
- **Secrets never enter code, logs, errors, or agent context.** Linear tokens are
  read per-call from the keyring; `gh` output is redacted before it enters an
  error; types holding secrets get hand-written redacting `Debug`. Never read
  `.env*`, key/cert files, or print token values. Keep this posture.
- **Headless/view split:** put state and I/O logic in `gui-core` (testable, no
  Iced); keep `gui` a thin view + task-wrapping layer. Same spirit elsewhere —
  shared logic goes in a library crate, not a binary.
- **Tests for all new logic.** Unit tests inline (`#[cfg(test)]`) for private
  behavior; `tests/` for integration. The protocol/state machines have rich
  test suites — extend them rather than adding untested branches.
- **Keep the assistant knowledge bundle current.** `docs/knowledge/` is the
  hand-authored source for the Universal Pohunek Assistant (materialized via
  `assistant.materialize`). Whenever a change alters something the bundle
  describes — a CLI command or flag, a protocol method/event, GUI behavior, an
  operating-model concept (sessions/projects/worktrees/agent profiles), a safety
  rule, the public-API surface in `docs/public-api.md`, or a path listed in
  `docs/knowledge/assistant/source-map.md` — update the matching knowledge
  file(s) in the *same* change and re-run `cargo xtask docs check`. A stale
  bundle is treated like stale code, not a follow-up.
- **Keep shell completion synchronized with the CLI.** The clap command tree is
  the source of truth for generated Bash, Zsh, and Fish completion; never
  hand-maintain generated shell scripts. Every command, subcommand, flag, or
  argument change must also review `crates/cli/src/completion.rs`, update any
  affected dynamic value completers (especially `--host` and session `target`
  arguments), and extend completion/parser tests. Preserve the completion
  safety contract: static generation performs no daemon or NetBird I/O, while
  dynamic lookups are opt-in, deadline-bounded, do not autostart the daemon, and
  fail silently. In the same change, update the CLI tables/examples in
  `README.md`, matching `docs/knowledge/` guidance and source map entries, and
  release/install coverage when those surfaces change. Run at least
  `cargo test -p pohunek-cli` and `cargo xtask docs check` before the full gates.
- Comments and all repository text are in **English**.

## Workflow

- Work on a branch off `main`; do not commit directly to `main`. Commit/push
  only when the user asks.
- **Milestones run in worktrees against `NEXT.md`.** Development moves one
  milestone at a time. `NEXT.md` (repo root) is the **transient** spec for the
  current milestone: it holds the scope and a testable definition-of-done, is
  **not committed**, and is deleted/replaced once the milestone lands. The loop
  is: plan the phase into `NEXT.md` → implement it in a fresh worktree off `main`
  → review the branch against `NEXT.md`'s DoD → merge to `main`, delete the
  branch/worktree, write the next `NEXT.md`. Longer-lived design docs live under
  `docs/design/`, not `NEXT.md`.
- **Plans are end-to-end complete.** Do not propose or build PoCs, minimal
  versions, or phased-minimal shortcuts unless the user explicitly asks for
  reduced scope. Plan and implement the full solution.
- **Commits are never signed.** Use clean, concise, English messages. Do not add
  a `Co-Authored-By` trailer or any "generated with" footer.
- Keep changes scoped. If you touch the wire protocol (`crates/protocol`), expect
  ripples in `client`, `daemon`, `cli`, and `gui-core` — update and test all.
- Run the full gate set above before declaring done. Report failures honestly
  with output; never claim green without running it.
- When a task spans 3+ steps, plan first and verify after each major step.

## Pointers

- `docs/architecture.md` — authoritative design and scope (read this first).
- `docs/ROADMAP.md`, `docs/phases/` — direction and historical context.
- `docs/public-api.md` — the SDK/CLI contract surface.
- `docs/knowledge/` — offline knowledge source built by `cargo xtask docs`.
- `docs/gui-review.md` — current GUI review findings and refactor backlog.
- `.agents/rust-guidelines/` — vendored Microsoft Pragmatic Rust Guidelines
  (read before editing `.rs`; `SKILL.md` routes you to the right file,
  `VENDORED.md` documents the source and how to re-sync).
- `README.md` — install, quick start, trust boundary.

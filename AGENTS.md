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
- **PTY/TUI-first.** Agents run in real terminals (Codex and Claude Code are
  both first-class). Not a re-rendered control plane.
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
| `crates/client`   | SDK client: typed errors, daemon transport. |
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
| `crates/gui-core` | Pure, headless state + SDK bridge for the GUI (no Iced dependency; fully unit-testable). |
| `crates/gui`      | Native Iced shell that wraps `gui-core` in `Task`/`Subscription`. |
| `crates/xtask`    | Workspace automation (docs build/validate/check). |
| `web/`            | Bun workspace: generated protocol types, SDK runtime, control-center backend/client core/SPA, and testkit. |

Other top-level: `docs/` (architecture, roadmap, phases, knowledge source),
`scripts/` (rofi/sway launchers, release helper).

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
`pohunekd`:

```bash
POHUNEK_E2E=1 POHUNEK_DAEMON_BIN=/absolute/path/to/target/debug/pohunekd \
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

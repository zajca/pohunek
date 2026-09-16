# Task: Implement the pohunek milestone specified in NEXT.md

Implement the current pohunek milestone end to end. The single authoritative
spec is `NEXT.md` at the workspace root. Work happens in the milestone
worktree passed to the harness as `--workspace`; never touch any other
checkout. Nothing may be committed, merged, or pushed by this run.

## Requirements

- R1. Implement every definition-of-done item in `NEXT.md`, in full. No PoC,
  no minimal versions, no stubs, no placeholders. If a DoD item is ambiguous
  or `NEXT.md` is missing, stop and ask instead of inventing scope.
- R2. The full gate set (below) passes on the final state of the worktree.
  A gate is green only when its command exits 0.
- R3. When a change touches anything the assistant knowledge bundle describes
  (CLI command/flag, protocol method/event, GUI behavior, operating-model
  concept, safety rule, `docs/public-api.md`, a path in
  `docs/knowledge/assistant/source-map.md`), update the matching
  `docs/knowledge/` file in the same change. A stale bundle is stale code.
- R4. When the wire protocol (`crates/protocol`) changes, update and test the
  ripples in `client`, `daemon`, `cli`, and `gui-core`, and run
  `cargo xtask ts check` (regenerate with `cargo xtask ts generate` if drift).
- R5. The worktree ends with all milestone work present as file changes.
  Do not run `git commit`, `git merge`, `git push`, or `scripts/release`.

## Operating protocol

- The manager slices the NEXT.md DoD into bounded subtask contracts in
  dependency order. One contract covers one DoD item or a coherent slice of
  it, sized to finish inside one executor episode. Each contract carries its
  goal, acceptance criteria, boundary constraints, and the relevant files.
- The executor implements one contract per episode inside the worktree and
  verifies its own work with the narrowest applicable loop (single-crate
  test/clippy) before returning. An execution report is a claim, not
  evidence.
- The executor report must carry the exact commands it ran with their exit
  codes; for the final gate run, include the decisive tail lines of each
  command's output so the auditor can cross-check without re-deriving the
  whole log.
- The auditor independently verifies each completed contract against its
  acceptance criteria and against the repo's definition of done: produce a
  per-DoD-item verdict (met / partial / missing) with concrete `path:line`
  evidence, then run the gate set. Running build/test commands is permitted
  inspection; mutating any tracked file is not. Never mark progress from the
  executor's claim alone — only from evidence you gathered yourself.
- Progress records advance only on clean audit evidence. A failed gate or a
  `partial`/`missing` verdict sends the item back as a new bounded contract.

### Gate economy

Run the full gate set once, on the final state of the milestone, as the
closing verification. Between rounds, verify only with the narrowest
applicable loop for the current contract. Re-running the full set is
warranted only by a concrete failure to fix or an explicit manager
instruction — an unchanged state needs no second full run, and an auditor who
wants stronger evidence checks a bounded sample, not the whole set again.

### Reconciling a previously interrupted run

The worktree may already contain uncommitted work from an earlier
interrupted run. Verify that work against `NEXT.md`, keep what is correct,
and state plainly what was reused. Git forensics (`git fsck`, dangling-object
analysis, ref archaeology) is a means to reconcile the tree, never a goal:
time-box it to the minimum needed to know what the tree contains, and never
spend an episode investigating it beyond that.

## Mandatory repo rules (all roles)

- Read `AGENTS.md` at the workspace root before the first episode; it is
  binding. Before creating or modifying any `.rs` file, read the applicable
  files from `.agents/rust-guidelines/` (start with `SKILL.md`, always
  include `11_universal_guidelines.md`).
- Typed errors with `thiserror` per crate; handle specific variants. No
  hardcoded magic values — named constants with a rationale comment.
- `#![forbid(unsafe_code)]` in library crates stays. `strict_types` and
  existing code style are not negotiable. Follow existing patterns; do not
  add backward-compatibility shims.
- Tests for all new logic: inline `#[cfg(test)]` for private behavior,
  `tests/` for integration. Extend existing protocol/state-machine suites.
- Config fails fast with a typed error; no silent defaults for required
  values.
- All repository text (code, comments, docs) in English.
- Security: never read `.env*`, key/cert/credential files, or the keyring.
  No secret value may enter code, logs, errors, or reports. Never echo
  environment variable values.
- Keep changes scoped to the milestone. If you spot unrelated dead code,
  flag it in the report; do not delete or refactor it.

## Gate set (run from the worktree root, in this order)

The full set mirrors CI exactly (see `AGENTS.md`). Run it in full, in this
order, as the closing verification of the milestone:

```bash
cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --all-features
cargo build -p pohunek-session-worker --bin pohunek-sessiond
cargo test --workspace --all-features
cargo build --workspace --release
RUSTFLAGS="-D warnings" cargo xtask docs check
cargo xtask hermes compatibility --pohunek-bin ABS
```

Web workspace gates (from the worktree's `web/`):

```bash
bun install --frozen-lockfile
bun run typecheck
bun run lint
bun test
bunx playwright install --with-deps chromium   # once per fresh environment
bun run test:e2e
```

Real-daemon web suite (mandatory for done, from the worktree root after
building the three binaries into the same `target/` as the gates above):

```bash
cargo build -p pohunek-daemon -p pohunek-session-worker -p pohunek-cli
POHUNEK_E2E=1 POHUNEK_DAEMON_BIN=<abs>/target/debug/pohunekd \
  POHUNEK_CLI_BIN=<abs>/target/debug/pohunek \
  POHUNEK_PYTHON_BIN=/usr/bin/python3 \
  bun test sdk/test/e2e.test.ts backend/test/real-daemon.e2e.test.ts
```

- A gate is green only when its command exits 0. A milestone is never green
  on a subset of this list: if a command genuinely cannot run in this
  environment (for example a missing pinned Hermes executable), that is a
  failed gate to report, never a silent skip.
- When the change touches dependencies or feature flags, also run the extra
  CI jobs: `cargo audit`,
  `cargo hack --feature-powerset --workspace clippy --all-targets`,
  `cargo udeps`.
- While iterating, narrower loops are fine (`cargo test -p <crate>`,
  `cargo clippy -p <crate> --all-targets`), but a run is not done until the
  full set above passes in order.
- Writing to `target/` while running tests or builds is expected and is not
  a workspace mutation.

## Completion

The run is done only when the auditor has verified, with evidence gathered
from the environment, that every DoD item is met and every gate in the set
above exited 0 on the final state. The closing reply must list each DoD item
with its verdict and `path:line` evidence, the gate results, and any open
gaps or blocked items.
---
name: gates
description: >-
  Run the pohunek CI gate set locally and report the results honestly. Use
  before declaring any Rust change done, when a branch must be verified as
  building, or whenever a run of the repository gate set is requested. This
  is the shared verification block the milestone, milestone-review,
  merge-advance, and release skills all rely on.
---

# gates — run the CI gate set

CI is the source of truth for this repo. This skill mirrors the exact gate
set AGENTS.md ("Build, test, lint — the gates that must pass") mirrors from
CI, in the same order, so a local pass lines up with what CI will run. Never
claim a gate passed without running it; report failures with the real command
output. AGENTS.md lists the full, current gate set — including SDK workspace
gates, the real-daemon suite, and `cargo xtask hermes compatibility` — and
always wins over this summary.

## Core gate set

Run from the workspace root. `clippy` and `docs check` run with warnings as
errors, mirroring CI.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features   # under -D warnings
cargo build -p pohunek-session-worker --bin pohunek-sessiond
cargo nextest run --profile ci --test-threads 4 --workspace --all-features
cargo test --doc --workspace --all-features             # nextest excludes doctests
cargo build --workspace --release
cargo xtask docs check
cargo xtask hermes compatibility --pohunek-bin /abs/path/to/pohunek
```

A local pass does not *guarantee* CI green — shard partitioning, macOS jobs,
and CI-only services make CI the arbiter — but these are the same commands CI
runs. Skipping a command is never silently green: a gate that genuinely
cannot run in this environment (missing pinned Hermes executable) is a failed/skipped gate to report explicitly, with the
reason.

`cargo xtask docs check` validates the assistant knowledge bundle (schema,
drift, source-map, runbooks, secret scan, release extras). Run it for every
change; it is mandatory when the change touches anything under `docs/knowledge/`,
a CLI command/flag, a protocol method/event, or `docs/public-api.md`.

## When the change touches the SDK workspace

Run from the repository root (see AGENTS.md for the full commands): `bun install
--frozen-lockfile`, `bun run typecheck`, `bun run lint`, `bun test`, and
`bun test sdk/ts/scripts` (the SDK release pack contract). The real-daemon suite runs
with the built `pohunekd`/`pohunek-sessiond`/`pohunek` binaries when the
change can affect daemon/worker behavior.

## How to run it

1. Run each command in order. Stop reporting a step as green only after it
   exits 0.
2. If a step fails, capture the failing output and fix the cause (or delegate
   the fix). Re-run the failed step and every step whose inputs the fix
   changed; a step whose inputs are unchanged keeps its result, and a fix to
   workspace-wide inputs (`Cargo.toml`, `Cargo.lock`, `.config/nextest.toml`,
   the CI workflow) re-runs the whole set. The reported set must hold for the
   final revision: every step's evidence was produced on inputs identical to
   it (AGENTS.md "Testing policy").
3. Report a compact status per gate (pass / fail + first failing lines, or
   explicitly skipped with why). Say plainly what actually ran. If all pass,
   say so plainly; if any fail or were skipped, say which and why, with output.

## Narrower loops while iterating

Use these to shorten the feedback loop (see AGENTS.md "Fast loops" —
`cargo t`, `cargo tw`, `python3 scripts/test-partitions run <shard>`),
but always finish with the full set above.

The default inner-loop command is `cargo t`. `cargo ta` (`cargo xtask
affected`) is the CPU-saving option when several agents or worktrees share
the host: the fast tests of the crates the branch and working tree touch,
plus their dependents, escalating to everything for workspace-wide or unmapped paths.
`cargo ta --print` shows the per-file reasons. It is never a substitute for
any gate above.

## Extra CI jobs (only when your change touches deps/features)

```bash
cargo audit
cargo hack --feature-powerset --workspace clippy --all-targets
cargo shear --locked --deny-warnings                           # stable; PR gate
cargo +nightly udeps --workspace --all-targets --all-features  # optional; CI: main/weekly
```

Note: `knowledge` gates its protocol bridge behind a `protocol` feature, so
`--all-features` only covers the everything-on case — the feature-powerset job
is what catches a broken feature combination.

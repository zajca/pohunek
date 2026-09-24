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
output. AGENTS.md lists the full, current gate set — including web workspace
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
cannot run in this environment (missing pinned Hermes executable, no
Playwright browsers) is a failed/skipped gate to report explicitly, with the
reason.

`cargo xtask docs check` validates the assistant knowledge bundle (schema,
drift, source-map, runbooks, secret scan, release extras). Run it for every
change; it is mandatory when the change touches anything under `docs/knowledge/`,
a CLI command/flag, a protocol method/event, GUI behavior, or `docs/public-api.md`.

## When the change touches the web workspace

Run in `web/` (see AGENTS.md for the full commands): `bun install
--frozen-lockfile`, `bun run typecheck`, `bun run lint`, `bun test`, and, for
changes covered by the e2e suite, `bun run test:e2e` (after
`bunx playwright install --with-deps chromium`). The real-daemon suite runs
with the built `pohunekd`/`pohunek-sessiond`/`pohunek` binaries when the
change can affect daemon/worker behavior.

## How to run it

1. Run each command in order. Stop reporting a step as green only after it
   exits 0.
2. If a step fails, capture the failing output, fix the cause (or delegate the
   fix), and re-run the whole set — do not skip a step because it "passed last
   time".
3. Report a compact status per gate (pass / fail + first failing lines, or
   explicitly skipped with why). Say plainly what actually ran. If all pass,
   say so plainly; if any fail or were skipped, say which and why, with output.

## Narrower loops while iterating

Use these to shorten the feedback loop (see AGENTS.md "Fast loops" —
`cargo t`, `cargo tw`, `python3 scripts/test-partitions run <shard>`),
but always finish with the full set above.

## Extra CI jobs (only when your change touches deps/features)

```bash
cargo audit
cargo hack --feature-powerset --workspace clippy --all-targets
cargo udeps --workspace --all-targets --all-features   # needs nightly
```

Note: `knowledge` gates its protocol bridge behind a `protocol` feature, so
`--all-features` only covers the everything-on case — the feature-powerset job
is what catches a broken feature combination.

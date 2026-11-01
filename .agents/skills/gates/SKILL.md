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

CI is the source of truth for this repo. This skill mirrors the repository
verification guide (`.agents/instructions/verification.md`) and the current CI gate set, in the
same order, so a local pass lines up with what CI will run. Never claim a gate
passed without running it; report failures with the real command output. The
verification guide is the current inventory — including SDK workspace gates,
the real-daemon suite, and `cargo xtask hermes compatibility` — and always wins
over this procedural summary.

## Core gate set

Run from the workspace root. `clippy` and `docs check` run with warnings as
errors, mirroring CI. Run them on the CI toolchain: with the rustup proxies
(`$CARGO_HOME/bin`, default `~/.cargo/bin`) first on PATH, rustup selects the
`rust-toolchain.toml` pin for `rustc`, `clippy` and `rustfmt` alike. A
distribution toolchain earlier on PATH ignores the pin, so its clippy and fmt
results are not gate evidence; the `rustc-pin` row of `scripts/dev-bootstrap`
flags a differing `rustc`.

```bash
cargo fmt --all --check
scripts/check-rust-toolchain-pin                        # no floating Rust channel in CI
cargo clippy --workspace --all-targets --all-features   # under -D warnings
scripts/darwin-check                                    # macOS-only code; needs zig on a Linux host
cargo build -p pohunek-session-worker --bin pohunek-sessiond
cargo nextest run --profile ci --test-threads 4 --workspace --all-features
cargo test --doc --workspace --all-features             # nextest excludes doctests
cargo build --workspace --release
cargo xtask docs check
scripts/build-hermes-package /abs/path/to/hermes.tar.zst
cargo xtask hermes compatibility --bin-dir /abs/path/to/target/release \
  --package /abs/path/to/hermes.tar.zst
```

A local pass does not *guarantee* CI green — shard partitioning, macOS jobs,
and CI-only services make CI the arbiter — but these are the same commands CI
runs. Skipping a command is never silently green: a gate that genuinely
cannot run in this environment (missing pinned Hermes executable) is a failed/skipped gate to report explicitly, with the
reason.

`cargo xtask docs check` validates the assistant knowledge bundle (schema,
knowledge-log fragments, drift, source-map, the shape of the assistant prompt
contract, runbooks, secret scan, release extras, command examples against the clap tree, the crate table, the package
lists and the documentation index). Run it for every
change; it is mandatory when the change touches anything under `docs/knowledge/`,
a CLI command/flag, a protocol method/event, or `docs/public-api.md`.

## Stress the changed tests

Mandatory when the diff (committed, staged or unstaged) adds or changes a Rust
test in a heavy, relay-db or macOS-lane package or file: any test outside the
`profile.fast` default-filter in `.config/nextest.toml` (the complement of its
`not ( ... )` expression is the heavy partition). Run it after the core gate
set passes and before push, with the profile that owns the tests:

```bash
scripts/stress-changed-tests --base origin/main --count 50 --deadline 45m --profile heavy
scripts/stress-changed-tests --base origin/main --count 20 --deadline 45m --profile relay-db   # needs the relay database fixture
scripts/stress-changed-tests --base origin/main --count 50 --deadline 45m --profile macos --unpinned   # macOS hosts
```

It builds the test binaries first (`cargo nextest list`), then runs exactly the
changed tests N times under `scripts/stress-run` (4 pinned CPUs, no retries).
Counts are lane suggestions, not defaults: 50 for heavy, 20 for relay-db; pick
the deadline from the measured cost of one iteration. On macOS there is no CPU
affinity, so the run needs `--unpinned` and the contention is host-wide; say so
in the report. Both cargo calls share one `-p` package set, taken from the full
selection, so the run does not rebuild with a different feature unification.
They build with `--all-features`, like CI's Linux test archive, so
feature-gated tests (relay `postgres-tests`) run. CI's macOS lane and tier
archives build default features, so under `--profile macos` the step builds
default features too: a test that exists only behind a Cargo feature is not in
the list, because CI's macOS lane never builds it either. Without
`--allow-missing-on-host` it fails by name; with the flag it is reported NOT
STRESSED and belongs to the Linux run that builds it with `--all-features`.
`--print` shows the selection without running anything. A test
the script cannot map or list fails the step by name; a `macro_rules!` that
generates tests fails by the macro's name (stress the whole partition then). A
diff that changes tests for two operating systems cannot pass on one host: run
the step on each host with `--allow-missing-on-host`, which names every test the
host does not build as NOT STRESSED, and carry those names to the host that
builds them in the report. The flag excuses only a test with a non-`test` `cfg`
on its declaring chain (its own attributes, enclosing inline modules, the
file's `#![cfg]`, the `mod x;` declarations above it); a missing test under
plain `#[cfg(test)]` still fails.

A failure is a FLAKY result under the flaky-test policy: open or reuse an issue
with the root cause, fix the test or the product race, and run the step again.
Never retry the step to green and never lower `--count` to pass. A diff that
adds no test in those lanes skips the step, and the report says so.

## When the change touches the SDK workspace

Run from the repository root (see `.agents/instructions/verification.md` for
the full commands): `bun install
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
   it (testing policy (`.agents/instructions/verification.md#testing-policy`)).
3. Report a compact status per gate (pass / fail + first failing lines, or
   explicitly skipped with why). Say plainly what actually ran. If all pass,
   say so plainly; if any fail or were skipped, say which and why, with output.

## Narrower loops while iterating

Use these to shorten the feedback loop (see
`.agents/instructions/verification.md` "Fast loops" —
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
cargo +nightly-YYYY-MM-DD udeps --workspace --all-targets --all-features  # optional; CI: main/nightly
```

The udeps date is the pinned nightly: the `toolchain: nightly-YYYY-MM-DD`
input of the `cargo-udeps` job in `.github/workflows/ci.yml`.

Note: `knowledge` gates its protocol bridge behind a `protocol` feature, so
`--all-features` only covers the everything-on case — the feature-powerset job
is what catches a broken feature combination.

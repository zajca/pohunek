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
contract, runbooks, secret scan, release extras, command examples against the
clap tree, the crate table, the package lists and the documentation index). Run
it for every change; it is mandatory when the change touches anything under
`docs/knowledge/`, a CLI command/flag, a protocol method/event, or
`docs/public-api.md`.

## Stress the changed tests

Mandatory when the diff (committed, staged or unstaged) adds or changes a Rust
test in a heavy or macOS-lane package or file: any test outside the
`profile.fast` default-filter in `.config/nextest.toml` (the complement of its
`not ( ... )` expression is the heavy partition). Run it after the core gate
set passes and before push, with the profile that owns the tests:

```bash
scripts/stress-changed-tests --base origin/main --count 50 --deadline 45m --profile heavy
scripts/stress-changed-tests --base origin/main --count 50 --deadline 45m --profile macos --unpinned   # macOS hosts
```

It builds the test binaries first (`cargo nextest list`), then runs exactly the
changed tests N times under `scripts/stress-run` (4 pinned CPUs, no retries).
Counts are lane suggestions, not defaults: 50 for heavy; pick
the deadline from the measured cost of one iteration. On macOS there is no CPU
affinity, so the run needs `--unpinned` and the contention is host-wide; say so
in the report. Both cargo calls share one `-p` package set, taken from the full
selection, so the run does not rebuild with a different feature unification.
They build with `--all-features`, like CI's Linux test archive, so
feature-gated tests run. CI's macOS lane and tier
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

## Receipts

`scripts/gate-receipt run --gate <name> --max-age 24h -- <command>` wraps one
canonical gate line so a second run on an unchanged tree skips the command.
The first run executes the command and writes a receipt; the second verifies
the receipt and exits 0 without running it. A receipt is local convenience,
never CI evidence: CI and the Opus review marker stay the landing evidence.

Wrap each gate with its canonical line, identical across sessions so the key
matches (the argv after `--` is part of the key):

```bash
scripts/gate-receipt run --gate fmt --max-age 24h -- /tmp/heavy-run.sh cargo fmt --all --check
scripts/gate-receipt run --gate toolchain-pin --max-age 24h -- /tmp/heavy-run.sh scripts/check-rust-toolchain-pin
scripts/gate-receipt run --gate clippy --max-age 24h -- /tmp/heavy-run.sh cargo clippy --workspace --all-targets --all-features
scripts/gate-receipt run --gate darwin --max-age 24h -- /tmp/heavy-run.sh scripts/darwin-check
scripts/gate-receipt run --gate nextest --max-age 24h -- /tmp/heavy-run.sh cargo nextest run --profile ci --test-threads 4 --workspace --all-features
scripts/gate-receipt run --gate doctests --max-age 24h -- /tmp/heavy-run.sh cargo test --doc --workspace --all-features
scripts/gate-receipt run --gate docs --max-age 24h -- /tmp/heavy-run.sh cargo xtask docs check
```

`gate-receipt` strips the daemon's session runtime names from the wrapped
command's environment and keys no session runtime name, so no unsetting is
needed around the invocation and receipts are shared across sessions.
Every other `POHUNEK_*` name is contributor or CI configuration (for
example `POHUNEK_PYTHON_BIN`, which selects the Hermes test interpreter):
it passes through to the gate unchanged and is keyed by hash, so a changed
value reruns the gate.

The final gate before a push may use a receipt only when that receipt was
written for the exact tree being pushed; a receipt never carries a gate
across a changed tree.

24h matches the lane rhythm: a receipt written during the day's work is still
fresh for the evening's final gate or the next morning's re-run, while a
tree that sits longer than a day re-runs its gates instead of trusting stale
proof from a toolchain or dependency world that may have moved.

A receipt is not accepted — the command runs again — when any of these holds:

- No receipt file exists for the key, or it is not valid JSON of the known schema.
- The key differs: a changed working tree (tracked, staged, unstaged, or
  untracked-but-not-ignored files), a different gate name or argv, a
  different working directory, different `rustc -vV`/`cargo -vV` output, a
  different command-file hash, a different `CARGO_HOME` location, extra
  compiler flags or wrappers (`RUSTC`, `RUSTC_WRAPPER`,
  `RUSTC_WORKSPACE_WRAPPER`, `CARGO_ENCODED_RUSTFLAGS`, `RUSTC_BOOTSTRAP`),
  or different tracked environment hashes. The daemon's session runtime
  names are never keyed, so a second session reuses the receipt; every
  other `POHUNEK_*` name is configuration and is keyed, so a changed
  contributor setting reruns the gate.
- It is older than `--max-age`, or it ends more than 60s in the future
  (beyond the `CLOCK_SKEW_SECONDS` clock skew bound).
- It is not owned by the current uid, or it is readable or writable by
  group or others.
- The store directory is not owned by the current uid, or is not mode 0700.
  A wrong store mode is never repaired silently: both `run` and `check`
  refuse the receipt, `run` runs the command and writes no receipt.
- Ignored inputs are out of scope by construction: `target/`, local `.env`
  files, and other ignored paths never enter the tree id, so changing only
  them never invalidates a receipt.
- A gate that needs the network or external state is never wrapped: the
  two Hermes gates (`scripts/build-hermes-package`,
  `cargo xtask hermes compatibility`) and `cargo audit` re-run every time.
  Their inputs live outside the key — the package tarball, the `--bin-dir`
  build outputs, the remote registry — so a receipt could skip a gate whose
  real inputs have moved.
- A gate whose output under an ignored path feeds a later gate is never
  wrapped: `bins` builds `pohunek-sessiond`, which the nextest daemon tests
  spawn from beside the test binaries, and `release-build` fills
  `target/release`, which the Hermes compatibility gate reads through
  `--bin-dir`. Both outputs live under the ignored `target/`, outside the
  key, so a skipped producer would leave a missing or stale artifact that a
  later gate consumes. Build them every time.

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

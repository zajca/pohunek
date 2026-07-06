---
name: gates
description: >-
  Run the pohunek CI gate set locally and report the results honestly. Use
  before declaring any Rust change done, and whenever the user says "run the
  gates", "run CI", "zkontroluj že projde CI", "spusť testy a clippy", "je to
  zelené?", or asks to verify a branch builds. This is the shared verification
  block the milestone, milestone-review, merge-advance, and release skills all
  rely on.
---

# gates — run the CI gate set

CI is the source of truth for this repo. This skill runs the exact gate set CI
runs, in the same order, so a local pass means CI will pass. Never claim a gate
passed without running it; report failures with the real command output.

## The gate set

Run from the workspace root. `clippy` and `docs check` run under `-D warnings`
(a warning is a failure), mirroring `.github/workflows/ci.yml`.

```bash
cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
cargo build --workspace --release
RUSTFLAGS="-D warnings" cargo xtask docs check
```

`cargo xtask docs check` validates the assistant knowledge bundle (schema,
drift, source-map, runbooks, secret scan, release extras). Run it for every
change; it is mandatory when the change touches anything under `docs/knowledge/`,
a CLI command/flag, a protocol method/event, GUI behavior, or `docs/public-api.md`.

## How to run it

1. Run each command in order. Stop reporting a step as green only after it
   exits 0.
2. If a step fails, capture the failing output, fix the cause (or delegate the
   fix), and re-run the whole set — do not skip a step because it "passed last
   time".
3. Report a compact status per gate (pass / fail + first failing lines). If all
   five pass, say so plainly. If any fail, say which and why, with output.

## Narrower loops while iterating

Use these to shorten the feedback loop, but always finish with the full set above:

```bash
cargo test -p pohunek-gui-core                # one crate
cargo test -p pohunek-cli some_test_name      # one test
cargo clippy -p pohunek-daemon --all-targets  # lint one crate
```

## Extra CI jobs (only when your change touches deps/features)

CI also runs these; run them locally if your change adds/removes dependencies or
features, otherwise they are not part of the standard loop:

```bash
cargo audit
cargo hack --feature-powerset --workspace clippy --all-targets
cargo udeps --workspace --all-targets --all-features   # needs nightly
```

Note: `knowledge` gates its protocol bridge behind a `protocol` feature, so
`--all-features` only covers the everything-on case — the feature-powerset job
is what catches a broken feature combination.

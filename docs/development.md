# Development

Workspace layout, the CI gate set, fast test loops, and the release pipeline.
[AGENTS.md](../AGENTS.md) remains the canonical contributor guide.

The workspace is a Cargo monorepo (edition 2021, MSRV 1.96) plus a Bun
workspace (rooted at the repository root, spanning `sdk/ts/*`) for the TypeScript packages.

| Crate | Role |
|-------|------|
| `crates/protocol` | Wire contract: envelopes, methods, events, version negotiation. |
| `crates/package`| Canonical runtime package archive: deterministic `tar.zst` builder, strict reader, size limits, package digest, and the signed runtime catalog verifier. |
| `crates/client` | Rust SDK: typed errors, transports, attach helpers. |
| `crates/daemon` | `pohunekd`: public control plane, logical session registry, reconciliation, detection, notifications. |
| `crates/worker-protocol` | Versioned owner-private daemon-to-worker protocol and framing. |
| `crates/session-worker` | `pohunek-sessiond`: one durable PTY runtime owner per live session. |
| `crates/cli` | `pohunek`: every command over the control protocol. |
| `crates/prompt` | Shared prompt rendering + `link.*` metadata schema (CLI and scripts). |
| `crates/knowledge` | Knowledge-bundle primitives for the assistant and offline docs. |
| `crates/terminal` | VT screen tracking and attach compositing. |
| `crates/netbird` | NetBird status parsing, host resolution, bind validation, and overlay adapter. |
| `crates/overlay` | Provider-neutral overlay contract, configured registry, and per-overlay routing. |
| `crates/paths` / `crates/hostcheck` | XDG/socket contract; host environment probes. |
| `crates/xtask` | Workspace automation: docs build/check, TS type generation. |
| `sdk/ts/` | `@pohunek/protocol` (`sdk/ts/protocol`), `@pohunek/sdk` (`sdk/ts/sdk`), `@pohunek/testkit` (`sdk/ts/testkit`). |

Read **[AGENTS.md](../AGENTS.md)** first — it is the canonical contributor guide.
Authoritative design lives in [docs/architecture.md](architecture.md);
the protocol contract in [docs/public-api.md](public-api.md).

## Gates

CI treats warnings as errors. Run the full set before calling anything done:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features
cargo build -p pohunek-session-worker --bin pohunek-sessiond  # daemon tests spawn it by path
cargo nextest run --profile ci --test-threads 4 --workspace --all-features
cargo test --doc --workspace --all-features  # nextest excludes doctests
cargo build --workspace --release
cargo xtask docs check          # knowledge bundle: schema/drift/secrets/runbooks
```

Routine loops (cargo-nextest profiles live in `.config/nextest.toml`; see
`AGENTS.md` "Fast loops" for watcher, and CI-timing variants):

```bash
cargo t                        # all fast unit + integration tests, no PTY/DB fixtures
cargo t -p pohunek-daemon       # fast tests in one crate
cargo ti                       # fast daemon/client/session-worker surface
cargo tw                       # unfiltered full suite, four test processes
bun test sdk/ts/sdk/test/config.test.ts -t "one case"  # one TypeScript test file, name pattern
bacon                          # watcher: profile-fast nextest loop (bacon.toml)
python3 scripts/test-partitions run cli    # exact CI shard (unit/daemon/relay/cli/relay-db/heavy)
python3 scripts/test-partitions check      # verify all tests belong to exactly one shard
scripts/ci-timings compare --baseline 2026-09-14..2026-09-16 --current 2026-09-20..2026-09-21 \
    --event pull_request --conclusion success   # reproduce CI timing evidence
```

Requires cargo-nextest >= 0.9.131 (`flaky-result`) and Python >= 3.11
for the shard helper. `profile.fast.default-filter` is the cost boundary;
`scripts/test-partitions` subdivides it by package ownership, with an exact
complement for heavy tests. Whole fixture-owning modules stay heavy: real
PTY/worker lifecycle, PostgreSQL, Hermes subprocess suites,
and nested Cargo integration checks. Fast includes short filesystem, mock-socket,
and CLI integration tests; `--lib` alone is **not** a cost boundary.

CI runs four fast matrix shards and separate heavy jobs without waiting for
Clippy or release builds. Heavy uses four test processes and the real worker
binary. Relay heavy tests (the only PostgreSQL fixture consumers) run in a
dedicated `relay-db` job whose `POHUNEK_RELAY_TEST_DATABASE_URL` points at a
disposable PostgreSQL service; a `paths-filter` job skips that job — including
the Postgres service — when no relay-relevant file (`crates/relay*/**`,
`migrations/**`, `Cargo.lock`, the CI workflow) changed. The full `ci`/`local` profiles remain unfiltered. Existing ignored tests retain
their opt-in status. Each CI shard uploads a uniquely named JUnit artifact;
sequential local fast shards overwrite `target/nextest/ci/junit.xml`, while heavy
writes `target/nextest/heavy/junit.xml`.

The target is fast feedback in about two minutes for routine changes, not a
promise for cold builds or the complete required CI suite. Review JUnit execution
times separately from compile/setup time; validating the 90%-of-changes target
requires representative CI history. When adding a costly fixture, update the
cost filter, run the partition coverage check, and measure the fast loop again.

SDK workspace:

```bash
bun install --frozen-lockfile
bun run typecheck && bun run lint && bun test
```

`bun run typecheck` is one command: `tsc -b` over the composite (source-only)
`protocol → sdk → testkit` graph (incremental via
`.tsbuildinfo` in each `dist-types/`), then the standalone checks — the SDK
release scripts and the per-package `test/tsconfig.json`
projects — run concurrently. Tests stay out of the composite graph because
their imports of sibling packages resolve to `.ts` sources and would otherwise
be pulled into non-referenced projects (and form reference cycles). Each
package also supports `tsc -b` on its own (`cd sdk/ts/sdk && bun run typecheck`,
which also typechecks that package's tests). Per-package `dist-types/` output
is git-ignored.

Stale per-branch Cargo target isolation directories (`target/<name>` shaped
like a Cargo target dir, untouched for 30+ days) are removed with:

```bash
scripts/cargo-sweep-targets --dry-run   # preview
scripts/cargo-sweep-targets --days 30   # delete; --cron prints a monthly line
```

Only entries shaped like Cargo target dirs are deleted. `target/debug`,
`target/release`, nextest/doc scratch dirs, manual `pohunek-eval`
transcripts, `doc`/`package` output, and cross-compile triples are kept, and
a `--target-dir` that is not a Cargo target dir refuses to run.

A protocol change is not done until the generated TypeScript types match:

```bash
cargo xtask ts generate   # regenerate sdk/ts/protocol/src/generated/**
cargo xtask ts check      # CI gate
```

## Conventions that matter here

- **Rust guidelines are mandatory.** The Microsoft Pragmatic Rust Guidelines
  are vendored at `.agents/rust-guidelines/`; read the relevant files before
  touching any `.rs` file (`SKILL.md` is the index).
- **Typed errors** (`thiserror`) per crate; no bare catch-alls. Library crates
  `#![forbid(unsafe_code)]`.
- **Config fails fast** — required values are validated at load; no silent
  defaults. No hardcoded magic values.
- **Secrets never enter code, logs, errors, or agent context.** Keyring
  references only; `gh` output is redacted before it can reach an error.
- **Protocol ripples**: touching `crates/protocol` means updating `client`,
  `daemon`, `cli`, the generated TS types, `docs/public-api.md`,
  and the `docs/knowledge/` bundle in the same change.
- **Behavioral tests, not per-function tests** (AGENTS.md "Testing policy"):
  a regression scenario for each bug or lifecycle fix, a component scenario for
  new observable behavior, no coverage target. The protocol and state machines
  have rich suites — extend them.

## Release

`scripts/release` bumps the workspace version, tags `vX.Y.Z`, and pushes; the
Release workflow re-runs the gates on the tag, then builds and publishes glibc
and MUSL x86_64 CLI and daemon archives, and ad-hoc signed `aarch64-apple-darwin`
CLI and daemon archives. No macOS job uses secrets or a protected environment.
A download-only `attest` job, the only job holding `id-token: write` and
`attestations: write`, creates a build-provenance attestation for every
published asset (Linux, macOS, SDK, and the `.sha256` checksum files); the publish jobs depend on it, so an
unattested asset is never published. Developer ID signing and notarization are
not part of the pipeline. The offline docs, the README, and the reference
pages under `docs/` are bundled into every native component archive. CLI archives also contain
`packaging/smoke-hermes-plugin-release`. Release automation provisions the
source-locked Hermes runtime without provider credentials, runs the model-free
compatibility gate, extracts each CLI archive, and executes its packaged smoke
script against the extracted `pohunek` binary. Operators can repeat the same
script with an explicitly supplied, preinstalled pinned Hermes executable. It
creates an isolated temporary profile/state, requires that executable rather
than downloading it, and fails if install, status, doctor, or uninstall cannot
prove the embedded plugin and generated skill.

After a published stable release, `.github/workflows/notify-tap.yml` (a `workflow_run` of `Release`, the only workflow with a secret, `TAP_DISPATCH_PAT`) sends the Homebrew tap `zajca/homebrew-pohunek` a `pohunek-work-release` repository dispatch for the `pohunek` formula and the tag, and the tap bumps it; pre-releases are skipped. `scripts/tests/test_notify_tap_workflow.py` pins it.

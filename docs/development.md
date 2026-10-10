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
cargo t                        # all fast tests, no PTY/DB fixtures
cargo t -p pohunek-daemon       # fast tests in one crate
cargo ti                       # fast daemon/client/session-worker surface
cargo tw                       # unfiltered full suite, four test processes
bun test sdk/ts/sdk/test/config.test.ts -t "one case"  # one TypeScript test file, name pattern
bacon                          # watcher: profile-fast nextest loop (bacon.toml)
python3 scripts/test-partitions run cli    # exact CI shard (core/daemon/relay/cli/relay-db/heavy)
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

## Release consumer suite

`crates/cli/tests/release_consumer.rs` drives ONE runtime per invocation,
entirely out of process, through the exact released binaries. It is what a
release row and the archive smoke run, and it produces the consumer report that
`cargo xtask compat attest` consumes. It never links the daemon and never looks
at `target/` or the source tree, so it runs as a prebuilt executable
(`cargo test -p pohunek-cli --test release_consumer --no-run`).

Four inputs are mandatory and absolute, with no fallback to `CARGO_BIN_EXE_*`;
the smoke inputs are required only in smoke mode. A missing, empty, relative or
non-regular value (a link counts as non-regular) fails naming the variable:

| Variable | Meaning |
| --- | --- |
| `POHUNEK_CONSUMER_BIN_DIR` | directory with `pohunek`, `pohunekd`, `pohunek-sessiond` (and `runtime-catalog-anchor.json` in smoke mode) |
| `POHUNEK_CONSUMER_RUNTIME` | runtime id to exercise (`pi`, `codex`, `claude`, ...) |
| `POHUNEK_CONSUMER_PACKAGE` | package archive from `cargo xtask package build` |
| `POHUNEK_CONSUMER_REPORT` | where the report is written after success |
| `POHUNEK_CONSUMER_CATALOG` | optional; selects smoke mode |
| `POHUNEK_CONSUMER_STAGE_BIN` | required in smoke mode; verified stage's `bin` directory, also first on `PATH` |

With no `POHUNEK_CONSUMER_*` variable set the test skips, which keeps a plain
`cargo test` green. Once any is set, every missing prerequisite (an input, the
upstream on `PATH`, a driver) is a failure, never a skip. A stale report is
removed first, and a failed run leaves none. A report path that is, or aliases
(same path, hard link, symlink, parent-directory symlink), the package, catalog,
anchor or a release binary, or whose `.partial` temporary file does, is refused
before anything is deleted or written; a relative input is resolved against the
working directory for this check, so an input that validation will reject is
protected too. The stale report is removed after that check and before the
inputs are validated, so a run that fails early never leaves the report of an
earlier run behind. Executables are staged through
`pohunek_test_support::fs`, which keeps a write descriptor out of sibling
threads' children (no `ETXTBSY`). Every subprocess the suite waits on (the upstream
and staged `--version` reads, each CLI call) runs in its own process group with
a deadline and a bounded output collection; an overrun kills the group and
fails naming the program, so a hung upstream or a background child holding the
pipes cannot stall a prebuilt executable.

- **Row mode** (no catalog): the harness generates a throwaway Ed25519 root,
  places its anchor beside the copied daemon and signs a schema 2 catalog
  authorizing exactly the given archive for the daemon's own release version.
  The release block and attestation digests of that catalog are placeholders,
  because the real attestation does not exist until this run has produced its
  report. A release row calls it with the binaries extracted from the just-built
  daemon archive, the archive from `cargo xtask package build`, and the locked
  upstream on `PATH`.
- **Smoke mode** (`POHUNEK_CONSUMER_CATALOG` set): nothing is generated. The
  signed catalog is used as given and the archive's own anchor is copied
  byte for byte beside the daemon, so the official install is judged by the
  bundle's real trust. The network-isolated archive smoke calls it this way.

What a run does: copies the three binaries into
`<tmp>/prefix/libexec/pohunek/<version>/` (the installer layout; the version is
what `pohunekd --version` and `pohunek --version` both report) and hashes them;
starts `pohunekd` as a subprocess (worker supervision as direct children) in a
hermetic XDG environment with the upstream's directory in front of `PATH`;
checks through `/proc/<pid>/exe` that the daemon and the worker execute the
layout copies and that the executed image hashes to the installed bytes; runs
every command as the layout's `pohunek` subprocess: `plugin install --catalog`,
`plugin list`/`inspect` (official, enabled, selected), `host inspect local`
(the daemon's version probe must report a supported release equal to a direct
`<upstream> --version` read with the package's own probe grammar), one session
against a loopback model stub through ready, input, working, idle, stop and
resume (after the resume a second prompt is sent and the model stub must see
the first prompt and its reply in that turn's request, which proves the
upstream restored the conversation; where the runtime re-reports through its
hook, a new reporter must appear), then removes the sessions, stops the daemon with SIGTERM and requires
a clean exit and no remaining pohunek process. The bin dir and the layout are
re-hashed before the report is written.

The report (`schema`, `runtime`, `package_digest`, `upstream_version`,
`suite_version`, `executables`) carries the version the daemon actually probed,
the suite version of the `compat/matrix.json` this executable was built with
and the SHA-256 of the three executed images, not strings from the lock.

Prerequisite of the guard scenarios: this build's `pohunekd` and `pohunek-sessiond`
must exist beside the test profile, as for the other daemon-backed tests
(`cargo build -p pohunek-daemon --bin pohunekd` and `cargo build -p
pohunek-session-worker --bin pohunek-sessiond`; CI builds every binary first,
and the failure message names these commands). They fail rather than skip when
a binary is missing.

Guard scenarios (`guards::*`) run in the default test run without an upstream or
a model: the input contract, a swapped binary or bin dir, an executable outside
the layout, an archive the catalog does not name (typed `package_untrusted`), an
upstream version outside the package's range, smoke-mode anchor handling. They
use this build's binaries through an explicit `target_bin_dir` helper, so a run
that executes a prebuilt test executable on a machine without that build
selects the suite by name (`the_release_binaries_serve_the_runtime_out_of_process`).

Adding a driver: a new `runtime-packages/<runtime>/` directory without an entry
in `DRIVERS` (`crates/cli/tests/support/release_drivers.rs`) makes the suite
fail with a message saying so. An entry names the runtime id, how its screen
shows readiness, the marker that makes the model stub hold a reply, whether
the conversation id arrives through a hook, a `prepare` function that writes the
upstream's hermetic configuration (throwaway home or config directory, the
loopback stub as its only model endpoint, every update, telemetry and
marketplace switch off) and returns the profile `[env]`, and the text of the
reply as it appears on screen. Reuse a stub from `crates/cli/tests/support/`
or add one beside them.

## Archive smoke

`packaging/smoke-archive` proves that a final daemon archive from `cargo xtask
release assemble` serves every official runtime with no network and no source
tree. It runs the prebuilt consumer executable in smoke mode, once per runtime:

```sh
cargo test --no-run -p pohunek-cli --test release_consumer   # prints the executable path
packaging/smoke-archive \
  --archive <bundle>/pohunek-daemon-<v>-<target>.tar.gz \
  --stage <stage-dir> \
  --consumer <release_consumer-executable> \
  [--runtime <id>]... [--isolation auto|userns|sudo]
```

`<stage-dir>/<runtime>/` is the staged upstream of each runtime: an npm prefix
with `bin/<binary>` and a `STAGE.sha256` digest list. The runtime set is data:
it is the `runtime/packages/*.tar.zst` the archive ships, and a shipped package
without a staged upstream is a failure, not a skip (`--runtime` only restricts
the set). Steps, each fail-closed:

1. Refuse an archive with absolute, `..`, link or special members, extract it
   into a private temp directory, run the archive's own
   `packaging/verify-archive`, and require the catalog and every package to be
   covered by `MANIFEST` and every package to match its catalog digest.
2. Enter a fresh network and PID namespace, bring loopback up and assert that
   loopback is the only interface and that no default route exists. A process
   that outlives a consumer run fails the run and dies with the namespace.
3. Inside the namespace verify each stage with `sha256sum -c STAGE.sha256`
   (every file listed, no link leaving the stage).
4. Run the consumer copy from a temp working directory with `PATH` =
   `<stage>/<runtime>/bin` + the system node directory, a hermetic
   `HOME`/`XDG_*`, and only the `POHUNEK_CONSUMER_*` variables, pointing into
   the extracted archive (`POHUNEK_CONSUMER_CATALOG` is the archive's signed
   catalog, so its own anchor is the trust). The consumer requires its resolved
   upstream executable to be the entry from `POHUNEK_CONSUMER_STAGE_BIN` and
   to remain inside that verified stage.
5. Require each report's `package_digest` to equal the catalog entry's digest
   and its `executables` to equal the SHA-256 of the archive's binaries.

Isolation strategies, chosen at run time (`--isolation` forces one): `userns`
(`unshare --user --map-root-user --net --pid`, needs unprivileged user
namespaces, which Ubuntu 24.04 blocks through AppArmor unless
`kernel.apparmor_restrict_unprivileged_userns=0`) and `sudo` (`sudo -n unshare
--net --pid`, loopback raised as root, then the invoking uid/gid is restored
with `setpriv` and all capabilities dropped). If neither works the script
fails; there is no un-isolated fallback. It needs Linux, util-linux `unshare`,
iproute2 `ip`, `jq` and `node` on `PATH`.

What it proves: the released bytes, catalog and anchor, the staged upstream
and the packages work together offline, and the consumer saw no path of the
checkout (stage and temp paths inside the checkout are refused, the consumer
executable is copied out of `target/`). The script itself comes from the
checkout and runs outside the namespace until step 2. It does not prove other
platforms: macOS has no network namespaces, so a macOS row is not covered. The
consumer's temp root is kept short (`/tmp/pks.XXXXXX/t`) because the suite
refuses a `TMPDIR` that makes its socket paths too long.

Scenarios: `python3 -m unittest scripts.tests.test_smoke_archive` (synthetic
archive and stub consumers; the `sudo` scenario is skipped where `sudo -n` is
unavailable).

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
- **Integration/E2E tests, not unit tests** (AGENTS.md "Testing policy"): a
  test drives cooperating production components through a supported boundary,
  or the real product processes — never a unit test. Extending an existing
  suite does not exempt an added test from that boundary; the unit-tier
  tests still in the suite are #671 migration work, not everyday additions.
  The protocol and state machines have rich suites — extend them first.

## Release

`scripts/release` bumps the workspace version, tags `vX.Y.Z`, and pushes; the
Release workflow (`.github/workflows/release.yml`) re-runs the gates on the tag
and then builds, assembles and publishes the release in one fixed flow:

1. **Producers** upload workflow artifacts only: `release-build.yml` (reusable)
   builds the glibc and MUSL x86_64 CLI and daemon archives, the glibc relay
   archive, the official runtime package archives (every `runtime-packages/*`
   directory: `package verify`, then `package build`, named
   `pohunek-runtime-<runtime>-<version>.tar.zst` with a `.sha256`) and the SDK
   tarballs; the macOS jobs build, ad-hoc sign and verify the Apple Silicon
   archives. No macOS job uses secrets or a protected environment.
2. **Evidence** (`release-evidence.yml`, reusable): one job per
   (runtime x target) row of `compat/matrix.json` extracts the binaries from the
   daemon ARCHIVE, stages the pinned upstream, runs the out-of-process consumer
   suite and emits `attestation-<runtime>-<target>.json`; the download-only
   `attest` job creates build provenance for every producer artifact;
   `assemble` (environment `release`) verifies that provenance with `gh
   attestation verify`, then `cargo xtask release assemble` signs the catalog
   with the CI root key and writes the bundle; `attest-bundle` attests the
   bundle; `smoke` runs `packaging/smoke-archive` on every Linux daemon archive
   in a network namespace; `verdict` requires all of them.
3. **Publish**: the single job with `contents: write`. It checks out nothing and
   runs no repository or downloaded code. It downloads the `release-bundle`
   artifact, checks it against `release-inventory.sha256` (the only list of
   files), verifies provenance, refuses a moved tag or any existing release of
   the tag (drafts included), creates a DRAFT with `gh release create --draft
   --verify-tag`, uploads exactly the inventory, compares the draft's asset
   names, digests and sizes with it, and only then publishes
   (`gh release edit --draft=false`) and reads the result back. A failed run
   deletes the draft it created. A visible release is therefore complete or
   absent; if a draft is ever left behind (the runner was killed), delete it in
   the GitHub UI or with `gh release delete vX.Y.Z --yes`, then rerun the
   failed workflow.

Every asset (Linux, macOS, SDK, package archives, the signed catalog, the
attestation documents, the rebuilt daemon archives, and the `.sha256` files)
has a build-provenance attestation from `release-evidence.yml`; the two jobs
that hold `id-token: write` and `attestations: write` (`attest`,
`attest-bundle`) download and attest only. Only the signing step of `assemble`
reads the secret `CATALOG_SIGNING_KEY_CI` of the environment `release`
(deployment policy: tags `v*`), passed by name from the caller to the reusable
workflow; it writes the seed to a 0700 tmpfs directory
with mode 0600, removes it from the environment before any program starts, and
shreds it afterwards. Developer ID signing and notarization are not part of the
pipeline. The offline docs, the README, and the reference
pages under `docs/` are bundled into every native component archive. CLI archives also contain
`packaging/smoke-hermes-plugin-release`. Release automation provisions the
source-locked Hermes runtime without provider credentials, runs the model-free
compatibility gate, extracts each CLI archive, and executes its packaged smoke
script against the extracted `pohunek` binary. Operators can repeat the same
script with an explicitly supplied, preinstalled pinned Hermes executable. It
creates an isolated temporary profile/state, requires that executable rather
than downloading it, and fails if install, status, doctor, or uninstall cannot
prove the embedded plugin and generated skill.

### Release rehearsal

`ci.yml` runs the same two reusable workflows with `mode: rehearsal`
(`release-rehearsal-build`, `release-rehearsal`) on push to main, the weekly
schedule, manual dispatch, and pull requests that change the release workflows,
`packaging/**`, `scripts/release-workflow/**`, `crates/xtask/**`, `compat/**`,
`runtime-packages/**`, the consumer suite or the release workflow tests. It
builds the Linux archives, packages and SDK tarballs from the PR commit, runs
every matrix row, assembles a bundle and runs the archive smoke. It uses no
secret and no environment: the daemon archives are staged with a throwaway trust
anchor and the bundle is signed with a key derived from
`sha256("rehearsal <run id> <commit>")`, so the key signs only a bundle that is
never published, and no provenance is created. It does not run for fork or
Dependabot pull requests. The rehearsal cannot prove what needs the real tag:
signing with the production key against `packaging/runtime-catalog-anchor.json`,
`gh attestation verify` against real attestations, the macOS archives, and the
draft/publish semantics of the `publish` job.

### Adding a runtime package or row

Add `runtime-packages/<runtime>/`, `compat/<runtime>/` (lock and npm project) and
the rows to `compat/matrix.json` (`cargo xtask compat matrix-check`), and a driver
entry for the consumer suite. Nothing in `release.yml`, `release-build.yml` or
`release-evidence.yml` names a runtime: the package job, the row matrix and the
smoke derive them from those directories and files. A new target needs a runner
mapping in `scripts/release-workflow/row-matrix`, a build leg in
`release-build.yml` and the policy entries in `packaging/release-policy.json`.

The structure of the workflows (one writer, the OIDC jobs, the secret, pinned
actions, asset names against the policy) is pinned by
`scripts/tests/test_release_workflow.py`; the helper scripts under
`scripts/release-workflow/` by `scripts/tests/test_release_workflow_scripts.py`.
Check workflow edits with `actionlint` (with `shellcheck` on `PATH`) before
pushing.

Runtime package compatibility is attested per package and target:
`cargo xtask compat attest` turns a consumer-suite report into an attestation
recomputed from the artifact bytes, `cargo xtask compat verify` re-checks one,
and `cargo xtask compat matrix-check` keeps `compat/matrix.json` equal to the
`runtime-packages/` directories times the declared targets (bump its
`suite_version` when the consumer suite's semantics change). See the knowledge
page `concepts/release-attestation.md`.

`cargo xtask compat stage-upstream --runtime <rt> --out <dir>` installs the upstream release a runtime's lock pins (network needed) from the committed `compat/<rt>/npm/package-lock.json` with `npm ci` and writes `STAGE.sha256` and `STAGE.links`; `cargo xtask compat verify-stage --runtime <rt> --dir <dir>` re-checks the tree offline. A lock edit changes attestation digests. See the knowledge page `concepts/upstream-staging.md`.

`cargo xtask release assemble` turns the producer artifacts into the release bundle: it requires exactly the inventory that `packaging/release-policy.json` and `compat/matrix.json` imply, recomputes every checksum, binary-set digest and attestation from bytes, signs the catalog, rebuilds the daemon archives with the catalog and packages and writes `release-inventory.sha256`; `cargo xtask release verify-inventory --dir <bundle>` re-checks a bundle. See the knowledge page `concepts/release-bundle.md`.

After a published stable release, `.github/workflows/notify-tap.yml` (a `workflow_run` of `Release`, the only workflow with a secret, `TAP_DISPATCH_PAT`) sends the Homebrew tap `zajca/homebrew-pohunek` a `pohunek-work-release` repository dispatch for the `pohunek` formula and the tag, and the tap bumps it; pre-releases are skipped. `scripts/tests/test_notify_tap_workflow.py` pins it.

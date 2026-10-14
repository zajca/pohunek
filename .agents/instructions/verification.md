# Verification

CI runs with `RUSTFLAGS=-D warnings` (warnings are errors). Run these before
considering work done; they mirror CI exactly:

```bash
cargo fmt --all --check
scripts/check-rust-toolchain-pin                        # no floating Rust channel in CI
cargo clippy --workspace --all-targets --all-features   # must be clean under -D warnings
cargo build -p pohunek-session-worker --bin pohunek-sessiond  # daemon tests spawn it by path
cargo nextest run --profile ci --test-threads 4 --workspace --all-features
cargo test --doc --workspace --all-features              # nextest excludes doctests
cargo build --workspace --release                        # release profile must build (CI: see below)
cargo xtask docs check                                   # schema/log fragments/drift/source-map/verifier/secrets/runbooks
scripts/build-hermes-package /abs/hermes.tar.zst        # Hermes runtime package for the gate
cargo xtask hermes compatibility --bin-dir ABS --package /abs/hermes.tar.zst  # pinned, model-free Hermes CLI/golden gate
```

The `platform contracts (arm64)` CI job runs natively on Apple Silicon macOS
with `MACOSX_DEPLOYMENT_TARGET=14.0`. It lints nine packages with one clippy
run: `pohunek-platform` (including the real-launchd suite `tests/launchd.rs`),
`pohunek-paths`, `pohunek-worker-protocol`, `pohunek-session-worker`,
`pohunek-service-config`, `pohunek-test-support`, `pohunek-daemon`,
`pohunek-cli`, and `pohunek-hostcheck`. Their tests come from one nextest
archive built with `--profile macos`: `scripts/test-partitions --archive-file
<archive> check-macos --scope lane` proves each test is selected exactly once,
and `run-macos` runs them. The `macos` profile in `.config/nextest.toml` runs
five threads, retries twice for diagnosis (a flaky result still fails the run),
and serializes the tests that drive the shared `gui/<uid>` launchd domain. The
job runs the lane packages' doctests natively (nextest runs none), runs the
APFS foreign-owner test as root from the extracted archive, and fails hard when
the runner's `gui/<uid>` launchd domain is absent. It also runs the macOS
contributor path first: the `scripts/tests/test_dev_bootstrap.py` scenarios under the runner's
`python3` (>= 3.11, checked) and `scripts/dev-bootstrap --json --path native`,
which must report `launchd-gui-domain` as `ok`. Linux clippy never compiles the
launchd backend or any other `cfg(target_os = "macos")` code, so lint it from
Linux with `scripts/darwin-check` before pushing (`scripts/darwin-check -p
<crate>` narrows it; `bacon darwin-check` re-runs it on save). It runs
`cargo clippy --target aarch64-apple-darwin --all-targets --all-features -- -D
warnings` over the workspace and needs `rustup target add aarch64-apple-darwin`
plus `zig` on PATH: `ring` and `psm` compile C in their build scripts, and `zig
cc` supplies the macOS libc headers a Linux host lacks. It only checks, so a
green run says the macOS code compiles and lints, not that it behaves; the CI
job `macOS lint from Linux (aarch64-apple-darwin clippy)` runs the same
script on every pull request, and the native jobs stay the behavior evidence.
The `macOS tier packages (arm64)` job covers the other macOS packages,
`pohunek-client`, `pohunek-netbird`, `pohunek-package`, and `xtask`, with
`run-macos-tier` from a 13-package archive. `check-macos --scope all` checks
the full set. It never runs on a pull request: only pushes, the daily schedule,
and `main` or `nightly` dispatches start it.
The real-systemd suites (`crates/platform/tests/systemd.rs`,
`crates/daemon/tests/systemd_durable_worker.rs`,
`crates/cli/tests/service_systemd.rs`,
`crates/daemon/tests/supervised_lifecycle.rs`,
`crates/cli/tests/service_upgrade.rs`,
`crates/cli/tests/workflow_systemd.rs`) are `#[ignore]`d on Linux and need
`POHUNEK_SYSTEMD_E2E=1` plus a running user manager; the Linux CI job
`real systemd supervision` runs all six with `--ignored --test-threads 1`
after building the daemon, worker, and CLI binaries. Run them locally the same
way (`POHUNEK_DAEMON_BIN`, `POHUNEK_WORKER_BIN`, and `POHUNEK_CLI_BIN` point the
suites at the built binaries). The `upgrade from the previous release` CI job (`.github/workflows/upgrade.yml`,
called by `ci.yml` when the daemon, worker, protocol, or CLI crates change, and
by `release.yml` before any publishable binary is built) installs the previous
release's published Linux binaries (checksum verified, never rebuilt), runs
it on the built-in Claude Code and Codex runtimes, with no package installed and
no profile pinned. It installs its managed Claude hooks and starts a stopped and
a live session of Claude profiles, each launched by a fake agent that runs those
hooks (`crates/cli/tests/release_upgrade.rs`). This build then runs
`pohunek service check` and one `pohunek service upgrade` without
`--accept-runtime-loss`, and the test asserts that both preflights judge the live
and the stopped session `adoptable`, that the plan installs the claude and codex
packages and binds every profile, and that the applied step pins every host
profile to the head's package digests and reinstalls the hooks. The live session
is adopted by the same worker and keeps `screen`, `input`, hooks and
notifications working, and the store keeps its built-in binding until a
relaunch. The stopped session resumes with a plain `resume`, which must succeed,
and its new run is pinned to the package digest; the adopted live session then
stops and resumes through the package too. The head links a probe package
directory (so its daemon installs an archive of this build's package format),
new Claude and Codex sessions launch through the packages, and the previous
release's CLI still talks to the new daemon. The head's `runtime/` tree and its
anchor are staged by `cargo xtask release test-runtime` (a test-only catalog
with a discarded key), so the step runs against the head's own packages. The
fake agents are scripts whose interpreters are copies of `/bin/sh` named
`fake-claude-sh` and `fake-codex-sh` (the worker accepts a launch process only by
an executable name containing its provider), and they answer `--version` with the
lowest release the head package's descriptor accepts. The previous release must ship a
built-in Claude and Codex runtime with the daemon archive it installs from, so
v0.34.2 is the oldest usable previous release; `scripts/upgrade-test` refuses an
older one before downloading or building anything. It uploads the previous release's store
and protocol outputs as the `upgrade-goldens-<tag>` artifact. Run it on any pair
with `gh workflow run upgrade.yml -f previous_tag=v0.34.2 -f head_ref=<ref>`
(the harness comes from the dispatched ref; `head_ref` is only the source built
as the target). Run it locally on a Linux x86_64 host with systemd, passwordless
sudo, `gh`, and `jq`:

```bash
scripts/upgrade-test --previous latest --repo zajca/pohunek --head-dir . \
  --cache-dir ~/.cache/pohunek-upgrade --artifacts-dir /var/tmp/pohunek-upgrade-out \
  --create-account
```

CI runs it with `--current-user` instead, as the runner user against the
runner's own user manager; that mode refuses to run unless `CI=true` and
`GITHUB_ACTIONS=true`. On failure the script leaves the unit status, journals,
`service.toml`, the transaction journal, and every CLI call's output under the
artifacts directory (`diagnostics/`, and `test-output/calls/`).

Released binaries use the default `$HOME` layout and the user's systemd manager,
so the script never touches your own installation: it creates a throwaway local
account (`--create-account` is the explicit consent), starts that account's user
manager, runs the test there with a minimal environment, and deletes the account
and its home afterwards. When the target has the previous release's version the
script builds it with a `+upgrade.<commit>` version suffix (`Cargo.toml` and
`Cargo.lock` are restored). Never run the `release_upgrade` test directly in
your own account.
The `macOS package install and upgrade
(arm64)` CI job builds the daemon archive twice with `packaging/macos/build-release`
and `packaging/macos/package --development` (the archives must be
byte-identical and pass `packaging/macos/audit-macho`), then installs,
upgrades with live sessions, refuses corrupt archives, and uninstalls from the
extracted archives against real launchd
(`scripts/acceptance/macos-package-install`). Intel Macs are outside the
current release scope. This gate does not mean complete macOS host support;
delivery scope, order, and status are tracked by the
[`Complete macOS support` milestone](https://github.com/zajca/pohunek/milestone/2),
the [macOS project](https://github.com/users/zajca/projects/4), and the issue
hierarchy rooted at #94. `docs/design/macos-support-rfc.md` records design
constraints and rationale, not live delivery state.

CI uses three execution tiers (issue #477). Pull requests always run the
required Linux checks. On PRs, macOS platform contracts run when their input
filter matches; the package job runs on `macos-15` only for packaging,
install-acceptance, launchd-service, or relevant workflow inputs; the Hermes
macOS job runs only for its narrow input filter, never because the runtime
package selection picked Hermes; and the SDK macOS job runs for its own narrow
input filter. The Linux `hermes-package` job follows the package selection like
the other runtime package jobs. A change to
`.github/workflows/ci.yml` selects all four pull-request macOS jobs. The
release rehearsal, which a pull request starts through its `release_rehearsal`
filter, adds macOS jobs from the workflows it calls: the signed
`aarch64-apple-darwin` CLI and daemon builds (`rehearsal-macos`), the macOS
leg of the release consumer build, and the `aarch64-apple-darwin` release rows
of the Claude, Codex, Hermes, and Pi packages. A push to
`main` runs the main tier: platform contracts, the macOS tier packages, the
`macos-15` package job, Hermes macOS, and SDK macOS, alongside the Linux jobs
selected on main. PR and main runs cancel superseded runs on the same ref. A
daily 05:00 UTC nightly run includes the main tier, the required `macos-14`
package leg, the cold rebuild comparison, and the nightly-only relay database
suite. The `macos-14` package leg runs only in nightly schedule and `nightly`
manual dispatch, and must pass for nightly CI evidence. Manual
dispatch requires the `main` or `nightly` choice; nightly adds the nightly-only
checks to main. Schedule and manual runs do not cancel PR or main runs.

Before the final head of a pull request merges, dispatch `ci.yml` once with
`tier=main` on the branch (`gh workflow run ci.yml --ref <branch> -f tier=main`)
only when the diff touches a release workflow or release xtask code, the runtime
packages or packaging, the TypeScript SDK, Cargo dependencies, the protocol
version window, or what the main and nightly tiers run (their jobs in `ci.yml`,
`scripts/test-partitions`, `.config/nextest.toml`). Ordinary worker, PTY, hook,
daemon, and CLI changes need no dispatch: pull-request CI already runs the
macOS lanes.

The automatic red-main issue reporter remains pending under #477 S3. The S4
release evidence gate is implemented: `scripts/release` requires successful
main and nightly CI evidence for the exact release commit before tagging. The
script dispatches those tiers when it pushes the candidate; `--no-push` checks
existing evidence only.

SDK workspace gates (one Bun workspace root at the repository root spans `sdk/ts/*`):

```bash
bun install --frozen-lockfile
bun run typecheck   # one command: tsc -b source graph + test and release-script checks
bun run lint
bun test
bun test sdk/ts/scripts   # SDK release pack contract
```

Stale per-branch Cargo isolation dirs: `scripts/cargo-sweep-targets --dry-run`
(preview) / `--days 30` (delete, 30-day retention; `--cron` prints the monthly
line). `target/debug`, `target/release`, nextest/doc scratch dirs, manual
`pohunek-eval` transcripts, and standard Cargo outputs (`doc`, `package`,
cross-compile triples) are kept; only entries shaped like Cargo target dirs
are deleted, and a non-target `--target-dir` refuses to run.

The real-daemon suites are opt-in locally and mandatory in CI after building
`pohunekd`, `pohunek-sessiond`, and `pohunek` (the suites run the daemon in
subprocess worker mode, which spawns `pohunek-sessiond` from beside `pohunekd`).
The Hermes plugin e2e lives in the SDK workspace
(`sdk/ts/sdk/test/hermes-plugin.e2e.test.ts`), depends on no client package, and
drives the real CLI. The daemon serves `hermes` only from the official Hermes
package, so the e2e installs the archive built from `runtime-packages/hermes`
through a throwaway signed catalog whose trust anchor it places beside a private
copy of the daemon:

```bash
cargo build -p pohunek-daemon -p pohunek-session-worker -p pohunek-cli
scripts/build-hermes-package target/hermes-e2e/hermes.tar.zst
cargo xtask hermes throwaway-catalog --package target/hermes-e2e/hermes.tar.zst \
  --output-dir target/hermes-e2e/catalog
POHUNEK_E2E=1 POHUNEK_DAEMON_BIN=/absolute/path/to/target/debug/pohunekd \
  POHUNEK_CLI_BIN=/absolute/path/to/target/debug/pohunek \
  POHUNEK_PYTHON_BIN=/usr/bin/python3 \
  POHUNEK_HERMES_PACKAGE=/absolute/path/to/target/hermes-e2e/hermes.tar.zst \
  POHUNEK_HERMES_CATALOG_DIR=/absolute/path/to/target/hermes-e2e/catalog \
  bun test sdk/ts/sdk/test/e2e.test.ts sdk/ts/sdk/test/hermes-plugin.e2e.test.ts
```

The throwaway catalog expires a day after `throwaway-catalog` writes it.

The official Pi runtime package (`runtime-packages/pi`, lock in `compat/pi/`) has
always-running tests in `crates/cli/tests/pi_package.rs` (descriptor, supported
range against the lock, manifest on real screens, existence check on Pi's real
file layout) and two `#[ignore]`d tests that drive a real `pi` through the
installed package against a loopback model stub. The `pi-package` CI job
installs the locked release from npm and runs them; locally:

```bash
cargo xtask package build runtime-packages/pi --output ABS/pi.tar.zst
POHUNEK_PI_E2E=1 POHUNEK_PI_PACKAGE_ARCHIVE=ABS/pi.tar.zst \
  cargo test -p pohunek-cli --test pi_package -- --include-ignored --test-threads 1
```

Moving the supported Pi range means changing the descriptor's `min`/`below`
and the lock together; the pure test fails when they differ.

The Codex runtime package source (`runtime-packages/codex`, lock and captured
screens in `compat/codex/`) has the same shape in
`crates/cli/tests/codex_package.rs`: always-running descriptor and manifest
parity with the frozen v0.34.2 goldens in `compat/codex/builtin-v0.34/`, the
supported range against the lock, and the manifest on real screens, plus
`#[ignore]`d tests that drive a real `codex`
with a loopback Responses stub and a fresh `CODEX_HOME`. The daemon-backed
tests install the built archive through a signed catalog with a throwaway key
and trust anchor, so the real-Codex tests launch through the installed package
and its version probe; the fixture kills every process it started, also when a
test fails. Run them with
`POHUNEK_CODEX_E2E=1 cargo test -p pohunek-cli --test codex_package --
--include-ignored --test-threads 1`; the `codex-package` CI job installs the
locked release from npm. Never point a manual run at the operator's real Codex home.

The TypeScript SDK is released as three npm-pack tarballs
(`pohunek-ts-protocol-X.Y.Z.tgz`, `pohunek-ts-sdk-X.Y.Z.tgz`,
`pohunek-ts-testkit-X.Y.Z.tgz`, each with a `.sha256`) built by
`sdk/ts/scripts/pack-release.ts` in the Release workflow's read-only `sdk-pack`
job, which needs `sdk-gate`; the single `publish` job of the Release workflow uploads them
with the rest of the verified release inventory. `bun test sdk/ts/scripts` is the pack contract: it
packs, serves the tarballs from a local HTTP server and runs a real
`bun install` of a consumer that pins them by URL against an unreachable
registry. Run it after touching an SDK package manifest, export map or file
layout. Manual pack:

```bash
SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)" bun sdk/ts/scripts/pack-release.ts \
  --version 0.0.0-test --base-url http://127.0.0.1:8765/rel --out /path/to/out
```

A protocol change is not done until `cargo xtask ts check` passes; regenerate
with `cargo xtask ts generate`.

## Fast loops

Start from a fast loop, not from the full gate set; run the applicable full
gate set once, on the final revision, before declaring work done (see "Testing
policy"). A warm local timing is not CI evidence: per-shard
JUnit artifacts and `scripts/ci-timings` (which also derives job wall clock,
JUnit execution time, and sccache/rust-cache hit rates from `gh` run data) are
the measurement path behind
`docs/design/test-performance-report.md`.

The loop cost boundary (nextest >= 0.9.131; profiles in `.config/nextest.toml`,
shard helper requires Python >= 3.11). CI compiles the workspace test binaries
once in `build-tests` (`cargo nextest archive --workspace --all-features`) and
runs four fast matrix shards, the heavy job, and the relay DB job from that
archive, independently of lint/release jobs. The cost boundary is
`profile.fast.default-filter`; update it when adding a PTY, DB, Hermes, or other
costly fixture. The helper takes its exact complement for heavy and verifies
all discovered tests, including ignored ones, are assigned exactly once. The
`relay-db` shard needs the real worker binary and a disposable PostgreSQL URL in
`POHUNEK_RELAY_TEST_DATABASE_URL` (`python3 scripts/test-partitions run
relay-db`); `cargo t`/`cargo ti` do not. Without that URL the relay
PostgreSQL tests fail locally by design: they are CI-only evidence from the
`tests (relay DB, PostgreSQL)` job, not regressions.
`cargo tw` remains unfiltered at four test processes. Archive jobs install only
`cargo-nextest` (no rust-cache; the heavy job adds a Clippy toolchain and
`cargo fetch --locked` for `xtask::dependency_policy`) and call
`python3 scripts/test-partitions --archive-file A run|check ...`, which
extracts `A` into the checkout's `target/` and lists or runs without compiling.
Tests resolve binaries and sources at run time through `pohunek-test-support`
(`CARGO_BIN_EXE_*`, `CARGO_MANIFEST_DIR`, the worker beside the test binaries),
so an archive runs from any absolute path (the CI test jobs check out into
`relocated/`); never reintroduce `env!("CARGO_BIN_EXE_*")` or
`env!("CARGO_MANIFEST_DIR")` in test code (an xtask scan test enforces it).
`archive.include` in `.config/nextest.toml` makes archive creation fail when
`pohunek-sessiond` is missing, so daemon tests never run without their worker.
`build-bins` remains a separate default-feature build for the SDK job, because
the archive's `--all-features` build enables cli's `test-util`.
In CI, the Postgres-backed
relay job is gated by a paths filter and does not run (its PostgreSQL service
never starts) for non-relay changes.

The CI workflow's PR/macOS path ownership and tier conditions are guarded by
`scripts/tests/test_ci_tiers.py`; run
`python3 -m unittest scripts.tests.test_ci_tiers` when changing the workflow.
The inventory distinguishes PR-triggering inputs from inputs covered by the
post-merge main tier, so narrowing a filter cannot silently drop coverage.
Manual runs use `workflow_dispatch` input `tier=main|nightly`; the daily
schedule selects nightly. The main tier is latest-wins on pushes to `main`,
while PR runs are latest-wins per pull request. S3's automatic red-main issue
reporter remains pending. The S4 release evidence gate is enforced by
`scripts/release`, which accepts only successful main and nightly evidence for
the exact candidate commit; `--no-push` requires those runs to exist already.

Doctests and the release build: the `doctests` job runs on
every pull request. The release build (`release-build` job) runs on push to
`main`, the daily schedule, and manual dispatch, and on a pull request only
when it touches a `release` filter input in `ci.yml` (`Cargo.toml`,
`crates/**/Cargo.toml`, `Cargo.lock`, `.cargo/**`, toolchain pins, the CI and
release workflows, `scripts/release`, `packaging/**`); run the release build
locally before declaring such a change done.

Timing in tests: a test never asserts a duration against real time or uses a
sleep as synchronization. When the deadline is the behavior under test, run on
virtual time (`#[tokio::test(start_paused = true)]` or an injected clock); when
it is incidental, wait on a readiness signal with `pohunek_test_support::wait`,
bounded by `HANG_GUARD`. `crates/xtask/tests/timing_ratchet.rs` counts sleeps
and `.elapsed()` literal assertions in test code per file against
`timing_ratchet_baseline.txt`, which may only go down (lower it with the
ignored `regenerate_timing_baseline` test). Paused-time tests exempt only
tokio timers (and `.elapsed()` when the file uses `tokio::time::Instant`
alone); `thread::sleep` and `Timer::after` stay counted. An unavoidable real-time wait
carries `// timing-allowed: #<issue> <reason>` on or directly above its line.

Flaky tests: `.config/nextest.toml` sets `flaky-result = "fail"` in
`profile.default`, so every profile inherits it. Only `heavy` and `relay-db`
retry (`count = 2`, fixed 1s delay): PTY, socket, worker-process, and
PostgreSQL fixtures. `ci`/`fast` never retry, because a fast test that fails
intermittently is a bug; `package` (the runtime package jobs' real-upstream
tests) never retries either, because a failure there is a behavior change of
the locked release or a bug. A test that passes only on a retry is reported as
`FLKY-FL` in the log and stays a `<failure>` in `junit.xml`, and the run still
fails. Retries only tell "fails every time" apart from "fails sometimes". Each
CI test job runs `scripts/junit-flaky-summary` with `if: always()`, which lists
flaky and failed tests in the job summary. A FLAKY result means opening a bug issue with
the root cause. A per-test `flaky-result = "pass"` override is allowed only
with a linked issue and a reason in a comment next to it.

Hermetic tests: a test owns its fixtures and never touches host state.
Directories come from `pohunek_test_support::tempdir()` or
`pohunek_test_support::env::TestEnv` (private root, cwd, HOME/XDG/TMPDIR, scrubbed
child environment), never from `std::env::temp_dir()`, a host `/tmp` path or the
`tempfile` constructors, and a socket stays bound instead of "bind port 0, drop,
reuse the number". A test never calls `std::env::set_var`/`remove_var`: it passes the
value to the code under test, or, where reading the process environment is the
subject, changes it through `pohunek_test_support::process_env::ProcessEnv` (one
binary-wide lock, restored on drop and on unwind; tests that only read
environment-derived values hold `ProcessEnv::lock()` too, and a command that must not
see another test's `PATH` is built with `process_env::command`).

No test leaves a session worker running. A real daemon starts each
`pohunek-sessiond` in its own process group and lets it outlive the daemon, and a
stopped session keeps its worker until `session.remove`, so a test that kills the
daemon child must remove every session it created first. `TestEnv` carries a
`pohunek_test_support::workers::WorkerGuard` for its root; a test that builds its
own root (for example `tempdir_with_prefix`) wraps it with `WorkerGuard::watch`,
declared after the root so it drops first. On drop, on success and on unwind, the
guard terminates every worker whose `--daemon-socket-path` is below the root and
fails the test that left it. The worker's argv is the only marker it carries (the
launcher clears its environment), so the match is by root path and never touches
workers of the host or of another run. `scripts/test-partitions run` adds a run-level
check on Linux: the run gets a private `TMPDIR` base (at most 9 bytes, the budget of the
deepest nested test root; derived next to `RUN_BASE_MAX_LENGTH`), and any worker below
it after nextest exits, or after the run is cancelled, is terminated and fails the run.
Both checks read the process table from `/proc` (never through `PATH`), kill the
worker's workload (descendants and the sessions they lead, which outlive a terminated
worker) before the worker, and signal each process only after re-verifying its start time. Plain `cargo nextest run` relies on
the guards alone.
The daemon crate's fixtures end every other process a failing fixture started the same
way: its `ScopedDir` and thread-scoped roots and the supervised-lifecycle `Installation`
carry a `ScopeReaper` (`procwatch::scoped_host`) that, on drop and on unwind, kills every
same-user process whose working directory is at or below the root, read through the
`pohunek_platform` `HostInspector` (libproc on macOS, `/proc` on Linux); the `Installation`
reaper spares `pohunek-sessiond` so its `WorkerGuard` still fails the test that leaks a
worker, and a reaper never panics, because it can drop at thread exit. A fixture process
whose ownership markers the product must read runs the test binary as the holding image
(`pohunek_test_support::hold`, declared with `hold_image!`; it returns at once under a
plain `--ignored` run) instead of a platform binary, because macOS 27 withholds the
environment of `/bin/sh`, `/bin/cat` and `/bin/sleep`.
A copy of a shell image used as a fixture interpreter goes through
`pohunek_test_support::fs::copy_shell_image`, because macOS 27 kills a copy of a platform binary on
launch; the helper re-signs the copy ad hoc only when the kernel killed it.
`crates/xtask/tests/hermetic_scan.rs` enforces all of this in test code with no
baseline. A case where the host state is the subject carries
`// hermetic-allowed: #<issue> <reason>` on or directly above its line; the
environment-mutation rule takes no marker, because `ProcessEnv` is the one sanctioned
mechanism.

Rust:

```bash
cargo t                                       # default inner loop: cost-filtered fast tests
cargo ta                                      # CPU-saving loop: fast tests of changed crates + dependents
cargo ta --print                              # per-file reasons and the command; runs nothing
cargo xtask package affected --json          # official runtime packages a change selects (CI package jobs)
cargo t -p pohunek-daemon                     # fast tests in one crate (alias takes -p)
cargo nextest run --profile local -p pohunek-cli some_test_name  # one test
cargo clippy -p pohunek-daemon --all-targets  # lint one crate
python3 scripts/test-partitions run cli       # exact CI shard: core/daemon/relay/cli/relay-db/heavy
python3 scripts/test-partitions check         # exhaustive, disjoint nextest inventory check
python3 scripts/test-partitions --archive-file nextest-archive.tar.zst run heavy  # CI mode: no compile
python3 scripts/test-partitions --archive-file A check-macos   # 13-package macOS set: exact lane/tier/bucket partition
python3 scripts/test-partitions --archive-file A check-macos --scope lane  # PR lane archive: its 9 packages only
python3 scripts/test-partitions --archive-file A run-macos     # PR macOS lane: 9 packages, profile macos
python3 scripts/test-partitions --archive-file A run-macos-tier  # main/nightly macOS tier: client, netbird, package, xtask
```

`cargo ta` (`cargo xtask affected [--base REF] [--print] [-- NEXTEST_ARGS]`)
is the CPU-saving alternative to `cargo t`. It adds about 3 s of xtask start
and gives no wall-clock gain on an idle host, because the build is identical
and test time is bounded by the slowest selected test. It runs fewer tests,
which saves CPU when several agents or worktrees share the host, so prefer it
there; prefer `cargo t` on an otherwise idle host or for a wide change.
It diffs against the merge base with `origin/main` (else `main`, else it
fails; `--base` overrides) and adds staged, unstaged, and untracked files.
Each file selects the package whose directory contains it;
paths that crates embed or their tests read from outside their own directory
(`docs/knowledge`, `compat/`, `scripts/`, `packaging/`, the release workflow)
select those crates. It then runs `cargo t -E 'rdeps(=a) | ...'`, so dependents
run too and the fast profile's default filter still applies. It fails safe:
the root `Cargo.toml`, `Cargo.lock`, `.cargo/`, `.config/nextest.toml`,
`rust-toolchain*`, lint/format configs, and any path no rule covers run
everything. Only a reviewed allowlist in `crates/xtask/src/affected.rs` (other paths:
`sdk/ts/`, the root Bun workspace files, `docs/`, `.github/`, `.claude/`, `.agents/`, `assets/`, root `*.md`,
`LICENSE`, `bacon.toml`) selects no Rust tests; when nothing else changed it
exits 0 without running nextest and names the follow-up checks (`cargo xtask
docs check`, the `scripts/` unittests, the Bun gates). It narrows only which
tests run and never replaces the full gate set.

New worktree: `scripts/worktree-new <slug> [<base-ref>]` creates
`pohunek-worktrees/<slug>` beside the primary checkout, whichever checkout
it runs from (the primary checkout is the parent of `git rev-parse
--path-format=absolute --git-common-dir`), prints that absolute path, puts it
on `zajca/<slug>` (base: `origin/main` after a fetch), and seeds its own
`target/debug` caches from the main checkout with `cp --reflink=always`, so the first build recompiles only the workspace
crates that differ, not every registry dependency. It needs the main
checkout's `target/` and the worktree on one reflink-capable filesystem
(btrfs/XFS; never `/tmp`), fails closed otherwise, while a Cargo build holds
the main checkout's lock, while another `worktree-new` run is in progress, or
when any entry of the seeded source trees is a symlink, and never falls back to
a full copy; `--no-seed` accepts a cold build. It builds the worktree at a
temporary sibling path on a temporary branch, both named with a random token,
then `git worktree move`s it into place and renames the branch last, so a
failed run rolls back only what it provably created and never touches work
another process made under the final names. If only that last rename fails,
the finished worktree is kept on its temporary branch and both are reported.
The seed helps in proportion to how recently the main checkout was built at a
similar `Cargo.lock`, so a stale seed is skipped automatically: when the base's `Cargo.lock`
differs from the main checkout's (an explicit `<base-ref>` can carry another
dependency set), or the last `Cargo.lock` change landed on the base's mainline
after the newest mtime of the seed's `.fingerprint` and `deps` dirs, a seeded
first build would rebuild the changed dependencies anyway, so the script seeds nothing and prints `not seeded
(stale seed ...)` with the reason. `--force-seed` seeds anyway (mutually
exclusive with `--no-seed`), and a staleness signal that cannot be read keeps
the seed. Refreshing the main checkout's `target/` is a plain `cargo build
--workspace --all-targets --all-features` there.

SDK TypeScript (run from the repository root):

```bash
bun test sdk/ts/sdk/test/e2e.test.ts                 # one test file
bun test -t "notifications"                          # only tests matching the name pattern
bun test sdk/ts/sdk/test/config.test.ts -t "one case" # file plus name pattern
cd sdk/ts/sdk && bun run typecheck                   # one package's tsc -b graph
```

Rust toolchain pin: `rust-toolchain.toml` pins the exact stable release
(`channel`, with `clippy` and `rustfmt`) that every CI job installs through
`.github/actions/setup-rust` and that local runs use. The MSRV
(`rust-version = "1.96"` in `Cargo.toml`) stays the declared floor; nothing
builds at it in CI. Local runs follow the pin only through the rustup proxies:
put `$CARGO_HOME/bin` (default `~/.cargo/bin`) first on PATH and run `rustup
toolchain install` once in the repository root (with no argument it installs
the toolchain `rust-toolchain.toml` names). A distribution `cargo`/`rustc`
earlier on PATH (`/usr/bin`) ignores the file. `RUSTUP_TOOLCHAIN` and `rustup
override` win over it. Inside the repository a missing pinned toolchain is
auto-installed by the proxies and by most `rustup` subcommands; set
`RUSTUP_AUTO_INSTALL=0` to prevent that. The one other toolchain is the dated
nightly of the `cargo-udeps` job: its `toolchain: nightly-YYYY-MM-DD` input in
`.github/workflows/ci.yml` is the only place a dated nightly is named.
`scripts/check-rust-toolchain-pin` rejects a floating channel (`stable`, an
undated `nightly`, a non-exact `channel`) and any direct
`dtolnay/rust-toolchain` use outside the composite action.

Bumping the pin is one pull request per bump:

```bash
scripts/bump-rust-toolchain --stable X.Y.Z           # rewrites rust-toolchain.toml
scripts/bump-rust-toolchain --nightly YYYY-MM-DD     # rewrites every dated nightly input in the workflows
```

Either flag or both; it validates the formats, rewrites only the pins,
reports a second identical run as no change, prints the follow-up commands,
and never installs anything. In the same PR, install the new toolchain, fix
every new clippy lint and `cargo fmt` change it brings, run the gate set, and
make sure `scripts/check-rust-toolchain-pin` passes. Pull-request CI does not
run the `cargo-udeps` job, so a nightly bump is verified before merge by a
manual nightly-tier run of the bump branch
(`gh workflow run ci.yml --ref <branch> -f tier=nightly`).

Local toolchain: `scripts/dev-bootstrap` checks the tools these loops need
against their minimum versions and prints the exact install command for each
missing or too-old one. `rustc` (the workspace `rust-version`), `cargo-nextest` (minimum from `.config/nextest.toml`),
`python3` >= 3.11, and the Hermes test interpreter (`POHUNEK_PYTHON_BIN` or the
`test_python.rs` candidates: Python >= 3.10 with `yaml` that passes the runtime
file policy; the `pohunek-cli` Hermes tests fail without it, and the printed
fix mirrors the CI uv step) are required; `bacon`, `hyperfine`
(`scripts/measure-dev-loop`), and `mold` (CI linker only; checked on Linux only) fail only with
`--strict`. The `rustc-pin` row compares the active `rustc` (first on PATH)
with the `rust-toolchain.toml` pin and prints the PATH and `rustup toolchain
install` fix on a mismatch. A mismatch fails only with `--strict`: the
workspace builds and tests on any `rustc` at or above the MSRV, a mismatch
changes only what clippy and rustfmt report (CI enforces both on the pin), and
a distribution `rustc` without rustup cannot follow the pin. The row checks
`rustc` only; a local clippy or fmt result is gate evidence when the rustup
proxies are first on PATH, because only then do `clippy` and `rustfmt` come
from the pinned toolchain too. A missing or
malformed `rust-toolchain.toml` is a configuration error (exit 2). Version
commands run in the repository root with `RUSTUP_AUTO_INSTALL=0`, so when the
rustup proxies are first and the pinned toolchain is not installed yet (right
after a bump), the `rustc` and `cargo-nextest` rows report
`toolchain-not-installed` with `rustup toolchain install` (in the repository
root) as the fix, and the pin row is not compared. That fails the run without
`--strict`: until the toolchain exists, every `cargo`/`rustc` call in the
repository fails or downloads it first. It never
installs anything and never changes host configuration; run the printed fixes
yourself.

Those rows are the `rust` contribution path, the default. `--path NAME`
(repeatable) selects others: `sdk` (`bun` at or above the `.bun-version` pin,
which must agree with `packageManager` in `package.json`, plus a `bun-pin` row
that, like `rustc-pin`, fails only with `--strict`; the fix is the official
installer for the pin), `scripts` (`python3`; optional `shellcheck` and, on
Linux, `dash`, checked by presence only because it has no version flag),
`macos-lint` (the `aarch64-apple-darwin` rustup target and `zig` for
`scripts/darwin-check`; `not-applicable` on macOS, which lints natively),
`relay-db` (`POHUNEK_RELAY_TEST_DATABASE_URL`, reported only as set or
`unset`, never its value, and removed from the environment of every command
the script runs) and `native` (on Linux a systemd user manager in `running` or
`degraded` state, whose row names the per-command `POHUNEK_SYSTEMD_E2E=1`
opt-in the suites run with; on macOS the launchd `gui/<uid>` domain; a suite
the host cannot run is `unavailable`, or `ci-only` with the CI job that runs
it, and never `ok`). A tool shared by two paths is
reported once, under the first selected path. Each path also names its
checks, which the script lists and never runs: `focused` commands for the
inner loop (rust `cargo xtask affected` and `cargo t -p <crate>`; sdk the Bun
gates; scripts the `scripts/tests` unittests; macos-lint
`scripts/darwin-check`; relay-db `python3 scripts/test-partitions run
relay-db`; native the binary build, then the six real-systemd suites on
Linux, or the `pohunek-platform`, `pohunek-daemon` and `pohunek-cli` tests
that drive launchd on macOS), a `final` pointer to the gate set in
`.agents/skills/gates/SKILL.md` and this guide (never a copy), and `ci-only`
CI jobs whose run is evidence those focused checks do not produce (the other
OS's native suites stay a `ci-only` row). The report prints them under each
path's rows only with an explicit `--path`; the default invocation prints the
rows alone. `--json` prints one document on
stdout (`schema` `pohunek.dev-bootstrap`, `version` 1, `paths`, `strict`, `os`,
`rows` with `path`/`name`/`category`/`status`/`version`/`minimum`/`location`/
`detail`/`fix`, `checks` with `path`/`kind`/`command`/`note` (`command` is null
except for a `focused` check; `[]` in an error document), `failing`, `result`
`ready`/`not-ready`/`error`; keys and new enumerated values (statuses,
categories, check kinds, error codes) are only added within a version, a
consumer treats an unknown value as not ok, and a removed or renamed key
bumps the version); the status
codes and the `unsupported-path` / `config-error` / `internal-error` error
codes are stable and listed in the script's docstring; every outcome, an
unexpected exception included, prints exactly one document. Exit status: 0
ready, 1 not ready, 2 for an unknown `--path`, a configuration error, or an
internal error under `--json` (without `--json`, the script's exit-2 messages
go to stdout).

```bash
scripts/dev-bootstrap                          # rust path; non-zero if a required tool fails
scripts/dev-bootstrap --strict                 # optional tools and a pin mismatch fail the run too
scripts/dev-bootstrap --path rust --path sdk   # several contribution paths
scripts/dev-bootstrap --json --path native     # machine-readable report on stdout
```

Watcher (`bacon.toml` at the repo root; optional tool, no gate depends on it —
install with `cargo install --locked bacon`). Inside bacon, `e` switches to
`check`, `n`/`t` to `nextest-fast`, `a` to `affected`, and `c` to `clippy-fast`:

```bash
bacon                      # default job `nextest-fast`: profile-fast nextest loop
bacon check                # `cargo check` over all targets and features only
bacon clippy-fast          # CI lint command
bacon affected             # `cargo xtask affected` on every save
bacon nextest-fast -- -p pohunek-daemon    # narrow the loop to one crate
```

Full debuginfo: the `dev`/`test` profiles emit line tables only for workspace
crates and no debuginfo for dependencies (root `Cargo.toml`), which keeps
file:line panic and `RUST_BACKTRACE` frames but not the variables and types a
step debugger needs. Opt in per invocation; the dependency override in
`[profile.dev.package."*"]` outranks `CARGO_PROFILE_DEV_DEBUG`, and Cargo has
no environment variable for a `"*"` package override, so dependencies need
`--config`. The opt-in compiles separate artifacts, so its first build is cold:

```bash
CARGO_PROFILE_DEV_DEBUG=true cargo build -p pohunek-daemon      # workspace crates only
cargo build -p pohunek-daemon --config 'profile.dev.debug=true' \
    --config 'profile.dev.package."*".debug=true'               # dependencies too
```

Optimized test dependencies: the root `Cargo.toml` compiles `regex-automata`,
`curve25519-dalek`, `ed25519-dalek` and `sha2` at `opt-level = 3` in the
`dev`/`test` profile, one `[profile.dev.package.<crate>]` block each; dependency
debuginfo stays off through `"*"`. The hot loops of the slowest fast tests run
inside these crates (lazy-DFA determinization for every pi detector case,
ed25519 and SHA-256 on every signed relay append), and at `opt-level = 0` they
dominate fast-loop time. Add a crate only when a profile of a slow test (for
example `valgrind --tool=callgrind` on its test binary) puts most of the time in
that dependency's own functions. Generic code is instantiated in the calling
crate and keeps that crate's opt-level, so the override does not reach it. Each
entry costs one optimized compile of the crate per cold build. Measure the fast
loop and `scripts/measure-dev-loop` before and after the addition.

Reproduce CI timing evidence from `gh` run data. Every fetch accumulates into
the snapshot `target/ci-timings/ci-runs.json`; passing that file as `--input`
re-measures from it with no network calls, while the plain commands always
query `gh` first. A measurement covers exactly the runs its own query named,
so a fuller snapshot never widens it. `--limit` applies per window, and a
window that fills it is reported as possibly truncated -- raise it until the
run count stops growing:

```bash
scripts/ci-timings runs --limit 60 --window 2026-09-14..2026-09-16 \
    --event pull_request --conclusion success      # fetch + snapshot + per-run table
scripts/ci-timings compare --baseline 2026-09-14..2026-09-16 \
    --current 2026-09-20..2026-09-21 --event pull_request --conclusion success
scripts/ci-timings compare --input target/ci-timings/ci-runs.json \
    --baseline 2026-09-14..2026-09-16 --current 2026-09-20..2026-09-21 \
    --event pull_request --conclusion success      # same tables, no network
scripts/ci-timings junit --label "tests (core, fast)" --run RUN_ID junit-core.xml
                                  # or: --job-seconds N to supply the job wall
                                  # by hand; --job NAME picks the paired CI job
scripts/ci-timings cache --run RUN_ID             # rust-cache restores per job, plus sccache
                                  # JSON where a job runs sccache (release.yml)
```

Record the local baseline before and after a change to the dev loop, and
reproduce CI timing evidence beside it -- this is the before/after
procedure for the dev-workflow performance project
([#163](https://github.com/zajca/pohunek/issues/163)). The local tool
measures cold/warm/incremental build + test cases into a dedicated target
dir and records metadata, sizes, and stripped environment variables in
`baseline.json`; `ci-timings` reports per-run and per-job wall clock, per-step
medians, and runner minutes from the same run data:

```bash
python3 scripts/measure-dev-loop run --dry-run    # print the exact commands, run nothing
python3 scripts/measure-dev-loop run              # cold, warm, incremental, size
python3 scripts/measure-dev-loop report target/measure-dev-loop-results/<ts>/baseline.json
python3 scripts/measure-dev-loop compare BEFORE.json AFTER.json
scripts/ci-timings runs --limit 3 --branch main --event push   # per-run + runner minutes
scripts/ci-timings steps --input target/ci-timings/ci-runs.json
scripts/ci-timings compare --baseline 2026-09-14..2026-09-16 \
    --current 2026-09-20..2026-09-21 --event pull_request
```

The real measurements are not part of any test or gate; run them
deliberately, one at a time, and keep the emitted `baseline.json` files
in the issue's evidence.

Hermes M3 supports only the pinned local interactive Hermes Agent `0.20.0`
runtime and its explicit profile-owned Pohunek operator plugin. The stable
model-free compatibility gate (`cargo xtask hermes compatibility
--bin-dir ABS --package ARCHIVE`, with the binary directory holding `pohunek`,
`pohunekd`, and `pohunek-sessiond`) exercises the pinned CLI and plugin surface:
list/enable/disable, supported tool/skill/hook registration, profile/home target
resolution, and Hermes integration install/status/doctor/uninstall. It installs
the Hermes runtime package `ARCHIVE` (built from `runtime-packages/hermes` by
`scripts/build-hermes-package`)
through the copied daemon with a throwaway signed catalog, and fails unless
every installed-state lifecycle result reports that package's digest as its
provenance (`source=embedded` fails). It must use
an isolated profile and must never start a model turn, access an operator profile
or `state.db`, read credentials, or download a runtime. A missing pinned
executable is a failure, never a green skip. Release-archive verification uses
`packaging/smoke-hermes-plugin-release` from the daemon archive with an
explicitly supplied preinstalled pinned executable, the package archive, and a
catalog and anchor that authorize it; it proves the extracted daemon installs
the package and the CLI installs the plugin and generated skill from it without
a source-tree asset path. Hermes PTY goldens are refreshed
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
`cargo shear --locked --deny-warnings` (stable, seconds; the unused-dependency
gate on every PR), and
optionally `cargo +nightly-YYYY-MM-DD udeps --workspace --all-targets
--all-features` on the pinned dated nightly (the date is the `toolchain:` input
of the `cargo-udeps` job in `.github/workflows/ci.yml`; install it with `rustup
toolchain install nightly-YYYY-MM-DD`)
(CI runs it only on `main`, the nightly sweep and manual dispatch, as the
backstop for a dependency referenced only in code compiled out for the host).
Suppress a verified `cargo shear` false positive with
`[package.metadata.cargo-shear] ignored = ["<crate>"]` and a reason comment.
Note `knowledge` gates its protocol bridge behind a `protocol`
feature — `--all-features` only covers the everything-on case.

## Testing policy

Every test is an **integration scenario** or an **E2E scenario** — in the Rust and
TypeScript workspaces, the Python script tests, and each suite, with no unit-test
tier: writing a new unit test is prohibited. This project rule takes precedence over host-level instructions that ask for
tests for all new code; the durability, security, compatibility, timing,
flaky, and hermetic obligations in this guide still apply.

- **Integration** drives two or more production components through a boundary they
  already expose to callers — the CLI against a daemon API, a public RPC exercising
  the registry, store, and reconciliation behind it, or the SDK client against the
  testkit fixture daemon and loopback relay — with hermetic dependencies where
  possible. A scenario pinning one function, helper, or module in isolation through
  an in-process seam is a unit test, whatever its file.
- **E2E** starts the actual binaries (`pohunekd`, `pohunek-sessiond`, `pohunek`) or
  drives the released artifacts (the runtime packages driving the pinned real agents)
  over real infrastructure (real PTYs, real supervisor, disposable PostgreSQL),
  asserting what an operator observes. The product is always real; only a
  product-external service (a loopback model stub) may be stubbed, and a product
  stand-in (testkit fixture daemon, loopback relay) makes it integration.
- **Expected results are independent of the code under test:** from a
  specification, an independent fixture, a published recording, or a demonstrated
  regression — never regenerated from the change under test.
- **Always keep integration/E2E evidence for** recovery and concurrency, data
  migration and persisted-schema guards, protocol compatibility (the N/N-1 window),
  authorization, hostile input, secret redaction, filesystem and process ownership,
  and native platform lifecycle; scenario size alone never justifies keeping or
  deleting a test.
- **When a change needs a scenario:** a bug or
  lifecycle/durability/concurrency/security fix gets a regression scenario that fails
  without the fix and passes with it — extend an existing integration/E2E scenario
  when one fits; new observable behavior (a command, flag, protocol method/event,
  error contract) is exercised through its callers' supported boundary; a trivial
  helper change or an already-covered refactor needs no new test.
- **Removing tests.** Delete a low-value test (a repeated assertion,
  self-roundtrip, or wrapper) when no meaningful integration/E2E scenario is lost;
  never shrink the suite by ignoring, filtering, renaming, or moving tests, weakening
  assertions, dropping negative cases, or adding retries; never fold independently
  diagnosable behaviors into one giant scenario. Coverage and mutation tooling stay
  out of the process; historical plan/RFC test lists are history, not mandates.
- **Existing unit-tier tests.** Isolated component and per-helper scenarios (mostly
  inline `#[cfg(test)]`) from earlier policies still exist, running and gating until
  replaced — epic #601 removed 617 declarations with no measured speed gain and left
  the tier. Auditing them (replace at a supported boundary or delete) is the
  follow-up PR sequence on [#671](https://github.com/zajca/pohunek/issues/671); until
  then this policy governs every new and touched test.

Validation: while iterating, run the checks the change affects (`cargo t -p <crate>`,
`cargo ta`, one test file, the relevant Bun, script, or docs check); before declaring
work done, run the applicable gate set listed above once, on the final
revision — a later fix re-runs the checks whose inputs it changed, unchanged checks
keep their evidence, and CI is the final landing evidence (a gate that cannot run
locally is CI-only, never green).

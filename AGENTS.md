# AGENTS.md

Canonical guide for any coding agent working in this repository. Keep it short,
accurate, and current — if you change a command, a crate boundary, or a
convention, update this file in the same change.

## What pohunek is

`pohunek` is an **owner-first control plane for durable coding-agent sessions**
across the operator's own machines. A Rust daemon (`pohunekd`) owns the logical
session registry and public API on each host; one isolated
`pohunek-sessiond` worker owns each live PTY and agent process. The Rust CLI
(`pohunek`) drives the daemon locally over a Unix socket and remotely over a
NetBird/WireGuard address. A native Iced GUI (`pohunek-gui`) is an optional
client. The accepted future direction adds an optional trusted team relay
without replacing these direct owner paths; it is not implemented yet.

It is pre-1.0 and experimental: wire shapes, config files, and on-disk metadata
may change freely. **Do not add backward-compatibility shims** unless asked.

Authoritative design lives in `docs/architecture.md` (it wins over `idea.md`).
Hard constraints, decided on purpose — respect them in every change:

- **Direct owner operation stays first-class.** Standalone and direct-overlay
  clients keep talking to each host daemon without a relay dependency. Their
  trust boundary remains owner-only socket/file permissions plus the configured
  overlay, with NetBird as the production provider.
- **The owner WebUI stays first-class.** The shipped Bun backend remains the
  private local/NetBird browser gateway. It is not replaced by the relay and
  must never become a second team-auth or relay-routing authority.
- **The optional team relay is additive.** The accepted design introduces one
  trusted Rust `pohunek-relayd` authority for teams, end-user authorization,
  routing, audit, and quotas. Each host remains authoritative for its PTYs,
  processes, worktrees, session origin, and locally approved `HostShare`
  ceilings. The first release trusts collaborators at the host Unix-account
  boundary: relay ACLs constrain relay actions but are not workload isolation;
  [#88](https://github.com/zajca/pohunek/issues/88) owns that later boundary.
  See the [accepted RFC](docs/design/team-relay-control-plane-rfc.md), especially
  its identity, recovery, scheduling, catalog, audit, and dependency sections,
  and [#85](https://github.com/zajca/pohunek/issues/85); neither the relay binary
  nor team mode is shipped yet.
- **PTY/TUI-first.** Agents run in real terminals (Codex, Claude Code, and the
  pinned local Hermes Agent runtime are first-class). Not a re-rendered control
  plane.
- **Remote owner transport is direct over NetBird**, never SSH bridging. Relay
  transport is the separate host-initiated path defined by the accepted RFC.
- **Providers (Linear/GitHub) are shell-out based** (`gh`, Linear GraphQL) and
  live only in client surfaces (CLI scripts, gui-core), never in the daemon.
- **Protocol today:** public protocol v3 is owner-only newline-delimited JSON
  over a Unix socket (local) and TCP on configured overlays (remote); attach
  uses a separate raw-byte connection per PTY. [#70](https://github.com/zajca/pohunek/issues/70)
  owns the coordinated v4 host-link cutover; do not describe relay protocol as
  shipped before that issue lands.

## Repository map

Cargo workspace, edition 2021, MSRV 1.96. Binaries: `pohunek` (CLI),
`pohunekd` (daemon), `pohunek-gui` (GUI).

| Crate | Role |
|-------|------|
| `crates/protocol` | Shared control-protocol envelopes + version negotiation. The wire contract. |
| `crates/client`   | SDK client: typed errors, daemon transport, standalone configured-overlay discovery with bounded probing. |
| `crates/assistant` | Assistant launch orchestration (agent selection, knowledge bundle, launch) and the host connection types (`HostConfig`, `ConnectionOptions`, `connect_client`) shared by the CLI and the GUI. |
| `crates/daemon`   | Host control plane (`pohunekd`): logical registry, worker reconciliation, public protocol, detection/hooks. |
| `crates/worker-protocol` | Private versioned daemon-to-worker protocol and framing. |
| `crates/session-worker` | Durable per-session PTY owner (`pohunek-sessiond`). |
| `crates/cli`      | CLI (`pohunek`): commands over the local protocol. |
| `crates/prompt`   | Shared prompt rendering for provider launch flows. |
| `crates/knowledge`| Knowledge-bundle primitives for the assistant and offline docs. |
| `crates/terminal` | Shared VT screen tracking and attach compositing primitives. |
| `crates/netbird`  | NetBird status parsing, host resolution, bind-address validation. |
| `crates/overlay`  | Provider-neutral overlay contract, configured registry, routing identity, and per-overlay ports. |
| `crates/paths`    | Shared XDG path and local socket contract for daemon, CLI, and GUI clients. |
| `crates/hostcheck`| Host environment probes shared by `doctor` and the daemon's `doctor` RPC. |
| `crates/logging` | Process-safe size rotation and retention for daemon and per-session worker logs. |
| `crates/test-support` | Test-only fixture roots that are symlink-free and short enough for Unix sockets on Linux and macOS, the hermetic per-test `TestEnv`, readiness waits bounded by one hang-guard ceiling (`wait`), the binary-wide unwind-safe process-environment override (`process_env`), the paused-clock auto-advance inhibitor (`time`), and fixture writers (`fs`) that cannot cause `ETXTBSY`. |
| `crates/platform` | Target-neutral process, peer-identity, and native-supervisor contracts plus concrete OS backends. |
| `crates/service-config` | Typed, fail-fast `service.toml` (installation namespace, deadlines, agent environment allowlist) shared by `pohunek service`, `pohunekd`, and `pohunek-sessiond`. |
| `crates/gui-core` | Pure, headless state + SDK bridge for the GUI (no Iced dependency; fully unit-testable). |
| `crates/gui`      | Native Iced shell that wraps `gui-core` in `Task`/`Subscription`. |
| `crates/xtask`    | Workspace automation (docs, TypeScript generation, and pinned Hermes compatibility evidence). |
| `sdk/ts/`         | TypeScript SDK packages in the Bun workspace: `protocol` (generated protocol types), `sdk` (runtime client and runtime-path resolver), and `testkit`. |
| `web/`            | Retained owner-mode WebUI in the same Bun workspace: backend, client core, SPA, and release packaging. |

Other top-level: `compat/` (pinned upstream compatibility locks and sanitized
goldens), `docs/` (architecture, roadmap, phases, knowledge source), `scripts/`
(rofi/sway launchers, release helper).

## Build, test, lint — the gates that must pass

CI runs with `RUSTFLAGS=-D warnings` (warnings are errors). Run these before
considering work done; they mirror CI exactly:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features   # must be clean under -D warnings
cargo build -p pohunek-session-worker --bin pohunek-sessiond  # daemon tests spawn it by path
cargo nextest run --profile ci --test-threads 4 --workspace --all-features
cargo test --doc --workspace --all-features              # nextest excludes doctests
cargo build --workspace --release                        # release profile must build (CI: see below)
cargo xtask docs check                                   # schema/drift/source-map/secrets/runbooks
cargo xtask hermes compatibility --pohunek-bin ABS       # pinned, model-free Hermes CLI/golden gate
```

The `platform contracts (arm64)` CI job runs natively on Apple Silicon macOS
with `MACOSX_DEPLOYMENT_TARGET=14.0`: it checks, lints, and tests
`pohunek-platform` (including the real-launchd suite `tests/launchd.rs`),
`pohunek-paths`, `pohunek-session-worker`, `pohunek-service-config`,
`pohunek-daemon`, and `pohunek-cli`, and fails hard when the runner's
`gui/<uid>` launchd domain is absent. Linux clippy never compiles the
launchd backend, so lint it from Linux with
`cargo clippy --target aarch64-apple-darwin -p <crate> --all-targets -- -D warnings`
before pushing. The real-systemd suites (`crates/platform/tests/systemd.rs`,
`crates/daemon/tests/systemd_durable_worker.rs`,
`crates/cli/tests/service_systemd.rs`) are `#[ignore]`d and need
`POHUNEK_SYSTEMD_E2E=1` plus a running user manager; the Linux CI job
`real systemd supervision` runs all three with `--ignored --test-threads 1`
after building the daemon, worker, and CLI binaries. Run them locally the same
way (`POHUNEK_DAEMON_BIN`, `POHUNEK_WORKER_BIN`, and `POHUNEK_CLI_BIN` point the
suites at the built binaries). The `macOS package install and upgrade
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

Web workspace gates (one Bun workspace root at the repository root spans `sdk/ts/*` and `web/*`):

```bash
bun install --frozen-lockfile
bun run typecheck   # one command: tsc -b source graph + test/frontend/release checks
bun run lint
bun test
bunx playwright install --with-deps chromium  # prerequisite for browser e2e
bun run test:e2e
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
(`sdk/ts/sdk/test/hermes-plugin.e2e.test.ts`), depends on no web package, and
drives the real CLI:

```bash
cargo build -p pohunek-daemon -p pohunek-session-worker -p pohunek-cli
POHUNEK_E2E=1 POHUNEK_DAEMON_BIN=/absolute/path/to/target/debug/pohunekd \
  POHUNEK_CLI_BIN=/absolute/path/to/target/debug/pohunek \
  POHUNEK_PYTHON_BIN=/usr/bin/python3 \
  bun test sdk/ts/sdk/test/e2e.test.ts sdk/ts/sdk/test/hermes-plugin.e2e.test.ts \
  web/backend/test/real-daemon.e2e.test.ts
```

For control-center development, `bun run dev` starts two fixture daemons, the
backend, and the Vite frontend from the repository root; it does not require a Rust daemon or
NetBird. Bun remains the workspace runtime, but `node` must be available on
`PATH` because the orchestrator runs Vite in a Node child process for WebSocket
proxy compatibility; `POHUNEK_NODE_BIN` overrides a nonstandard Node path.

The TypeScript SDK is released as three npm-pack tarballs
(`pohunek-ts-protocol-X.Y.Z.tgz`, `pohunek-ts-sdk-X.Y.Z.tgz`,
`pohunek-ts-testkit-X.Y.Z.tgz`, each with a `.sha256`) built by
`sdk/ts/scripts/pack-release.ts` in the Release workflow's read-only `sdk-pack`
job, which needs `sdk-gate`; the separate `sdk-publish` job attaches them. `bun test sdk/ts/scripts` is the pack contract: it
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

Start from a fast loop, not from the full gate set; run the full gates once
before declaring work done. A warm local timing is not CI evidence: per-shard
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
`relay-db` shard needs the real worker binary and disposable PostgreSQL URL
(`python3 scripts/test-partitions run relay-db`); `cargo t`/`cargo ti` do not.
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
`build-bins` remains a separate default-feature build for the web and Hermes
jobs, because the archive's `--all-features` build enables cli's `test-util`.
In CI, the Postgres-backed
relay job is gated by a paths filter and does not run (its PostgreSQL service
never starts) for non-relay changes.

Doctests and the release build: the `doctests` job runs on
every pull request. The release build (`release-build` job) runs on push to
`main`, the weekly schedule, and manual dispatch, and on a pull request only
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
intermittently is a bug. A test that passes only on a retry is reported as
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
`crates/xtask/tests/hermetic_scan.rs` enforces all of this in test code with no
baseline. A case where the host state is the subject carries
`// hermetic-allowed: #<issue> <reason>` on or directly above its line; the
environment-mutation rule takes no marker, because `ProcessEnv` is the one sanctioned
mechanism.

Rust:

```bash
cargo t                                       # default inner loop: cost-filtered fast unit + integration tests
cargo ta                                      # CPU-saving loop: fast tests of changed crates + dependents
cargo ta --print                              # per-file reasons and the command; runs nothing
cargo t -p pohunek-gui-core                   # fast tests in one crate (alias takes -p)
cargo nextest run --profile local -p pohunek-cli some_test_name  # one test
cargo clippy -p pohunek-daemon --all-targets  # lint one crate
python3 scripts/test-partitions run cli       # exact CI shard: unit/daemon/relay/cli/relay-db/heavy
python3 scripts/test-partitions check         # exhaustive, disjoint nextest inventory check
python3 scripts/test-partitions --archive-file nextest-archive.tar.zst run heavy  # CI mode: no compile
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
everything. Only a reviewed allowlist in `crates/xtask/src/affected.rs` (other
`web/`, `sdk/ts/`, the root Bun workspace files, `docs/`, `.github/`, `.claude/`, `.agents/`, `assets/`, root `*.md`,
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

Web and SDK TypeScript (run from the repository root):

```bash
bun test web/backend/test/real-daemon.e2e.test.ts    # one test file
bun test -t "notifications"                          # only tests matching the name pattern
bun test sdk/ts/sdk/test/config.test.ts -t "one case" # file plus name pattern
cd sdk/ts/sdk && bun run typecheck                   # one package's tsc -b graph
```

Local toolchain: `scripts/dev-bootstrap` checks the tools these loops need
against their minimum versions and prints the exact install command for each
missing or too-old one. `rustc` (the workspace `rust-version`), `cargo-nextest` (minimum from `.config/nextest.toml`)
and `python3` >= 3.11 are required; `bacon`, `hyperfine`
(`scripts/measure-dev-loop`), and `mold` (CI linker only; checked on Linux only) fail only with
`--strict`. It never installs anything; run the printed fixes yourself:

```bash
scripts/dev-bootstrap            # report; non-zero if a required tool fails
scripts/dev-bootstrap --strict   # optional tools fail the run too
```

Watcher (`bacon.toml` at the repo root; optional tool, no gate depends on it —
install with `cargo install --locked bacon`). Inside bacon, `e` switches to
`check`, `n`/`t` to `nextest-fast`, `a` to `affected`, and `c` to `clippy-fast`:

```bash
bacon                      # default job `nextest-fast`: profile-fast nextest loop
bacon check                # `cargo check` over all targets and features only
bacon clippy-fast          # CI lint command
bacon affected             # `cargo xtask affected` on every save
bacon nextest-fast -- -p pohunek-gui-core  # narrow the loop to one crate
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
scripts/ci-timings junit --label "tests (unit, fast)" --run RUN_ID junit-unit.xml
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
--pohunek-bin ABS`) exercises the pinned CLI and plugin surface:
list/enable/disable, supported tool/skill/hook registration, profile/home target
resolution, and Hermes integration install/status/doctor/uninstall. It must use
an isolated profile and must never start a model turn, access an operator profile
or `state.db`, read credentials, or download a runtime. A missing pinned
executable is a failure, never a green skip. Release-archive verification uses
`packaging/smoke-hermes-plugin-release` with an explicitly supplied preinstalled
pinned executable; it proves the extracted CLI embeds the plugin and generated
skill without a source-tree asset path. Hermes PTY goldens are refreshed
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
optionally `cargo +nightly udeps --workspace --all-targets --all-features`
(CI runs it only on `main`, the weekly sweep and manual dispatch, as the
backstop for a dependency referenced only in code compiled out for the host).
Suppress a verified `cargo shear` false positive with
`[package.metadata.cargo-shear] ignored = ["<crate>"]` and a reason comment.
Note `knowledge` gates its protocol bridge behind a `protocol`
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
- **Keep shell completion synchronized with the CLI.** The clap command tree is
  the source of truth for generated Bash, Zsh, and Fish completion; never
  hand-maintain generated shell scripts. Every command, subcommand, flag, or
  argument change must also review `crates/cli/src/completion.rs`, update any
  affected dynamic value completers (especially `--host` and session `target`
  arguments), and extend completion/parser tests. Preserve the completion
  safety contract: static generation performs no daemon or NetBird I/O, while
  dynamic lookups are opt-in, deadline-bounded, do not autostart the daemon, and
  fail silently. In the same change, update the CLI tables/examples in
  `README.md`, matching `docs/knowledge/` guidance and source map entries, and
  release/install coverage when those surfaces change. Run at least
  `cargo test -p pohunek-cli` and `cargo xtask docs check` before the full gates.
- Comments and all repository text are in **English**.

The first complete team-relay release intentionally trusts collaborators at the
host Unix-account boundary. Relay API ACLs are authorization controls, not
command or hostile-workload isolation: direct-host profiles run under the
daemon owner's account. Real profile-backed container and VM isolation is a
post-release track owned by
[#88](https://github.com/zajca/pohunek/issues/88); do not call the relay track a
PoC or imply that current direct-host execution is a hostile-workload sandbox.

## Workflow

- Work on a branch off `main`; do not commit directly to `main`. Commit/push
  only when the user asks.
- **Work is tracked in GitHub Issues and the Pohunek Project**, not in local
  planning files. GitHub Issues are the canonical record for scope, design
  proposals and decisions, acceptance criteria (DoD items with stable IDs —
  `D1`, `D2`, ...), plans, verification evidence, blockers, and handoffs; the
  Pohunek Project tracks delivery status (`Todo` / `In Progress` / `Done`).
  `NEXT.md` is no longer an authority and no workflow requires it;
  `docs/design/` stays for accepted long-lived technical design. The shared
  rules — issue resolution, deduplication, body/comment structure, project
  status semantics, and safe persistence — live in the **`github-workflow`
  skill** (`.claude/skills/github-workflow/`) with configuration in
  `.github/agent-workflow.json`; every agent must follow it for any GitHub
  issue/comment/project write.
- **Gate meaningful work on an issue first.** Before meaningful work starts,
  resolve its issue: use an explicit issue URL/number when given, otherwise
  deduplicate against existing issues. When the user gives a concrete new
  scope and no matching issue exists, **auto-create** the issue and add it to
  the configured project without a further ask. Issue/comment/project writes
  on the configured repository are a standing owner authorization: never ask
  whether to file an issue, and file verified defects noticed incidentally
  too (see the `github-workflow` skill). Ask only when the scope is
  genuinely ambiguous or competing issues both plausibly cover it. Verified
  out-of-scope findings get their own follow-up issue automatically, but an
  unmet original DoD item never moves to a follow-up to claim the issue done.
- **The milestone loop runs against a GitHub issue.** Development moves one
  milestone at a time. The loop is: the phase plan and its DoD land as a
  GitHub issue (`plan-phase`) → implement it in a fresh worktree off `main`
  (`milestone`) → review the branch against the issue's DoD
  (`milestone-review`) → land it locally via `merge-advance` or publish it
  via `pr-handoff`, with the issue and project updated accordingly (issue
  closure + `Done` only after verified landing on the remote default branch
  for any repository change; only pure planning/investigation with no
  repository change completes on the issue-held artifact). Do not create new
  local `NEXT.md`/RFC files as work-management authorities; a pre-existing
  untracked `NEXT.md` is a read-only migration source at most.
- **Plans are end-to-end complete.** Do not propose or build PoCs, minimal
  versions, or phased-minimal shortcuts unless the user explicitly asks for
  reduced scope. Plan and implement the full solution.
- **Ship a stack of small sequential PRs, not one large PR.** Split the work
  into ordered slices, one coherent concern each, stacked branch on branch and
  landed bottom-up. Each PR is complete for its slice — builds, passes the
  full gate set alone, has tests, updates `docs/knowledge/` for surfaces it
  touches — with no stubs a later PR fills in; the stack as a whole delivers
  the full DoD, so slicing is not a PoC shortcut. The slice plan is recorded on
  the issue first. Size cue and mechanics: `pullRequests` in
  `.github/agent-workflow.json` and the `pr-handoff` skill.
- **Commits are never signed.** Use clean, concise, English messages. Do not add
  a `Co-Authored-By` trailer or any "generated with" footer.
- Keep changes scoped. If you touch the wire protocol (`crates/protocol`), expect
  ripples in `client`, `daemon`, `cli`, and `gui-core` — update and test all.
- Run the full gate set above before declaring done. Report failures honestly
  with output; never claim green without running it.
- When a task spans 3+ steps, plan first and verify after each major step.

## Accepted harness trade-offs (operator decisions)

The milestone loop in `.lh-harness/workflows/` deliberately trades partial
security for autonomy. These are recorded operator decisions, not oversights;
do not re-report them as review findings:

- A harness run whose final audit reports every DoD item met with the full
  gate set green **pre-authorizes** the `pr-handoff` flow (commit, push,
  open PR) without a further ask. This intentionally supersedes the default
  "commit/push only when the user asks" for that one path; it is publishing
  authorization only and does not authorize merging.
- Invoking the `deliver-issue` skill on an issue is the owner's explicit
  request to commit, push, open that issue's PRs, and merge them once the
  skill's merge criteria hold (green checks and a review of the final head
  with no unanswered actionable finding). It covers that one issue only and
  never releases, tags, or force-pushes `main`.
- Harness executors run their model CLI unsandboxed on the operator's host;
  `--workspace` scopes the working directory, not file access.

## Pointers

- `docs/architecture.md` — authoritative design and scope (read this first).
- `docs/ROADMAP.md`, `docs/phases/` — direction and historical context.
- `docs/public-api.md` — the SDK/CLI contract surface.
- `docs/knowledge/` — offline knowledge source built by `cargo xtask docs`.
- `docs/gui-review.md` — current GUI review findings and refactor backlog.
- `.agents/rust-guidelines/` — vendored Microsoft Pragmatic Rust Guidelines
  (read before editing `.rs`; `SKILL.md` routes you to the right file,
  `VENDORED.md` documents the source and how to re-sync).
- `scripts/harness-milestone` starts the milestone-build harness run:
  `scripts/harness-milestone <slug> --issue NUMBER|URL` (repository/default
  project resolved from `.github/agent-workflow.json`, the issue fetched and
  validated with `gh` before any git state changes; no NEXT.md). Its
  executable tests run with
  `python3 -m unittest discover -s scripts/tests -p 'test_*.py'`.
- `.lh-harness/workflows/` — milestone-build harness instructions
  (issue-driven; `.github/agent-workflow.json` holds the work-tracking
  config).
- `.claude/skills/` — milestone-loop skills, including the shared
  `github-workflow` tracking rules.
- `README.md` — install, quick start, trust boundary.

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
NetBird/WireGuard address. This repository ships no user interface: every GUI,
web control center, and launcher is a separate client in
[`zajca/pohunek-work`](https://github.com/zajca/pohunek-work). The accepted
future direction adds an optional trusted team relay without replacing these
direct owner paths; it is not implemented yet.

It is pre-1.0 and experimental: wire shapes, config files, and on-disk metadata
may change freely. **Do not add backward-compatibility shims** unless asked,
except inside the upgrade window below.

### Upgrade window

The contract: release N stays compatible with release N-1 in exactly four
places, and nowhere else; everything outside this window keeps the no-shim
rule. The current state of each place is stated separately.

- **Live workers.** Contract and state: the private worker protocol accepts
  `PREVIOUS_VERSION` as well as `CURRENT_VERSION`
  (`crates/worker-protocol/src/version.rs`), so a daemon update leaves running
  sessions alone.
- **Worker journal.** Contract: the daemon reads the previous journal schema.
  State: the readable schemas are the explicit list
  `WORKER_JOURNAL_READABLE_SCHEMAS` (`crates/daemon/src/runtime/lifecycle.rs`);
  a schema is listed only when its layout names the worker generation, so today
  only the current `WORKER_JOURNAL_SCHEMA_VERSION` (4) is readable. Any other
  schema gets a typed reason and a WARN.
- **Public-protocol clients.** Contract: a protocol change keeps the previous
  version served through adapters on both sides of the connection. An adapter
  translates shape only. A semantic change raises `MIN_PROTOCOL_VERSION` too
  and is an announced break; the oldest adapter is deleted on the next bump.
  State: the daemon accepts `3..=4` (`MIN_PROTOCOL_VERSION` is
  `PROTOCOL_VERSION - 1`, adapter in `crates/protocol/src/compat/v3.rs`) and the
  Rust client, the CLI and the TypeScript SDK advertise the same window
  (`CLIENT_PROTOCOL_VERSIONS` is `SUPPORTED_PROTOCOL_VERSIONS`), translating
  through the same adapter (ported to TypeScript in `sdk/ts/sdk/src/compat.ts`,
  tested against the same recordings) when the daemon answers in N-1. A method
  new in N fails on the client with `daemon/daemon_protocol_too_old` before it
  is sent ([#527](https://github.com/zajca/pohunek/issues/527)).
- **Persisted daemon state.** Contract and state: `metadata.jsonl` migrates
  from any older kept schema, because skipping releases is normal with
  `update-pohunek`. A persisted shape change requires a schema bump and a
  migration step.

Every persisted record that cannot be migrated or adopted is logged at WARN,
surfaced to the operator, and carries a recovery hint. A store newer than the
binary, or older than every kept migration, makes the daemon refuse to start
and leaves the store untouched.

The operator-facing gate of the window is the upgrade preflight: `pohunek
service check|upgrade` run `pohunekd upgrade-preflight` of the new archive
before any effect. It judges the dry-run store migration and every live session
(`adoptable`, `would_lose_recovery`, `would_not_be_adopted`) from the store,
worker journals and process table only (`crates/service-config/src/preflight.rs`,
`crates/daemon/src/session/reconcile/upgrade_preflight.rs`,
`crates/daemon/src/store/dry_run.rs`); it never connects to a worker socket and
never writes. Sessions at risk refuse the upgrade unless `--accept-runtime-loss`,
the single loss-accepting flag, is given; a refused store never is overridable.

`scripts/release` compares these constants with the previous tag and requires
a release-notes line for each changed one (see the `release` skill).

#### Store schema and the shape guard

Every line of `metadata.jsonl` (session, resume, worktree and project records)
carries `schema_version`; a line without it is schema 1. The current schema is
`STORE_SCHEMA_VERSION` in `crates/daemon/src/store/schema.rs`. The daemon runs
`store::migrate_at_startup` in `crates/daemon/src/main.rs` before the session
registry exists, and copies the store to `<store>.pre-schema-<old>` before the
first migrating write.

The guard test in `crates/daemon/src/store/shape_guard.rs` compares the
serialized field set of every persisted record kind, including the nested
`SessionInfo` protocol type, with the snapshot
`crates/daemon/src/store/fixtures/shape/schema-<STORE_SCHEMA_VERSION>.txt`. It
fails when a field is added, removed or renamed without a schema bump, and when
an older schema has no migration step. A protocol field change in `SessionInfo`
trips it on purpose: the field lands in `metadata.jsonl`. To resolve a failure:

1. bump `STORE_SCHEMA_VERSION`;
2. add a step for the previous schema to `MIGRATIONS` in `schema.rs`;
3. add `fixtures/shape/schema-<new>.txt` (the failure prints the key set) and
   register it in `SHAPE_SNAPSHOTS`;
4. freeze a store written by the previous release as a fixture under
   `crates/daemon/src/store/fixtures/` and test that it migrates.

Authoritative design lives in `docs/architecture.md` (it wins over `idea.md`).
Hard constraints, decided on purpose — respect them in every change:

- **Direct owner operation stays first-class.** Standalone and direct-overlay
  clients keep talking to each host daemon without a relay dependency. Their
  trust boundary remains owner-only socket/file permissions plus the configured
  overlay, with NetBird as the production provider.
- **The owner browser path stays first-class.** The web control center is an
  external client (`zajca/pohunek-work`) that reaches the owner protocol through
  a private local/NetBird gateway. It is not replaced by the relay and must
  never become a second team-auth or relay-routing authority.
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
- **Core ships no UI.** The deliverables are the daemon (`pohunekd`), the
  session worker (`pohunek-sessiond`), the CLI (`pohunek`), the relay
  (`pohunek-relayd`), the Rust crates, and the TypeScript SDK workspace
  (`sdk/ts/`). No GUI, web frontend or backend, desktop launcher, or UI
  packaging belongs here; they live in `zajca/pohunek-work`.
- **The contract boundary is public contracts only.** A client reaches core
  through the CLI with `--json` or through the public protocol via the
  versioned SDKs: the Rust crates pinned by git tag and the TypeScript SDK
  release tarballs pinned by URL and integrity. The core crates a Rust client
  links (`client`, `protocol`, `paths`, `platform`, `prompt`, `knowledge`,
  `assistant`) are a pinned API, not a stable one: they stay pre-1.0 with no
  back-compat shims, and a client absorbs breaking changes when it bumps its
  pinned tag.
- **UIs pin the core release they were built against.** A Rust or
  TypeScript client advertises the window `CLIENT_PROTOCOL_VERSIONS` and reaches
  an N-1 daemon through its adapter; a client two releases newer than its daemon
  is refused (`daemon/version_mismatch`). A daemon
  of release N additionally serves the clients of release N-1 through the
  protocol window (`docs/architecture.md` "Protocol versioning"), which bounds
  how long a UI release trails a core update. A protocol surface that only UI
  clients call (`host.discover`, `worktree.remove`, and the UI use of
  `subscribe`) is still a public obligation: core keeps server-side contract
  tests for it, listed in `docs/public-api.md` ("External clients"). Core adds
  no CLI command only to serve a UI.
- **Issue/PR providers (Linear, GitHub) live only in `zajca/pohunek-work`,
  never in core** (neither the daemon, the CLI, nor this repository's scripts).
- **Protocol today:** public protocol v4 (the daemon also accepts v3 through the
  protocol window) is owner-only newline-delimited JSON
  over a Unix socket (local) and TCP on configured overlays (remote); attach
  uses a separate raw-byte connection per PTY. [#70](https://github.com/zajca/pohunek/issues/70)
  owns the coordinated v4 host-link cutover; do not describe relay protocol as
  shipped before that issue lands.

## Repository map

Cargo workspace, edition 2021, MSRV 1.96. Binaries: `pohunek` (CLI),
`pohunekd` (daemon).

| Crate | Role |
|-------|------|
| `crates/protocol` | Shared control-protocol envelopes + version negotiation. The wire contract. |
| `crates/package`  | Canonical runtime package archive (deterministic `tar.zst` builder, strict reader, size limits, package digest), the signed runtime catalog verifier, the trust anchor file parser and catalog signing, descriptor-relative extraction into verified package roots with per-file re-verification, the owner-private registry store (with tampered-root removal and the persisted catalog high-water/revocation state), the directory archive builder used by `package build` and `plugin link`, and (in the daemon) the `PackageSource` that serves installed packages as runtimes. |
| `crates/client`   | SDK client: typed errors, daemon transport, standalone configured-overlay discovery with bounded probing. |
| `crates/assistant` | Assistant launch orchestration (agent selection, knowledge bundle, launch) and the host connection types (`HostConfig`, `ConnectionOptions`, `connect_client`) shared by the CLI and other clients. |
| `crates/daemon`   | Host control plane (`pohunekd`): logical registry, worker reconciliation, public protocol, detection/hooks. |
| `crates/worker-protocol` | Private versioned daemon-to-worker protocol and framing. |
| `crates/session-worker` | Durable per-session PTY owner (`pohunek-sessiond`). |
| `crates/cli`      | CLI (`pohunek`): commands over the local protocol. |
| `crates/prompt`   | Shared prompt rendering for provider launch flows. |
| `crates/knowledge`| Knowledge-bundle primitives for the assistant and offline docs. |
| `crates/terminal` | Shared VT screen tracking and attach compositing primitives. |
| `crates/netbird`  | NetBird status parsing, host resolution, bind-address validation. |
| `crates/overlay`  | Provider-neutral overlay contract, configured registry, routing identity, and per-overlay ports. |
| `crates/paths`    | Shared XDG path and local socket contract for daemon, CLI, and other clients. |
| `crates/hostcheck`| Host environment probes shared by `doctor` and the daemon's `doctor` RPC. |
| `crates/logging` | Process-safe size rotation and retention for daemon and per-session worker logs. |
| `crates/test-support` | Test-only fixture roots that are symlink-free and short enough for Unix sockets on Linux and macOS, the hermetic per-test `TestEnv`, readiness waits bounded by one hang-guard ceiling (`wait`), the binary-wide unwind-safe process-environment override (`process_env`), the `workers` guard that reaps and fails on leaked `pohunek-sessiond` workers of a fixture root, the paused-clock auto-advance inhibitor (`time`), and fixture writers (`fs`) that cannot cause `ETXTBSY`. |
| `crates/platform` | Target-neutral process, peer-identity, and native-supervisor contracts plus concrete OS backends. |
| `crates/service-config` | Typed, fail-fast `service.toml` (installation namespace, deadlines, agent environment allowlist) shared by `pohunek service`, `pohunekd`, and `pohunek-sessiond`; also the upgrade preflight report that the installer and the daemon exchange. |
| `crates/xtask`    | Workspace automation (docs, TypeScript generation, pinned Hermes compatibility evidence, runtime package archives, and the `catalog` release tooling that builds, signs with a caller-supplied key file and verifies the runtime catalog and its trust anchor). |
| `sdk/ts/`         | TypeScript SDK packages in the Bun workspace: `protocol` (generated protocol types), `sdk` (runtime client and runtime-path resolver), and `testkit` (fixture daemon and loopback test relay). |

Other top-level: `runtime-packages/` (source of the official runtime packages, one
directory per runtime holding exactly its archive content; see its README),
`compat/` (pinned upstream compatibility locks and sanitized
goldens), `docs/` (architecture, roadmap, phases, knowledge source), `scripts/`
(release helper, CI and dev tooling; the rofi/sway launchers live in
`zajca/pohunek-work`).

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
cargo xtask docs check                                   # schema/drift/source-map/verifier/secrets/runbooks
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
suites at the built binaries). The `upgrade from the previous release` CI job (`.github/workflows/upgrade.yml`,
called by `ci.yml` when the daemon, worker, protocol, or CLI crates change, and
by `release.yml` before any publishable binary is built) installs the previous
release's published Linux binaries (checksum verified, never rebuilt), starts a
session with a fake agent running that release's managed hooks, upgrades the
service to this build with `pohunek service check` and `service upgrade`, and
asserts the session stays live on the same worker, `screen`, `input`, hooks and
notifications work, `stop` then `resume` relaunch with the native reference, and
the previous release's CLI still talks to the new daemon
(`crates/cli/tests/release_upgrade.rs`). It uploads the previous release's store
and protocol outputs as the `upgrade-goldens-<tag>` artifact. Run it on any pair
with `gh workflow run upgrade.yml -f previous_tag=v0.33.0 -f head_ref=v0.33.1`
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
drives the real CLI:

```bash
cargo build -p pohunek-daemon -p pohunek-session-worker -p pohunek-cli
POHUNEK_E2E=1 POHUNEK_DAEMON_BIN=/absolute/path/to/target/debug/pohunekd \
  POHUNEK_CLI_BIN=/absolute/path/to/target/debug/pohunek \
  POHUNEK_PYTHON_BIN=/usr/bin/python3 \
  bun test sdk/ts/sdk/test/e2e.test.ts sdk/ts/sdk/test/hermes-plugin.e2e.test.ts
```

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
parity with the built-in Codex files, the supported range against the lock, and
the manifest on real screens, plus `#[ignore]`d tests that drive a real `codex`
with a loopback Responses stub and a fresh `CODEX_HOME`. The daemon-backed
tests install the built archive through a signed catalog with a throwaway key
and trust anchor, so the real-Codex tests launch through the installed package
and its version probe; the fixture kills every process it started, also when a
test fails. Run them with
`POHUNEK_CODEX_E2E=1 cargo test -p pohunek-cli --test codex_package --
--include-ignored --test-threads 1`; the `codex-package` CI job installs the
locked release from npm. Never point a manual run at `~/.codex`.

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
`build-bins` remains a separate default-feature build for the SDK and Hermes
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
cargo t -p pohunek-daemon                     # fast tests in one crate (alias takes -p)
cargo nextest run --profile local -p pohunek-cli some_test_name  # one test
cargo clippy -p pohunek-daemon --all-targets  # lint one crate
python3 scripts/test-partitions run cli       # exact CI shard: core/daemon/relay/cli/relay-db/heavy
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

## Testing policy

Every test is an **integration scenario** or an **E2E scenario** — in the Rust and
TypeScript workspaces, the Python script tests, and each suite, with no unit-test
tier: writing a new unit test is prohibited. This project rule takes precedence over
personal global instructions that ask for tests for all new code
(`~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`); the durability, security,
compatibility, timing, flaky, and hermetic obligations elsewhere in this file are
unchanged.

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
work done, run the applicable gate set from "Build, test, lint" once, on the final
revision — a later fix re-runs the checks whose inputs it changed, unchanged checks
keep their evidence, and CI is the final landing evidence (a gate that cannot run
locally is CI-only, never green).

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
- **Shared logic goes in a library crate, not a binary.**
- **Tests follow the Testing policy above.** A test drives cooperating
  production components through a supported boundary (integration) or the real
  product processes (E2E); never a new unit test. File placement (`tests/` vs
  inline `#[cfg(test)]`) follows the boundary a scenario exercises, not a
  unit/component tier, and a test does not expose a private API only to move
  it (see "Testing policy" for the migration state of the existing unit-tier
  tests). Extending the protocol or state-machine suites is still the first
  option for behavior they own, but an added scenario must itself meet the
  integration/E2E boundary — appending to an existing suite does not exempt a
  test from it, and never adds unit-tier tests to those suites.
- **Keep the assistant knowledge bundle current.** `docs/knowledge/` is the
  hand-authored source for the Universal Pohunek Assistant (materialized via
  `assistant.materialize`). Whenever a change alters something the bundle
  describes — a CLI command or flag, a protocol method/event, an
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
  arguments), and keep the completion scenarios current where the change adds
  or alters completable behavior (see "Testing policy"). Preserve the completion
  safety contract: static generation performs no daemon or NetBird I/O, while
  dynamic lookups are opt-in, deadline-bounded, do not autostart the daemon, and
  fail silently. In the same change, update the CLI tables/examples in
  `docs/cli.md`, matching `docs/knowledge/` guidance and source map entries, and
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
  skill** (`.agents/skills/github-workflow/`) with configuration in
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
  applicable full gate set alone, carries the behavioral scenarios its change
  needs under "Testing policy", updates `docs/knowledge/` for surfaces it
  touches — with no stubs a later PR fills in; the stack as a whole delivers
  the full DoD, so slicing is not a PoC shortcut. The slice plan is recorded on
  the issue first. Size cue and mechanics: `pullRequests` in
  `.github/agent-workflow.json` and the `pr-handoff` skill.
- **Commits are never signed.** Use clean, concise, English messages. Do not add
  a `Co-Authored-By` trailer or any "generated with" footer.
- Keep changes scoped. If you touch the wire protocol (`crates/protocol`), expect
  ripples in `client`, `daemon`, and `cli`, and into `docs/public-api.md` and the
  `docs/knowledge/` bundle — update all of them and run their checks. A non-additive wire change
  (rename, moved field) bumps `PROTOCOL_VERSION`, adds the edge adapter for the
  version it leaves behind in `crates/protocol/src/compat/` with golden fixtures
  and consumer recordings produced by that release's own code, and deletes the
  oldest adapter in the same change. A semantic change (new required parameter,
  removed method or event, changed error code or meaning) cannot be adapted: it
  also raises `MIN_PROTOCOL_VERSION` and is announced as a break. A method added after the previous release is listed in the adapter's `INTRODUCED_METHODS` so older connections get `method_not_found`.
- Run the applicable full gate set once on the final revision before declaring
  done; iterate on affected checks in between (see "Testing policy"). Report
  failures honestly with output; never claim green without running it.
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
- `.agents/skills/` — canonical home of the milestone-loop skills, including
  the shared `github-workflow` tracking rules. `.claude/skills/<name>` is a
  relative symlink to `../../.agents/skills/<name>` so Claude Code discovers
  the same skills; edit only the canonical tree under `.agents/skills/`.
- `README.md` — what pohunek is, the agent install prompt, the pohunek-work
  experiments, and the roadmap sketch. Reference material lives in
  `docs/features.md`, `docs/install.md`, `docs/cli.md`, `docs/sdk.md`, and
  `docs/development.md`.

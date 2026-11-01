---
type: SourceMap
id: assistant/source-map
title: Assistant source map
description: Existing repository paths that matter when verifying assistant behavior, client behavior, daemon behavior, project handling, and protocol contracts.
source_kind: manual
intents: [setup, project, update, debug, help]
---

# Assistant Source Map

Use this map when the knowledge bundle is not precise enough and exact current
implementation behavior must be verified against the source tree.

Current CLI and command surface:

- `crates/cli/src/lib.rs`
- `crates/cli/src/main.rs`
- `crates/cli/src/completion.rs`
- `docs/cli.md` — human CLI reference: command table and examples.
- `crates/cli/src/commands/mod.rs`
- `crates/cli/src/commands/agent_skill.rs`
- `crates/cli/src/commands/assistant/mod.rs`
- `crates/cli/src/commands/assistant/bootstrap.rs`
- `crates/cli/src/commands/doctor.rs`
- `crates/hostcheck/src/lib.rs` — shared doctor probe list (Linux) and platform dispatch.
- `crates/hostcheck/src/macos.rs` — macOS doctor checks and the bounded `launchctl` domain probe.
- `crates/hostcheck/src/executable.rs` — exec-bit-aware executable resolution.
- `crates/cli/src/commands/daemon.rs`
- `crates/cli/src/commands/service.rs` — `pohunek service install|upgrade|uninstall|status|check|lock`.
- `crates/cli/src/service/` — the service transactions: install journal and
  transaction lock with the holder record `pohunek service lock` publishes
  (`record.rs`), the holder token its command adopts the lock with through
  `POHUNEK_SERVICE_LOCK_TOKEN` (`inherited.rs`), versioned
  layout and GC (`layout.rs`, `usage.rs`), installer values written to
  `service.toml` (`settings.rs`), the preflight `service check` shares with
  install and upgrade (`engine.rs`, `mod.rs`), and the stable `--json` shapes
  (`report.rs`).
- `crates/cli/src/commands/attach.rs`
- `crates/cli/src/commands/health.rs`
- `crates/cli/src/commands/session.rs`
- `crates/cli/src/commands/project.rs`
- `crates/cli/src/commands/notifications.rs`
- `crates/cli/src/commands/host_fanout.rs`
- `crates/cli/src/commands/discovery_cache.rs`
- `crates/cli/src/commands/setup.rs`
- `crates/cli/src/commands/setup_host.rs`
- `crates/cli/src/commands/host.rs`
- `crates/cli/src/commands/integration.rs`
- `crates/cli/src/commands/prompt.rs`
- `crates/cli/src/client.rs`
- `crates/cli/src/target.rs`
- `crates/cli/src/paths.rs`
- `crates/cli/src/error.rs`
- `crates/cli/tests/agent_skill.rs`
- `crates/cli/tests/prompt_link.rs`
- `crates/cli/tests/standalone_discovery.rs`
- `crates/cli/tests/session_process_api.rs`
- `crates/cli/tests/session_idle_stop.rs` — `session policy` idle-stop default against a real daemon.
- `crates/cli/tests/session_wait_turn.rs` — `session wait` turn mode against a real daemon.

Assistant launch and prompt rendering:

- `crates/assistant/src/lib.rs`
- `crates/assistant/src/host.rs`
- `crates/assistant/src/error.rs`
- `crates/assistant/src/launch.rs` — launch orchestration, the launch snapshot
  (launch metadata only), and prompt composition. The prompt's safety rules and
  navigation steps are rendered from `docs/knowledge/assistant/system.md`.
- `crates/assistant/tests/select_agent.rs`
- `crates/assistant/tests/materialize_newer_daemon.rs`
- `crates/assistant/tests/prompt_contract.rs` — the launched prompt carries every
  safety rule and navigation step of `system.md`.
- `crates/prompt/src/lib.rs`
- `crates/prompt/src/link.rs`

Knowledge bundle build, embed, and materialization:

- `crates/knowledge/build.rs` — assembles the embedded bundle and its content hash.
- `crates/knowledge/src/schema.rs` — concept types and frontmatter fields.
- `crates/knowledge/src/validation.rs` — bundle validation (frontmatter, ids,
  links, reserved files).
- `crates/knowledge/src/prompt_contract.rs` — shape of the `## Safety` and
  `## Navigation` sections of `assistant/system.md`, shared by validation and launch.
- `crates/knowledge/src/index.rs` — the intent-filtered concept index the prompt's
  table of contents is built from.
- `crates/knowledge/src/protocol_conversions.rs` — concept metadata as it crosses
  the `assistant.materialize` wire.
- `crates/knowledge/src/materializer.rs` — safe extraction of the embedded bundle.
- `crates/knowledge/src/assistant.rs` — bundle version, content hash, and launch ids.
- `docs/knowledge/concepts/host-governance.md`

TypeScript SDK and test relay (the web control center is an external client in
`zajca/pohunek-work`):

- `sdk/ts/sdk/src/index.browser.ts`
- `sdk/ts/sdk/README.md`
- `sdk/ts/sdk/src/client.ts`
- `sdk/ts/sdk/src/envelope.ts`
- `sdk/ts/sdk/src/origin.ts`
- `sdk/ts/sdk/src/transport.ts`
- `sdk/ts/sdk/src/runtime-paths.ts`
- `sdk/ts/testkit/src/runtime-root.ts`
- `sdk/ts/testkit/src/fixture-daemon.ts` — the stateful fixture daemon, including
  its `session.wait_turn` model (scenario turn signals, `exited` on stop,
  cursor placement, progress) and the fidelity limits it documents.
- `sdk/ts/testkit/src/bun-relay.ts` — Bun-only loopback WebSocket relay that the
  SDK transport tests run against over real sockets.
- `sdk/ts/scripts/typecheck.ts` — one-command typecheck: incremental `tsc -b`
  over the composite source-only project graph (`protocol`/`sdk`/`testkit`) in
  the root `tsconfig.json`, then the standalone `sdk-release` check and the
  per-package `test/tsconfig.json` projects; tests stay out of the composite
  graph because their sibling-package imports resolve to `.ts` sources.
- `docs/knowledge/guides/ts-sdk.md`
- `docs/design/track-b-web-control-center-plan-2026-07-22.md`
- `docs/phases/04-browser-control-center.md`

Team relay status. Implemented means the issue of the team-relay delivery list
(RFC section 23, umbrella [#56](https://github.com/zajca/pohunek/issues/56)) is
closed as completed; deferred means it is still open:

- Implemented: direct-overlay foundation (#69), RFC landing (#80), stable host
  identity (#81), the reduced relay foundation (#85: PostgreSQL fencing and
  recovery, protected provisioning, generic OIDC browser and device
  authentication, bounded HTTPS account and credential lifecycle, native
  credential CLI), Google and GitHub team admission with bounded revalidation
  (#92), provider-neutral account linking (#107), and the RFC reconciliation (#91).
- Deferred: host enrollment and the userspace WireGuard link (#72), protocol v4
  host link (#70), `HostShare` enforcement (#82), session visibility and sharing
  (#83), snapshots and subscriptions (#84), routing, aggregation, attach and the
  typed public API (#71), the complete team administration API (#108), team
  clients and web (#86), operational hardening (#87), and the post-release
  webhook delivery (#73) and runtime-backed profiles (#88).

- `docs/design/team-relay-control-plane-rfc.md`
- `docs/architecture.md`
- `docs/ROADMAP.md`
- `docs/public-api.md`
- `docs/knowledge/concepts/team-relay.md`
- `docs/knowledge/safety/trust-model.md`
- `docs/knowledge/guides/remote-hosts.md`
- `docs/knowledge/guides/tailscale.md`
- `docs/knowledge/guides/ts-sdk.md`
- `crates/relay-protocol/src/`
- `crates/relay-protocol/src/link.rs` — typed account-link contracts: channel,
  state, safe revisioned record and page, browser/device start, poll request and
  result, cancel and unlink requests, and the identity-removal result. No type
  here carries a provider profile attribute.
- `crates/relay/migrations/`
- `crates/relay/migrations/0004_account_linking.sql` — the durable account-link
  guarantees: monotonic `principals.account_link_generation`, active
  `(issuer, subject)` uniqueness for identities that are not removed, link-once
  identity provenance, one pending transaction per account, one unconsumed
  provider row per transaction, immutable transaction provenance, and
  terminal-state transition enforcement.
- `crates/relay/src/auth/service/link.rs` — the account-link lifecycle: browser
  and device start, device poll, browser-callback commit, status page, cancel,
  and unlink, with the current-actor, generation, possession, collision, and
  audit checks each transition makes.
- `crates/relay/src/operator.rs`
- `crates/relay/src/lifecycle.rs`
- `crates/relay/src/lifecycle/local.rs`
- `crates/relay/src/recovery/mod.rs`
- `crates/relay/src/config.rs`
- `crates/relay/src/store/`
- `crates/relay/src/auth/`
- `crates/relay/src/authorization/`
- `crates/relay/src/admission/`
- `crates/relay/src/server/mod.rs`
- `crates/relay/src/runtime.rs`
- `crates/relay/src/bin/pohunek-relayd.rs`
- `crates/relay-client/src/`
- `crates/cli/src/commands/relay.rs`
- `crates/cli/src/commands/relay/`

Shipped host-local identity and governance:

- `crates/protocol/src/governance.rs`
- `crates/protocol/src/method.rs`
- `crates/daemon/src/governance.rs`
- `crates/daemon/src/host_state/`
- `crates/daemon/src/doctor.rs`
- `crates/daemon/src/api/handler/governance.rs` — strict null-only
  `host.governance.inspect` parameters, safe public projection, and the fixed
  redacted governance-unavailable response.
- `crates/daemon/src/api/handler/mod.rs` — central public method dispatch for
  the shared owner-transport governance service.
- `crates/client/src/transport.rs`
- `crates/cli/src/commands/host.rs`
- `crates/xtask/src/generators/protocol.rs`
- `sdk/ts/sdk/src/governance.ts` — strict TypeScript validation of the public
  `host.governance.inspect` response and its fixed redacted contract-mismatch
  error.
- `docs/knowledge/concepts/host-governance.md`

Release packaging and contributor verification:

- `.config/nextest.toml` — shared fast cost boundary, bounded heavy profile,
  the serial `package` profile of the runtime package jobs (no retries, a
  per-test timeout, JUnit report), and the flaky-test policy
  (`flaky-result = "fail"`; retries on `heavy` and `relay-db` only).
- `.cargo/config.toml` — `cargo t`/`cargo ti` select fast unit and integration
  tests; `cargo tw` keeps the full suite; `cargo ta` runs `cargo xtask
  affected`. Fast loops do not replace full gates.
- `crates/xtask/src/affected/` — `cargo xtask affected`: maps changed,
  staged, unstaged, and untracked files to workspace packages, closes them over
  their reverse dependencies (a dev-dependency for one hop), and runs `cargo t`
  with an explicit `package(=..)` filterset; a manifest or lockfile change goes
  through a `cargo metadata` diff of both revisions, workspace-wide and unmapped
  paths run everything, and only a reviewed non-Rust allowlist selects no
  tests. `affected ci` writes the same selection for a pull request's CI jobs,
  and `affected guard` checks that every repository path a test reads selects
  that test.
- `crates/xtask/src/package_affected.rs` — `cargo xtask package affected`: maps
  changed files to the official runtime packages a change selects; a core
  change, an embedded file, or an unknown path selects every package. The
  per-PR package jobs also run every package without a verified release
  baseline (`package_baseline.rs`).
- `crates/xtask/src/package_bump.rs` — `cargo xtask package bump`: the version
  gate over the baseline verdicts; fails a changed package whose version is
  not above the released versions, and reports `undecided` for an absent
  catalog.
- `crates/knowledge/src/log.rs` — knowledge-log fragments, the files of the
  log.d directory next to `docs/knowledge/log.md`, which the release fold
  removes with its last fragment: the fragment rules, the entry order, the assembled
  log the embedded bundle (`crates/knowledge/build.rs`) and `docs build` ship,
  and the release fold.
- `crates/xtask/src/knowledge_log.rs` — `cargo xtask docs log fold|migrate` and
  the `knowledge-log` docs check.
- `scripts/test-partitions` — disjoint unit/daemon/relay/cli/relay-db/heavy CI
  shards and exhaustive inventory check, and the macOS lane selections
  (`check-macos`, `run-macos`, `run-macos-tier`) that the `platform contracts
  (arm64)` and `macOS tier packages (arm64)` jobs run from one archive. Heavy is the exact complement of the
  fast filter; relay heavy tests (`relay-db`, the only PostgreSQL fixture
  consumers) run in a CI job gated by a paths filter on relay-relevant files,
  so non-relay changes never start the Postgres service. The shards run from
  one nextest archive built once by the `build-tests` CI job
  (`--archive-file`, extracted at any path) and
  retain per-shard JUnit timing evidence. Cold compilation
  is separate from the approximately two-minute fast-feedback target.
- `scripts/tests/test_partitions.py` — regression checks for coverage validation.
- `scripts/upgrade-test` — the upgrade test: downloads the previous release's
  Linux daemon archive (checksum verified), requires its
  `runtime/packages/claude.tar.zst` and an unexpired signed catalog (v0.34.2 or
  later), builds the target binaries, and runs
  `crates/cli/tests/release_upgrade.rs` as a throwaway local account with its
  own systemd user manager. The test installs the Claude package with the
  previous release, starts its session from the package, and checks the
  package pin survives the upgrade and `resume`. `.github/workflows/upgrade.yml` runs it from
  CI (pull requests touching the daemon, worker, protocol, or CLI crates),
  from `release.yml` before any publishable binary is built, and from
  `gh workflow run upgrade.yml -f previous_tag=<tag> -f head_ref=<ref>` with a
  branch, tag or commit of this repository, never `refs/pull/*` or a fork commit.
- `bacon.toml` — optional watcher jobs for the documented fast loops
  (`check`, `nextest-fast` on the `fast` profile, `affected`, `clippy-fast`)
  with explicit `e`/`n`/`a`/`c` job-switch keys; no gate depends on it.
- `rust-toolchain.toml` — the exact stable Rust that CI and local rustup
  proxies use (`channel`, clippy and rustfmt); the workspace `rust-version`
  stays the declared MSRV floor. `.github/actions/setup-rust` installs it (or a
  job's dated nightly) in every workflow job.
- `scripts/check-rust-toolchain-pin` — CI gate rejecting a floating channel
  (`stable`, `beta`, undated `nightly`) or a direct `dtolnay/rust-toolchain`
  use outside `.github/actions/setup-rust` in workflows and actions, and a
  non-exact pin; `scripts/tests/test_rust_toolchain_pin.py` runs it on fixture
  trees and the real tree.
- `scripts/bump-rust-toolchain` — moves the stable pin (`--stable X.Y.Z`)
  and the dated workflow nightly (`--nightly YYYY-MM-DD`) in one step; the
  bump PR also fixes new lints. `scripts/tests/test_bump_rust_toolchain.py`
  covers it.
- `scripts/dev-bootstrap` — local tool check: required `rustc` (the workspace `rust-version`), `cargo-nextest` (minimum
  read from `.config/nextest.toml`), `python3` >= 3.11 and the Hermes test
  interpreter (`POHUNEK_PYTHON_BIN` or the `test_python.rs` candidates: Python
  >= 3.10 with `yaml` passing the runtime file policy; the printed uv fix
  mirrors the `hermes-test-python` composite action and reads its pins, and
  unreadable pins fail only that row with `config-error`), optional `bacon`,
  `hyperfine`, and on Linux `mold` (fail only with `--strict`); a `rustc-pin`
  row warns when the active `rustc` differs from `rust-toolchain.toml` (fails
  only with `--strict`); a rustup proxy whose pinned toolchain is not
  installed is reported as `toolchain-not-installed` on the `rustc` and
  `cargo-nextest` rows with `rustup toolchain install` as the fix (fails the
  run); version commands run in the repository root with
  `RUSTUP_AUTO_INSTALL=0`; prints the exact fix for
  each missing or too-old tool (a shadowed or off-PATH copy gets the
  shell-quoted `export PATH` fix) and never installs anything itself. Those
  rows are the default `rust` contribution path; `--path` (repeatable) adds
  `docs` (`rustc` and its pin row for `cargo xtask docs check`), `sdk` (bun at the `.bun-version`/`packageManager` pin), `scripts`
  (`python3`, `shellcheck`, Linux `dash`), `macos-lint` (the
  `aarch64-apple-darwin` target and `zig`), `relay-db`
  (`POHUNEK_RELAY_TEST_DATABASE_URL` as set/unset only, never passed to child
  commands) and `native` (the systemd user manager or the launchd `gui/<uid>`
  domain; the other platform's suites are `ci-only`). Each path also lists,
  never runs, its `focused` commands (e.g. `cargo xtask affected`, the Bun
  gates, `scripts/darwin-check`), a `final` pointer to the gate set in
  `.agents/skills/gates/SKILL.md` and `.agents/instructions/verification.md`,
  and its `ci-only` CI jobs; the report prints them only with an explicit
  `--path`. `--json` prints one versioned document (`pohunek.dev-bootstrap`,
  version 1; keys and new status, category, check-kind or error-code values
  are only added within a version, and a consumer treats an unknown value as
  not ok) with stable status and error codes and a `checks` array (`[]` in an error document); exit 2 on an unknown
  path, a configuration error, or (under `--json`) an unexpected internal
  error.
- `scripts/tests/test_dev_bootstrap.py` — integration scenarios that run the
  real script as a child process against a disposable repository copy with
  fake tools on `PATH`: version parsing, the config-derived nextest minimum,
  required vs. optional and `--strict` exit status, MSRV and missing-`cargo`
  handling, the `rust-toolchain.toml` pin row and malformed pins, shadowed
  copies, the Hermes test interpreter check (explicit, policy, version,
  `yaml`, action-derived fix, a pin problem failing only its row, the real CI files), the byte-exact default report, every contribution path, each
  path's focused checks and final-gate pointer (listed only with an explicit
  `--path`), the `--json` contract and its error codes, and that the relay
  database URL never reaches any output. The `platform contracts (arm64)` CI
  job runs it on macOS and asserts the real host's `launchd-gui-domain` row.
- `scripts/ci-timings` — reproduction of CI timing evidence from `gh` run
  data: per-job wall clock and workflow medians (`runs`, `compare`), per-shard
  nextest JUnit summaries (`junit`), sccache/rust-cache
  cache-hit extraction (`cache`), per-job step medians (`steps`), and
  per-tier CI timing (`tiers`): pull request, push to main, `tier=main`
  dispatch, schedule and `tier=nightly` dispatch, each with its completion
  share (only runs with a verdict count as complete), job counts per
  complete run, and job queue split by runner platform from the REST jobs'
  runner labels. Windows take dates or UTC timestamps; `gh` server errors and
  secondary rate limits are retried, and a fetch that fails part way keeps
  what it fetched. Every counted job's wall clock is also rounded up to a whole minute and
  summed into runner minutes, carried by `runs`, `steps`, `compare`, and
  every `--json` summary. A fetch accumulates run documents into
  `target/ci-timings/ci-runs.json` (`--cache` writes elsewhere), but a
  measurement covers only the runs its own query named, so a fuller snapshot
  never changes a median; `--input` reads a snapshot instead and makes no
  network calls at all, which is how the report is regenerated offline.
  `--limit` applies per window and a filled window is flagged as possibly
  truncated; run logs are cached per run *and* attempt. The before/after report lives in
  `docs/design/test-performance-report.md`.
- `scripts/tests/test_ci_timings.py` — regression checks for the timing
  helper's parsing, medians, and markdown renderers.
- `scripts/junit-flaky-summary` — lists flaky (failed, then passed on a
  retry) and persistently failed tests from nextest JUnit reports as a
  Markdown table in `$GITHUB_STEP_SUMMARY` (or stdout with `--stdout`); a
  missing or unreadable report is written into the summary. CI runs it with
  `if: always()` after the fast, heavy, and relay-db nextest steps.
- `scripts/test-duplicates` — reads one CI run's downloaded JUnit artifacts and
  fails on a test that two jobs of the same OS ran, unless
  `scripts/test-duplicates.json` allows it with a reason; that file also names
  each artifact's OS, and an unnamed artifact fails the check. The `test
  duplicates (same OS)` job runs it in the main and nightly tiers (#1226).
- `scripts/tests/test_test_duplicates.py` — scenarios for that check.
- `scripts/tests/test_junit_flaky_summary.py` — regression checks for that
  summary against real nextest JUnit captures in
  `scripts/tests/fixtures/nextest-junit/`.
- `scripts/measure-dev-loop` — the local dev-loop baseline measurement
  (issue #163): the `cold`, `warm`, and `incremental` time cases timed
  with hyperfine or `time.monotonic` in a dedicated, marker-guarded
  target dir, plus the `debug` tree's hardlink-deduplicated size. Every
  measured subprocess runs with `CARGO_TARGET_DIR` pinned to that dir,
  `RUSTC_WRAPPER`/`CARGO_BUILD_RUSTC_WRAPPER` disabled, and all
  `POHUNEK_*` variables removed; repository/machine metadata lands in
  every `baseline.json`. Subcommands: `run` (`--dry-run` prints
  commands without executing), `report`, and `compare`.
- `scripts/tests/test_measure_dev_loop.py` — regression checks for the
  local baseline helper's marker-file deletion safety, hardlink size
  dedup, environment stripping, hyperfine JSON parsing, dry-run plans,
  and the report/compare renderers; no test starts a real build.
- `scripts/cargo-sweep-targets` — retention cleanup for per-branch isolated
  Cargo target dirs (`target/<name>`): canonicalizes and sentinel-verifies the
  target root, deletes only entries with Cargo target shape whose newest nested
  mtime exceeds the retention window, and keeps `debug`/`release`/`nextest`/
  `tmp`/`pohunek-docs`/`pohunek-eval`/`doc`/`package` plus cross-compile
  triples (`rustc --print target-list`).
- `scripts/tests/test_cargo_sweep_targets.py` — regression checks for that
  helper's destructive guards; the CI script-regression step runs
  `python3 -m unittest discover -s scripts/tests -p 'test_*.py'`.
- `scripts/worktree-new` — creates `pohunek-worktrees/<slug>` beside the
  primary checkout (whichever checkout it runs from) on
  `zajca/<slug>` and seeds the worktree's own `target/debug` caches
  (`.fingerprint`, `build`, `deps`, `incremental`, plus `CACHEDIR.TAG`) from
  the main checkout with `cp -a --reflink=always`; one run at a time holds
  an exclusive lock on `<git-common-dir>/worktree-new.lock` from the
  existence checks through any rollback, a real probe clone into a private
  `mkdtemp` directory must succeed first, the main checkout's three Cargo
  lock files are created if absent and held exclusively while the source
  is validated and copied, a symlink anywhere in the profile dir's seeded
  trees or as a target-root file fails closed, both checkouts must use the
  default target layout per `cargo metadata`, and uplifted binaries are not
  seeded. A temporary branch `zajca/worktree-new-tmp-<slug>-<pid>-<random>`
  is created with `git branch --no-track` at the resolved base commit and
  the worktree is added and seeded at the matching temporary sibling path,
  then `git worktree move`d to its slug, and the branch is renamed to its
  final name last with `git branch -m`; a failure before that rename
  removes only the worktree at the temporary path, only while git lists it
  on the temporary branch, and never with `--force` (so modified or
  untracked files survive), and deletes only the temporary branch by `git update-ref -d` against the commit it was created
  at, and only once no registered worktree still has it checked out, so
  plain-git work under the final names is never touched. Worktrees are
  read from `git worktree list --porcelain -z`, so any path parses intact. A failed
  final rename keeps the worktree on its temporary branch and reports both.
  `--no-seed` skips seeding. A stale seed is skipped too: when the base's
  `Cargo.lock` blob differs from the main checkout's own, or the last
  `Cargo.lock` change landed on the base's mainline after the seed profile's
  newest `.fingerprint`/`deps` mtime, the run seeds nothing, takes no Cargo
  locks or probe, and prints a line starting `not seeded (stale seed`;
  `--force-seed` seeds regardless and cannot be combined with `--no-seed`,
  and a staleness signal that cannot be read keeps the seed.
- `scripts/tests/test_worktree_new.py` — regression checks for its slug and
  argument validation, fail-closed reflink probe, Cargo and repository lock
  contention (including two concurrent runs for one slug), layout checks,
  symlinked seed sources (including nested ones), the private probe
  directory, rollback of partial `git worktree add` failures, races with
  plain `git` creating the same worktree or branch, moving the temporary
  branch, recreating the final branch during rollback, or taking the final
  name before the rename, a failed worktree removal keeping its branch,
  NUL-separated worktree listings with newline paths, the temporary-path
  lifecycle, `--no-seed`, and the stale-seed skip with `--force-seed`, with an
  injected executor.
- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`
- `sdk/ts/scripts/pack-release.ts` — deterministic per-package npm-pack tarballs
  of the TypeScript SDK (`pohunek-ts-{protocol,sdk,testkit}-X.Y.Z.tgz` plus
  `.sha256`) whose inner `@pohunek/*` dependencies are rewritten to the
  release-asset URLs.
- `sdk/ts/scripts/test/pack-contract.test.ts` — real tarballs, a local HTTP
  server and a real `bun install` of a URL-pinned consumer.
- `README.md`
- `docs/install.md` — manual install: release archives, login service, upgrades, quick start, and installing the official runtime packages from a release daemon archive with its signed catalog.
- `docs/development.md` — workspace layout, gates, fast loops, release pipeline.
- `packaging/install-daemon.sh` — release-archive wrapper that re-executes
  itself under `pohunek service lock` and retires a legacy template-unit
  install in a fail-closed order (`pohunek service check` of every check the
  final install or upgrade makes, start of a stopped legacy daemon for the
  snapshot, socket rename barrier,
  `migration preflight --socket <moved>`, worker inventory, stop, post-stop
  re-inventory), then runs `pohunek service install|upgrade`:
  `install` while `pohunek service status --json` reports a pending install
  transaction or no `service.toml` exists, `upgrade` otherwise. Before any of
  that, and before running any archive binary, it supports only Linux x86_64
  and native macOS arm64 hosts and verifies the archive `MANIFEST`: daemon
  component, target matching the host, macOS minimum version, every member
  present, a regular file, unmodified, and not writable by another account.
- `docs/knowledge/runbooks/install-on-macos.md` — macOS install (Homebrew and archive),
  provenance verification, upgrade, rollback, uninstall, logs, logout/reboot,
  and Gatekeeper and Keychain troubleshooting runbook.
- `packaging/write-manifest` — writes an archive's `MANIFEST` (component,
  version, target, signing state, minimum macOS, SHA-256 of every member) as the
  last assembly step; refuses symbolic links and member names outside a plain
  alphabet.
- `packaging/stage-archive` — assembles one component's archive staging
  directory (binaries, completions, offline docs, installer or
  smoke script, README, license, and the `docs/` reference pages) from the
  built binaries; it refuses to stage when one of those repository files is
  missing.
- `packaging/archive` — packs a staging directory into a byte-reproducible
  `.tar.gz` (sorted members, root ownership, `SOURCE_DATE_EPOCH` timestamps,
  normalized modes, `gzip -n`) with its `.sha256`.
- `scripts/tests/test_packaging.py` — regression checks for those three.
- `packaging/macos/DEPLOYMENT_TARGET` — the macOS deployment target (`14.0`)
  every native macOS build and audit uses.
- `packaging/macos/build-release` — locked, deployment-target-pinned,
  path-remapped native Apple Silicon release build of one component.
- `packaging/macos/audit-macho` — fails a release tree unless every Mach-O file
  is thin arm64, within the deployment target, linked only against system
  libraries, with no runtime search path and no build-machine path.
- `packaging/macos/package` — stages, audits, seals, and archives built macOS
  binaries; `--adhoc-release` produces the ad-hoc signed release archive
  (`signing adhoc`), and `--development` produces the unsigned
  `-unsigned-development` archive that is never released.
- `scripts/acceptance/macos-package-install` — CI acceptance of installing,
  upgrading with live sessions, refusing bad archives, and uninstalling from
  the extracted macOS archives against real launchd.
- `packaging/verify-archive` — shared installer preflight: supported host,
  macOS minimum, MANIFEST component/target, and every member present, intact,
  and not writable by another account; run by `packaging/install-daemon.sh`
  before it runs or changes anything.
- `scripts/tests/test_macos_packaging.py` — regression checks for the audit
  (with `otool`/`lipo`/`strings` shims) and the packaging scripts.
- `packaging/macos/sign` — ad-hoc signs every Mach-O file
  (`codesign --force --sign -`) with the identifier
  `io.github.zajca.pohunek.<name>`.
- `packaging/macos/verify-signed` — verifies the signatures; `--adhoc` requires
  `Signature=adhoc` and `codesign --verify --strict`, and rejects Developer ID,
  unsigned, or broken signatures.
- `scripts/tests/test_macos_signing.py` — regression checks for the ad-hoc
  signing tooling and the release workflow's macOS and attestation jobs, against
  shims.
- `crates/cli/tests/daemon_packaging.rs`
- `crates/cli/tests/service_lock.rs` — `pohunek service lock` and the lock it
  hands down, through the real binary.
- `scripts/acceptance/macos-launchd-lifetime` — manual macOS logout/reboot
  lifetime procedure; `scripts/acceptance/launchd_lifetime_evidence.py`
  evaluates its observations (tested by
  `scripts/tests/test_launchd_lifetime_evidence.py`).
- `docs/acceptance/README.md` — manual acceptance evidence and its schema.
- `scripts/release` — release versioning, the knowledge-log fold and the
  changelog fold into the release commit, exact-commit main/nightly CI evidence
  gate, and local-only `--no-push` behavior.
- `scripts/changelog` — validates the `changelog.d/` fragments and folds them
  into the committed `CHANGELOG.md`; `notes` prints the release-body block.
- `scripts/ci-evidence` — verifies successful Actions jobs and steps for an
  exact commit, with optional dispatch and bounded polling.
- `scripts/provision-hermes-compat`
- `scripts/tests/provision-hermes-compat.sh`
- `scripts/smoke-hermes-plugin-release`
- `scripts/build-hermes-package`
- `crates/xtask/src/hermes/package_host.rs`
- `docs/runbooks/hermes-operator-plugin.md`
- `docs/migrations/hermes-operator-plugin.md`

Daemon, sessions, integrations, and project state:

- `docs/design/macos-support-rfc.md`
- `crates/platform/src/lib.rs`
- `crates/platform/src/process/`
- `crates/platform/src/peer/`
- `crates/platform/src/supervisor/mod.rs` — `Supervisor`, `DaemonSupervisor`,
  `PluginUnits`, `JobDefinition`, `PluginUnitDefinition`, and the typed
  supervision errors.
- `crates/platform/src/supervisor/namespace.rs` — installation namespace,
  launchd labels, systemd unit names, and workflow plugin unit names.
- `crates/platform/src/supervisor/launchd/` — launchd backend (`/bin/launchctl`
  status table, plist rendering, worker and daemon agents, and workflow plugin
  unit LaunchAgents).
- `crates/platform/src/supervisor/systemd.rs` — systemd transient-unit backend,
  the daemon unit/slice installer, and the workflow plugin unit installer.
- `crates/platform/src/shell_env/` — executable search-path policy: bounded
  login-shell discovery, path validation, and executable resolution (see
  `docs/knowledge/guides/environment-resolution.md`).
- `docs/knowledge/guides/environment-resolution.md` — how the daemon and
  agents resolve `PATH` and executables.
- `crates/platform/src/process/sweep.rs` — ownership-marker sweep of a lost
  runtime generation, including the fork-lineage decision for processes whose
  environment the kernel withholds (the worker's journaled `worker_spawn_id`;
  a launchd creator number proves nothing), the coalition exclusion (the
  worker's journaled `worker_coalitions`; only a difference between both
  coalitions is evidence, and only of non-membership), the Linux cgroup
  exclusion (the worker's journaled `worker_control_group`; a process outside the
  worker's `user@<uid>.service` subtree is foreign, one inside proves nothing), the reboot proof (the worker's journaled `boot_session_id`, a canonical UUID; a host that names another boot holds no process of the runtime, so the sweep is complete without scanning), and the never-signalled
  skip of a worker child (`POHUNEK_WORKER_ID`) of the session without a
  worker-instance marker.
- `crates/service-config/src/lib.rs` — `service.toml` schema, trust, and
  validation.
- `crates/service-config/src/preflight.rs` — the upgrade preflight report
  (`PreflightReport`, verdicts, store state) and its reason-code constants,
  shared by `pohunek service` and `pohunekd upgrade-preflight`.
- `crates/worker-protocol/src/env.rs` — `BaseEnv`, the default environment
  allowlist, and the service-manager denylist.
- `crates/worker-protocol/src/hook_schema.rs` — the closed registry of hook
  schemas that worker and daemon validation read.
- `crates/paths/fixtures/runtime-paths.json`
- `crates/daemon/src/host_state/`
- `crates/daemon/src/store/mod.rs`
- `crates/daemon/src/store/schema.rs` — `STORE_SCHEMA_VERSION`, the `MIGRATIONS`
  table, the `.pre-schema-<old>` backup, and `StoreSchemaError`.
- `crates/daemon/src/store/dry_run.rs` — read-only in-memory run of the startup
  migration used by the upgrade preflight.
- `crates/daemon/src/store/shape_guard.rs` — persisted-shape guard test (keys and
  enum values) and its `fixtures/shape/schema-<N>.txt` snapshots.
- `crates/daemon/src/host_state/records/shape_guard.rs` — governance and identity
  record guard and its `host_state/fixtures/*-<N>.txt` snapshots.
- `crates/package/src/registry/shape_guard.rs` — `registry.json` guard (keys and
  `PackageSource` values) and its `registry/fixtures/registry-<N>.txt` snapshots,
  tied to `REGISTRY_SCHEMA`.
- `crates/shape-guard/src/lib.rs` — dev-only helpers shared by the guards:
  key and string-leaf collection, enum registry classification, snapshot
  comparison, and the `unit_values!`/`data_values!` macros.
- `crates/protocol/src/version.rs`
- `crates/worker-protocol/src/version.rs`
- `scripts/release` — compatibility-constant check against the previous tag.
- `crates/daemon/src/store/legacy_binding.rs`
- `crates/session-worker/src/journal.rs`
- `crates/cli/src/hermes_integration/lifecycle.rs`
- `sdk/ts/sdk/test/runtime-path-contract.test.ts`
- `.github/workflows/ci.yml`

- `crates/daemon/src/assistant.rs`
- `crates/daemon/src/main.rs`
- `crates/daemon/src/lib.rs`
- `crates/daemon/src/api/handler/mod.rs`
- `crates/daemon/src/api/handler/session.rs`
- `crates/daemon/src/session/mod.rs`
- `crates/daemon/src/session/observation.rs`
- `crates/daemon/src/session/idle_stop.rs` — idle stop of sessions opted in with
  `pohunek.idle_stop_after_secs`: the host default record, the deadline task and
  the guarded stop.
- `crates/daemon/src/session/turn_wait.rs` — `session.wait_turn`: edge-triggered
  selection, `targets_ended`, `cursor_reset`, per-target progress.
- `crates/daemon/src/session/turn_events.rs` — the in-memory turn-event journal,
  cursor placement, `exited` edge detection, output and turn-start times, and
  the turn waiter pool.
- `crates/daemon/src/session/diff.rs`
- `crates/daemon/src/session/hooks.rs`
- `crates/daemon/src/session/detector.rs`
- `crates/daemon/src/session/reconcile.rs`
- `crates/daemon/src/session/reconcile/upgrade_preflight.rs` — per-session
  adoption verdicts from the store, journals and process table, without a
  worker socket.
- `crates/daemon/src/session/native_repair.rs`
- `crates/daemon/src/session/target.rs`
- `crates/daemon/src/session/resume.rs` — resume and fork, the frozen profile revision check and the `ProfileChange` decision.
- `crates/daemon/src/session/procwatch.rs`
- `crates/daemon/src/runtime/`
- `crates/daemon/src/runtime/lifecycle.rs` — worker generation lifecycle
  engine: commit points, per-session locks, worker socket path refusal, an
  ended job ending the connect wait, post-timeout reconciliation.
- `crates/daemon/src/notify.rs`
- `crates/daemon/src/external/mod.rs`
- `crates/daemon/src/external/watch.rs`
- `crates/daemon/src/external/launch_roots_tests.rs` — observer roots derived from the host's config homes.
- `crates/daemon/src/external/inotify.rs`
- `crates/daemon/src/external/fsevents.rs`
- `crates/daemon/src/external/unsupported.rs`
- `crates/daemon/tests/procwatch.rs`
- `crates/daemon/src/procwatch/mod.rs`
- `crates/daemon/src/project/mod.rs`
- `crates/daemon/src/project/config.rs`
- `crates/daemon/src/project/detect.rs`
- `crates/daemon/src/worktree/mod.rs`
- `crates/daemon/src/store/mod.rs`
- `crates/daemon/src/capabilities.rs`
- `crates/daemon/src/integration/mod.rs`
- `crates/daemon/src/integration/commit.rs`
- `crates/daemon/src/integration/doctor.rs`
- `crates/daemon/src/integration/handler.rs`
- `crates/daemon/src/integration/homes.rs`
- `crates/daemon/src/integration/provider_handlers.rs`
- `crates/daemon/src/integration/reporter.rs`
- `crates/daemon/src/integration/uninstall.rs`
- `crates/daemon/src/integration/assets/codex/pohunek-agent-state.sh`
- `crates/daemon/src/integration/assets/codex/pohunek-agent-notify.sh`
- `crates/daemon/src/integration/assets/claude/pohunek-agent-state.sh`
- `crates/daemon/src/integration/assets/claude/pohunek-agent-notify.sh`
- `crates/daemon/src/notifications/mod.rs`
- `crates/daemon/src/notifications/store.rs`
- `crates/daemon/src/notifications/coordinator.rs`
- `crates/daemon/src/notifications/policy.rs`
- `crates/daemon/src/notifications/projector.rs`
- `crates/daemon/src/paths.rs`
- `crates/daemon/src/logging.rs`
- `crates/logging/src/config.rs`
- `crates/logging/src/lib.rs`

Agent runtime and profile resolution:

- `crates/session-worker/`
- `crates/worker-protocol/`
- `crates/daemon/src/agent/mod.rs`
- `crates/daemon/src/agent/launch_env.rs` — the redacting launch environment carrier.
- `crates/daemon/src/agent/native_launch.rs`
- `crates/daemon/src/agent/native_reference.rs`
- `crates/daemon/src/agent/profile.rs`
- `crates/daemon/src/agent/host/`
- `crates/xtask/tests/no_special_dispatch.rs`
- `crates/daemon/src/detect/mod.rs`
- `crates/daemon/src/detect/osc.rs`
- `crates/daemon/src/detect/manifest/mod.rs`
- `crates/daemon/src/detect/manifests/shell.toml`
- `crates/terminal/src/screen.rs`

Runtime package archive format:

- `docs/knowledge/concepts/runtime-package-archive.md`
- `crates/package/src/lib.rs`
- `crates/package/src/archive.rs` — strict USTAR writer and reader.
- `crates/package/src/canonical.rs` — canonical header encoding.
- `crates/package/src/compression.rs` — single-frame zstd encode and bounded decode.
- `crates/package/src/limits.rs` — named size limits.
- `crates/package/src/error.rs` — typed rejections that never echo archive content.
- `crates/package/src/install.rs` — descriptor-relative extraction into a staging directory and atomic publication.
- `crates/package/src/verify.rs` — per-file re-verification of a package root and manifest-checked reads.
- `crates/package/src/manifest.rs` — per-file manifest schema and manifest digest.
- `crates/package/src/layout.rs` — package root names, modes and bounds.
- `crates/daemon/src/agent/host/package.rs` — package-backed runtime loading and pin resolution.
- `crates/daemon/src/agent/host/handle.rs` — runtime host snapshot, reload and launch verification.
- `crates/package/src/version_probe.rs` — the `semver-v1` and `semver-line-v1` data-driven version probe policy (probe argv, `[min, below)` release range, the `line` output template and the out-of-range or unparseable refusal), shared by the daemon launch gate and the CLI Hermes integration gate.
- `crates/daemon/src/session/packages.rs` — reload and retention-checked uninstall.
- `crates/package/src/hash.rs` — SHA-256 helpers.
- `crates/package/src/directory.rs` — directory-to-archive builder behind `plugin link` and `cargo xtask package build`.
- `crates/package/src/catalog_state.rs` — persisted catalog high-water mark and revoked key ids.
- `crates/daemon/src/session/package_lifecycle/` — the `package.*` operations: validate from the verified archive, claim rules, catalog trust, record, reload, and the compatibility-gated selection (`selection.rs`).
- `crates/daemon/src/session/package_lifecycle/trust.rs` — catalog verification against the host anchor and persisted high-water mark, and the official entry, core version and platform checks (`package_untrusted`, `package_incompatible`).
- `crates/daemon/src/session/package_lifecycle/fault.rs` — the mapping of root and descriptor failures to the `plugin doctor` fault kinds.
- `crates/daemon/src/agent/host/claim.rs` — which package may serve which runtime id.
- `crates/daemon/src/agent/host/config_home.rs` — the `[config_home]` descriptor declaration and the one resolver of a runtime's config directory.
- `crates/daemon/src/agent/host/transcripts.rs` — the `[transcripts]` descriptor declaration: transcript tree below the config home or a root of its own (`root_env`/`root_home`), the walk depth and the compiled parser id.
- `crates/daemon/src/external/muse.rs` — the `muse-session-v1` parser: dated session logs, framed or bare records.
- `crates/daemon/src/agent/native_reference/dir_name.rs` — the descriptor-only `dir_name` existence declaration of a hook runtime (not part of the persisted `ExistenceSpec`).
- `crates/daemon/src/agent/host/features.rs` — the append-only runtime feature table, the descriptor `requires` check and the feature-gated key rules.
- `crates/daemon/src/api/handler/package.rs` — `package.*` handlers and the local-only gate.
- `crates/daemon/src/agent/profile.rs` — profile `package`/`digest` pins and `pinned_digests`.
- `crates/protocol/src/package.rs` — `package.*` payloads and stable error codes.
- `crates/cli/src/commands/plugin.rs` — `pohunek plugin list|inspect|install|link|update|select|enable|disable|uninstall|doctor`.
- `crates/cli/src/commands/plugin_profile.rs` — `pohunek plugin profile list|migrate`.
- `docs/knowledge/guides/runtime-packages.md`
- `crates/cli/src/commands/workflow.rs` — `pohunek workflow list|inspect|install|enable|disable|uninstall|run` and the hidden `__exec`, the consent review, the `--json` payloads and the `run` exit statuses.
- `crates/cli/src/workflow/mod.rs` — workflow error codes, classes and recovery hints.
- `crates/cli/src/workflow/manifest.rs` — the strict `workflow.toml` schema, size cap and argv rules.
- `crates/cli/src/workflow/store.rs` — the workflow registry root, per-plugin state and config directories, install, uninstall and purge, and the per-plugin unit records of jobs and services.
- `crates/cli/src/workflow/runner.rs` — `workflow run`: refusals, the child's argv, cwd and exact environment, the context JSON, deadline, output caps, signal handling, outcome precedence, and the `--json` stdout capture and its single-document check.
- `crates/cli/src/workflow/config.rs` — the owner knobs `<config>/pohunek/workflow.toml`: tables, defaults, ranges and the file safety rule.
- `crates/cli/src/workflow/runlog.rs` — the per-plugin `logs/runs.jsonl` record and its outcomes.
- `crates/cli/src/workflow/status.rs` — `workflow status`: native state, last run, consecutive failures and service backoff of each recorded unit.
- `crates/cli/src/workflow/units.rs` — job and service units: the service installation they need, their definitions, and enable, disable and uninstall against the user service manager.
- `crates/cli/src/workflow/supervised.rs` — the `__exec` trampoline every unit runs: refusals, the run lock, the wall-clock job schedule, the service backoff and stop handling.
- `crates/cli/src/workflow/dispatch.rs` — the event reaction dispatcher (`__dispatch`): the daemon `subscribe` stream with backoff reconnects, per-plugin bounded queues with `dropped` accounting, one worker per plugin and the stop handling.
- `crates/paths/src/lib.rs` — `WORKFLOW_PLUGINS_SUBDIR`, `WORKFLOW_PLUGIN_STATE_SUBDIR` and `WORKFLOW_PLUGIN_CONFIG_SUBDIR`.
- `crates/cli/tests/workflow_store.rs` — consent, digest, hostile-manifest, tampered-root, local-only and lifecycle scenarios through the real binary.
- `crates/cli/tests/workflow_run.rs` — `workflow run` scenarios through the real binary: exact environment and origin pair (with a real `pohunekd` session), refusals, deadline, output cap, interruption, slow output consumer, redaction, context and owner knobs.
- `crates/cli/tests/workflow_reactions.rs` — event reactions against the real systemd user manager and a real `pohunekd`: the context and origin-free environment, queue drops with delivery after overflow, an oversized event's omission, a wedged reaction that delays neither session commands nor a daemon restart, and the dispatcher unit following the reacting plugins.
- `crates/cli/tests/workflow_run_json.rs` — `workflow run --json` and verbatim-argument scenarios through the real binary: byte-exact `ok`, invalid, failed, cut and capped output as single error envelopes, refusals as envelopes, and every argument after the action reaching it unchanged.
- `crates/cli/tests/support/workflow_scenarios.rs` — job and service scenarios against the real service manager (`workflow_systemd.rs`, `workflow_launchd.rs`): enable and disable, overlap, timeout, service backoff, failure isolation, stale units, the supervised environment and clean stops.
- `docs/knowledge/guides/workflow-plugins.md`
- `docs/knowledge/guides/plugin-repositories.md` — the rule that new plugins live in their own repositories, naming, required CI, index admission and the release snapshot; the guard is `check_plugin_directories` in `crates/xtask/src/doc_drift.rs`.
- `docs/knowledge/runbooks/runtime-package-recovery.md` — recovery from failed package verification, refused official installs, and disabled, removed or damaged pinned packages.
- `docs/architecture.md` — "Runtime Packages and Trust": the package classes and the same-UID owner trust boundary.
- `docs/knowledge/guides/pi-package.md`
- `docs/knowledge/guides/codex-package.md`
- `docs/knowledge/guides/claude-package.md`
- `docs/knowledge/guides/hermes-package.md`
- `runtime-packages/README.md` — layout convention of the official package sources.
- `runtime-packages/pi/runtime.toml` — Pi runtime descriptor (assigned reference for launches and forks, existence check with the session-dir variable and the refused flag and settings, version probe).
- `runtime-packages/pi/detect.toml` — Pi detection manifest verified against real screens.
- `compat/pi/compatibility-lock.json` — pinned Pi release, supported range and Node requirement.
- `compat/pi/screens/` — screens captured from a real Pi that the manifest test classifies; `overlays/` holds the dialog, login and selector screens.
- `scripts/pi-detect-regex` — generates the Pi frame, dialog and overlay regexes and checks their line classes.
- `crates/cli/tests/pi_package.rs` — pure package tests and the opt-in real-Pi tests.
- `runtime-packages/codex/runtime.toml` — Codex runtime descriptor (hook native reference, `semver-line-v1` version probe).
- `runtime-packages/codex/detect.toml` — Codex detection manifest, equal to the v0.34.2 built-in one; the daemon compiles no Codex in, so this package is the only source of `codex`.
- `compat/codex/compatibility-lock.json` — pinned Codex release and supported range.
- `compat/codex/screens/` — screens and terminal titles captured from a real Codex that the manifest test classifies.
- `compat/codex/builtin-v0.34/` — golden copies of the built-in Codex descriptor and detection manifest of release 0.34.2.
- `crates/cli/tests/codex_package.rs` — pure package tests (descriptor and manifest parity with the v0.34.2 built-in goldens), catalog-installed package launch and version-probe tests, the clean-host scenarios (bare `codex`, its integration and an unpinned `base = "codex"` profile refused with the install hint until the package is installed), and the opt-in real-Codex tests.
- `crates/cli/tests/support/responses_stub.rs` — loopback Responses-API stub used instead of a model provider.
- `runtime-packages/claude/runtime.toml` — Claude Code runtime descriptor (hook native reference, resume and fork argv, `semver-line-v1` version probe).
- `runtime-packages/claude/detect.toml` — Claude Code detection manifest: the v0.34.2 built-in one plus the #613 rules (running-turn title and footer, wrapped selection forms, first-run and trust dialogs, the `done <time>` completed-turn line, the npm `claude.exe` process); the daemon compiles no Claude in, so this package is the only source of `claude`.
- `compat/claude/compatibility-lock.json` — pinned Claude Code release, native binary SHA-256 and supported range.
- `compat/claude/screens/` — screens and terminal titles captured from a real Claude Code that the manifest test classifies.
- `compat/claude/builtin-v0.34/` — golden copies of the built-in Claude descriptor and detection manifest of release 0.34.2.
- `crates/cli/tests/claude_package.rs` — pure package tests (descriptor and manifest parity with the v0.34.2 built-in goldens up to the enumerated #613 rule changes), catalog-installed package launch and version-probe tests, the clean-host scenarios (bare `claude`, its integration and an unpinned `base = "claude"` profile refused with the install hint until the package is installed; Claude hooks removable without the package), the reporter-script golden check, the opt-in real-Claude tests and the screen capture.
- `crates/cli/tests/support/messages_stub.rs` — loopback Messages-API stub used instead of a model provider.
- `runtime-packages/hermes/runtime.toml` — Hermes runtime descriptor (the v0.34.2 built-in Hermes descriptor with a `semver-line-v1` version probe and the package's own detection manifest).
- `runtime-packages/hermes/detect.toml` — Hermes detection manifest, equal to the v0.34.2 built-in one; the daemon compiles no Hermes in, so this package is the only source of `hermes`.
- `compat/hermes/builtin-v0.34/` — golden copies of the built-in Hermes descriptor and detection manifest of release 0.34.2.
- `runtime-packages/hermes/operator.toml` — closed sidecar of the Hermes operator plugin payload in `runtime-packages/hermes/operator/`.
- `crates/cli/tests/hermes_package.rs` — catalog-installed Hermes package tests (descriptor and manifest parity with the v0.34.2 built-in goldens, the version probe, the clean-host scenario (bare `hermes` refused with the install hint and the assistant without a capable agent until the package is installed), operator installs from the selected package, the package-backed operator lifecycle, the source errors including a missing package, and installs with an `embedded` marker) and the opt-in real-Hermes session test.
- `crates/cli/tests/support/process_guard.rs` — process cleanup shared by the real-agent package tests.
- `crates/cli/tests/support/plugin_harness.rs` — real-daemon harness shared by the package tests.
- `crates/test-support/src/catalog.rs` — throwaway signing key, trust anchor and signed catalog helpers (feature `catalog`) shared by the catalog install tests of the CLI and the daemon.
- `crates/daemon/tests/support/official_runtimes.rs` — installs the official Codex and Claude packages through a throwaway catalog for the daemon integration tests, and the `--version` answer their fake agents give.
- `crates/cli/tests/support/model_stub.rs` — loopback chat-completions stub used instead of a model provider.
- `crates/package/src/registry.rs` — owner-private registry record, transactions, retention and uninstall.
- `crates/package/tests/archive.rs`
- `crates/package/tests/install.rs`
- `crates/package/tests/registry.rs`
- `crates/package/tests/catalog_state.rs`

Signed runtime catalog:

- `docs/knowledge/concepts/runtime-catalog.md`
- `crates/package/src/catalog.rs` — catalog schema, key chain, signature verification, authorization, local trust.
- `crates/package/src/canonical_json.rs` — strict JSON parsing and the canonical signed byte form.
- `crates/package/tests/catalog.rs`
- `crates/package/src/release.rs` — the catalog release block, binary-set digests and per-entry attestation rules.
- `crates/package/src/release.rs` — the catalog release block, binary-set digests and per-entry attestation rules.
- `crates/package/src/release.rs` — the catalog release block, binary-set digests and per-entry attestation rules.
- `crates/xtask/src/runtime_package.rs` — `cargo xtask package build|verify`.
- `crates/xtask/src/package_baseline.rs` — `cargo xtask package baseline`: which official packages the signed release `v<version>` proves unchanged, with stable reason codes for the missing ones; `evaluate` is shared with `package bump`.
- `crates/xtask/src/package_evidence.rs` — `package build --evidence-dir`: the archive checksum, manifest projection, provenance input and compatibility vector, refused when the archived bytes, the lock or the matrix differ from the blobs of `HEAD`.
- `crates/package/src/anchor.rs` — the catalog trust anchor file format and its strict parser.
- `crates/package/src/sign.rs` — catalog signing for release tooling.
- `crates/daemon/src/catalog_anchor.rs` — reading the anchor beside the daemon executable and its file-safety policy.
- `crates/daemon/src/doctor.rs` — the `catalog_trust_anchor` doctor check.
- `crates/xtask/src/catalog.rs` — `cargo xtask catalog build|sign|verify|anchor|public-key`.
- `crates/xtask/src/catalog_key.rs` — owner-private signing key file reader.
- `packaging/runtime-catalog-anchor.json` — the production trust anchor shipped beside `pohunekd`.
- `packaging/catalog-trust/roots.txt` — the production roots and windows (with the `primary.pub` and `ci.pub` public keys) the anchor is generated from.
- `crates/cli/tests/plugin_catalog_install.rs` — clean-environment official install through a real daemon with a test key.
- `crates/xtask/src/attestation.rs` — `cargo xtask compat attest|verify|matrix-check`, the binary-set digest and the compatibility attestation.
- `compat/matrix.json` — the required runtime x target rows and the suite version of the release compatibility matrix.
- `crates/xtask/src/release.rs` — `cargo xtask release assemble|verify-inventory`: input checks, catalog signing and bundle assembly.
- `crates/xtask/src/release_policy.rs` — the release policy file parser.
- `crates/xtask/src/release_tree.rs` — safe archive extraction and the rebuilt daemon archive trees.
- `crates/xtask/src/release_inventory.rs` — the release inventory and its verification.
- `packaging/release-policy.json` — the expected archives, SDK packages and catalog validity of a release.
- `packaging/smoke-archive` — the network-isolated archive smoke (with `scripts/tests/test_smoke_archive.py`).
- `.github/workflows/release-build.yml` — reusable producer workflow: archives, package archives and SDK tarballs as workflow artifacts.
- `.github/workflows/release-evidence.yml` — reusable workflow: compatibility rows, provenance, catalog signing, bundle assembly and the archive smoke.
- `.github/workflows/release-smoke-scenarios.yml` — reusable macOS synthetic-archive scenarios, started before the rehearsal build and required by the evidence verdict.
- `scripts/release-workflow/` — the helper scripts the release workflows call.
- `crates/xtask/src/upstream_stage.rs` — `cargo xtask compat stage-upstream|verify-stage`: npm or provisioner staging of the pinned upstream runtimes, selected by the lock's `upstream.kind`, and the stage verifier.
- `compat/pi/npm/package-lock.json` — the pinned npm dependency tree of the Pi upstream (with the codex and claude counterparts under `compat/`).
- `crates/cli/tests/release_consumer.rs` — out-of-process consumer suite on the exact release binaries (inputs, layout, report); the per-runtime drivers (pi, codex, claude, hermes) are in `crates/cli/tests/support/release_drivers.rs`.
- `crates/xtask/src/attestation.rs` — `cargo xtask compat attest|verify|matrix-check`, the binary-set digest and the compatibility attestation.
- `compat/matrix.json` — the required runtime x target rows and the suite version of the release compatibility matrix.

Hermes compatibility evidence:

- `compat/hermes/compatibility-lock.json`
- `compat/hermes/README.md`
- `compat/hermes/goldens/manifest.json`
- `crates/xtask/src/hermes.rs`
- `crates/xtask/src/hermes_mock.rs`
- `crates/xtask/src/eval.rs`
- `crates/xtask/src/hermes_skill.rs`
- `crates/cli/tests/support/hermes_provider.rs` — the loopback Hermes model provider and private Hermes home of the Hermes package tests and the release consumer's Hermes driver.

Bundled agent skill:

- `docs/knowledge/guides/agent-skill.md`
- `crates/xtask/src/agent_skill.rs`
- `crates/cli/src/commands/agent_skill/SKILL.md`

Hermes operator plugin and managed lifecycle:

- `crates/cli/src/hermes_integration/mod.rs`
- `crates/cli/src/hermes_integration/error.rs`
- `crates/cli/src/hermes_integration/target.rs`
- `crates/cli/src/hermes_integration/policy.rs`
- `crates/cli/src/hermes_integration/lifecycle.rs`
- `crates/cli/src/hermes_integration/runner.rs`
- `crates/cli/src/process/bounded.rs` — the bounded child lifecycle the Hermes runner and `workflow run` spawn through: own process group, inherited descriptors closed, deadline kill (with a `SIGTERM` grace for workflow runs), and capped output, buffered or streamed to a sink.
- `crates/cli/src/hermes_integration/assets.rs`
- `crates/cli/src/hermes_integration/doctor.rs`
- `crates/cli/src/hermes_integration/skill.rs`
- `runtime-packages/hermes/operator/plugin.yaml`
- `runtime-packages/hermes/operator/__init__.py`
- `runtime-packages/hermes/operator/cli.py`
- `runtime-packages/hermes/operator/hooks.py`
- `runtime-packages/hermes/operator/policy.py`
- `runtime-packages/hermes/operator/redact.py`
- `runtime-packages/hermes/operator/tools.py`
- `crates/cli/src/hermes_integration/assets/pohunek/_pohunek_reporter.py`
- `crates/cli/src/hermes_integration/assets/tests/test_plugin_runtime.py`
- `docs/knowledge/guides/hermes-operator.md`
- `docs/design/hermes-agent-integration.md`
- `docs/design/hermes-agent-integration-plan.md`
- `docs/public-api.md`

Protocol contracts and transport:

- `crates/client/src/discovery.rs`

- `crates/client/src/lib.rs`
- `crates/client/src/overlay_config.rs`
- `crates/client/src/notifications.rs`
- `crates/client/src/transport.rs`
- `crates/client/src/error.rs`
- `crates/client/tests/previous_daemon.rs`
- `sdk/ts/sdk/src/compat.ts`
- `sdk/ts/sdk/test/previous-daemon.test.ts`
- `crates/paths/src/lib.rs`
- `crates/protocol/src/assistant.rs`
- `crates/protocol/src/compat/mod.rs`
- `crates/protocol/src/compat/v3.rs`
- `crates/protocol/src/envelope.rs`
- `crates/protocol/src/decimal.rs`
- `crates/protocol/src/limits.rs`
- `crates/protocol/src/idle_stop.rs` — `session.idle_stop.get/set` types and the
  `pohunek.idle_stop_after_secs` metadata value.
- `crates/protocol/src/turn_wait.rs` — `session.wait_turn` params, result,
  cursor, and progress wire types.
- `crates/protocol/src/lib.rs`
- `crates/protocol/src/method.rs`
- `crates/protocol/src/notification.rs`
- `crates/protocol/src/runtime_id.rs`
- `crates/protocol/src/session.rs`
- `crates/protocol/src/project.rs`
- `crates/protocol/src/capabilities.rs`
- `crates/protocol/src/integration.rs`
- `crates/protocol/src/error.rs`
- `crates/protocol/src/version.rs`
- `crates/protocol/tests/compat_v3.rs`
- `crates/protocol/tests/roundtrip.rs`
- `crates/netbird/src/lib.rs`
- `crates/netbird/src/status.rs`
- `crates/netbird/src/transport.rs`
- `crates/overlay/src/lib.rs`
- `crates/overlay-tailscale/src/lib.rs`
- `crates/overlay-tailscale/src/status.rs`
- `crates/overlay-tailscale/src/transport.rs`
- `crates/overlay-tailscale/tests/provider.rs`
- `crates/overlay/tests/registry_contract.rs`

Assistant knowledge entry points:

- `crates/knowledge/src/lib.rs`
- `docs/knowledge/`

Workspace crates by role (every workspace crate has an entry in this map;
`cargo xtask docs check` fails when one is missing):

- `crates/session-worker/src/lib.rs` — `pohunek-sessiond`: one durable PTY runtime
  owner per live session (`pty.rs`, `journal.rs`, `lease.rs`, `server.rs`).
- `crates/worker-protocol/src/lib.rs` — the versioned owner-private
  daemon-to-worker protocol, framing, and environment contract.
- `crates/platform/src/lib.rs` — operating-system contracts shared by the daemon
  and the session worker: process facts, peer identity, the service supervisor.
- `crates/terminal/src/lib.rs` — VT screen tracking and attach compositing.
- `crates/service-config/src/lib.rs` — typed `service.toml` and the upgrade
  preflight reports.
- `crates/relay-protocol/src/lib.rs` — typed public API contract of the optional
  team relay.
- `crates/relay-client/src/lib.rs` — HTTPS-only native relay authentication
  client without a daemon dependency.
- `crates/shape-guard/src/lib.rs` — dev-only persisted-shape guard helpers.
- `crates/test-support/src/lib.rs` — test-only helpers: hermetic fixture roots
  and environments, binary resolution, bounded readiness waits.

Design inputs:

- `docs/design/delegated-task-runs-rfc.md`
- `docs/design/universal-assistant.md`
- `docs/design/universal-assistant-plan.md`
- `docs/design/durable-session-workers-rfc.md`
- `docs/migrations/durable-session-workers.md`
- `docs/runbooks/durable-session-workers.md`
- `docs/public-api.md`

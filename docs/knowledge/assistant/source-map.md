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
- `crates/cli/src/commands/host.rs`
- `crates/cli/src/commands/integration.rs`
- `crates/cli/src/commands/prompt.rs`
- `crates/cli/src/client.rs`
- `crates/cli/src/target.rs`
- `crates/cli/src/paths.rs`
- `crates/cli/src/error.rs`
- `crates/cli/tests/notifications_clap.rs`
- `crates/cli/tests/agent_skill.rs`
- `crates/cli/tests/prompt_link.rs`
- `crates/cli/tests/standalone_discovery.rs`
- `crates/cli/tests/session_process_api.rs`

Assistant launch and prompt rendering:

- `crates/assistant/src/lib.rs`
- `crates/assistant/src/host.rs`
- `crates/assistant/src/error.rs`
- `crates/assistant/src/launch.rs`
- `crates/assistant/tests/select_agent.rs`
- `crates/prompt/src/lib.rs`
- `crates/prompt/src/link.rs`
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

Implemented reduced relay foundation and deferred team-relay architecture:
PostgreSQL fencing and recovery, protected stopped-lifecycle provisioning,
generic OIDC browser/device authentication, bounded HTTPS account and credential
lifecycle, provider-neutral account linking, and native relay CLI are
implemented. Host links, routing, attach, team administration, provider
verification, and team clients remain deferred:

- `docs/design/team-relay-control-plane-rfc.md`
- `docs/architecture.md`
- `docs/ROADMAP.md`
- `docs/public-api.md`
- `docs/knowledge/concepts/team-relay.md`
- `docs/knowledge/safety/trust-model.md`
- `docs/knowledge/guides/remote-hosts.md`
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
  and the flaky-test policy (`flaky-result = "fail"`; retries on `heavy` and
  `relay-db` only).
- `.cargo/config.toml` — `cargo t`/`cargo ti` select fast unit and integration
  tests; `cargo tw` keeps the full suite; `cargo ta` runs `cargo xtask
  affected`. Fast loops do not replace full gates.
- `crates/xtask/src/affected.rs` — `cargo xtask affected`: maps changed,
  staged, unstaged, and untracked files to workspace packages and runs
  `cargo t` with an `rdeps` filterset; workspace-wide and unmapped paths run
  everything, and only a reviewed non-Rust allowlist selects no tests.
- `scripts/test-partitions` — disjoint unit/daemon/relay/cli/relay-db/heavy CI
  shards and exhaustive inventory check. Heavy is the exact complement of the
  fast filter; relay heavy tests (`relay-db`, the only PostgreSQL fixture
  consumers) run in a CI job gated by a paths filter on relay-relevant files,
  so non-relay changes never start the Postgres service. The shards run from
  one nextest archive built once by the `build-tests` CI job
  (`--archive-file`, extracted at any path) and
  retain per-shard JUnit timing evidence. Cold compilation
  is separate from the approximately two-minute fast-feedback target.
- `scripts/tests/test_partitions.py` — regression checks for coverage validation.
- `bacon.toml` — optional watcher jobs for the documented fast loops
  (`check`, `nextest-fast` on the `fast` profile, `affected`, `clippy-fast`)
  with explicit `e`/`n`/`a`/`c` job-switch keys; no gate depends on it.
- `scripts/dev-bootstrap` — local tool check: required `rustc` (the workspace `rust-version`), `cargo-nextest` (minimum
  read from `.config/nextest.toml`) and `python3` >= 3.11, optional `bacon`,
  `hyperfine`, and on Linux `mold` (fail only with `--strict`); prints the exact fix for
  each missing or too-old tool (a shadowed or off-PATH copy gets the
  shell-quoted `export PATH` fix) and never installs anything itself.
- `scripts/tests/test_dev_bootstrap.py` — regression checks for version
  parsing, the config-derived nextest minimum, required vs. optional and
  `--strict` exit status, MSRV and missing-`cargo` handling, and shadowed
  copies; `which` and version calls are injected.
- `scripts/ci-timings` — reproduction of CI timing evidence from `gh` run
  data: per-job wall clock and workflow medians (`runs`, `compare`), per-shard
  nextest JUnit summaries (`junit`), sccache/rust-cache
  cache-hit extraction (`cache`), and per-job step medians (`steps`).
  Every counted job's wall clock is also rounded up to a whole minute and
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
- `docs/install.md` — manual install: release archives, login service, upgrades, quick start.
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
- `scripts/release`
- `scripts/provision-hermes-compat`
- `scripts/tests/provision-hermes-compat.sh`
- `scripts/smoke-hermes-plugin-release`
- `docs/runbooks/hermes-operator-plugin.md`
- `docs/migrations/hermes-operator-plugin.md`

Daemon, sessions, integrations, and project state:

- `docs/design/macos-support-rfc.md`
- `crates/platform/src/lib.rs`
- `crates/platform/src/process/`
- `crates/platform/src/peer/`
- `crates/platform/src/supervisor/mod.rs` — `Supervisor`, `DaemonSupervisor`,
  `JobDefinition`, and the typed supervision errors.
- `crates/platform/src/supervisor/namespace.rs` — installation namespace,
  launchd labels, and systemd unit names.
- `crates/platform/src/supervisor/launchd/` — launchd backend (`/bin/launchctl`
  status table, plist rendering, worker and daemon agents).
- `crates/platform/src/supervisor/systemd.rs` — systemd transient-unit backend
  and the daemon unit/slice installer.
- `crates/platform/src/shell_env/` — executable search-path policy: bounded
  login-shell discovery, path validation, and executable resolution (see
  `docs/knowledge/guides/environment-resolution.md`).
- `docs/knowledge/guides/environment-resolution.md` — how the daemon and
  agents resolve `PATH` and executables.
- `crates/platform/src/process/sweep.rs` — ownership-marker sweep of a lost
  runtime generation.
- `crates/service-config/src/lib.rs` — `service.toml` schema, trust, and
  validation.
- `crates/worker-protocol/src/env.rs` — `BaseEnv`, the default environment
  allowlist, and the service-manager denylist.
- `crates/paths/fixtures/runtime-paths.json`
- `crates/daemon/src/host_state/`
- `crates/daemon/src/store/mod.rs`
- `crates/daemon/src/store/schema.rs` — `STORE_SCHEMA_VERSION`, the `MIGRATIONS`
  table, the `.pre-schema-<old>` backup, and `StoreSchemaError`.
- `crates/daemon/src/store/shape_guard.rs` — persisted-shape guard test and its
  `fixtures/shape/schema-<N>.txt` snapshots.
- `crates/protocol/src/version.rs`
- `crates/worker-protocol/src/version.rs`
- `scripts/release` — compatibility-constant check against the previous tag.
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
- `crates/daemon/src/session/diff.rs`
- `crates/daemon/src/session/hooks.rs`
- `crates/daemon/src/session/detector.rs`
- `crates/daemon/src/session/reconcile.rs`
- `crates/daemon/src/session/target.rs`
- `crates/daemon/src/session/procwatch.rs`
- `crates/daemon/src/runtime/`
- `crates/daemon/src/runtime/lifecycle.rs` — worker generation lifecycle
  engine: commit points, per-session locks, worker socket path refusal, an
  ended job ending the connect wait, post-timeout reconciliation.
- `crates/daemon/src/notify.rs`
- `crates/daemon/src/external/mod.rs`
- `crates/daemon/src/external/watch.rs`
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
- `crates/daemon/src/agent/native_launch.rs`
- `crates/daemon/src/agent/native_reference.rs`
- `crates/daemon/src/agent/profile.rs`
- `crates/daemon/src/agent/host/`
- `crates/xtask/tests/no_special_dispatch.rs`
- `crates/daemon/src/agent/builtin/`
- `crates/daemon/src/detect/mod.rs`
- `crates/daemon/src/detect/osc.rs`
- `crates/daemon/src/detect/manifest/mod.rs`
- `crates/daemon/src/detect/manifests/codex.toml`
- `crates/daemon/src/detect/manifests/claude.toml`
- `crates/daemon/src/detect/manifests/hermes.toml`
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
- `crates/daemon/src/agent/host/version_probe.rs` — the `semver-v1` data-driven version probe policy (probe argv and `[min, below)` release range).
- `crates/daemon/src/session/packages.rs` — reload and retention-checked uninstall.
- `crates/package/src/hash.rs` — SHA-256 helpers.
- `crates/package/src/directory.rs` — directory-to-archive builder behind `plugin link` and `cargo xtask package build`.
- `crates/package/src/catalog_state.rs` — persisted catalog high-water mark and revoked key ids.
- `crates/daemon/src/session/package_lifecycle/` — the `package.*` operations: validate from the verified archive, claim rules, catalog trust, record, reload.
- `crates/daemon/src/agent/host/claim.rs` — which package may serve which runtime id.
- `crates/daemon/src/api/handler/package.rs` — `package.*` handlers and the local-only gate.
- `crates/daemon/src/agent/profile.rs` — profile `package`/`digest` pins and `pinned_digests`.
- `crates/protocol/src/package.rs` — `package.*` payloads and stable error codes.
- `crates/cli/src/commands/plugin.rs` — `pohunek plugin list|inspect|install|link|update|select|enable|disable|uninstall|doctor`.
- `crates/cli/src/commands/plugin_profile.rs` — `pohunek plugin profile list|migrate`.
- `docs/knowledge/guides/runtime-packages.md`
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
- `crates/xtask/src/runtime_package.rs` — `cargo xtask package build|verify`.

Hermes compatibility evidence:

- `compat/hermes/compatibility-lock.json`
- `compat/hermes/README.md`
- `compat/hermes/goldens/manifest.json`
- `crates/xtask/src/hermes.rs`
- `crates/xtask/src/hermes_mock.rs`
- `crates/xtask/src/eval.rs`
- `crates/xtask/src/hermes_skill.rs`

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
- `crates/cli/src/hermes_integration/assets.rs`
- `crates/cli/src/hermes_integration/doctor.rs`
- `crates/cli/src/hermes_integration/skill.rs`
- `crates/cli/src/hermes_integration/assets/pohunek/plugin.yaml`
- `crates/cli/src/hermes_integration/assets/pohunek/__init__.py`
- `crates/cli/src/hermes_integration/assets/pohunek/cli.py`
- `crates/cli/src/hermes_integration/assets/pohunek/hooks.py`
- `crates/cli/src/hermes_integration/assets/pohunek/policy.py`
- `crates/cli/src/hermes_integration/assets/pohunek/redact.py`
- `crates/cli/src/hermes_integration/assets/pohunek/tools.py`
- `crates/cli/src/hermes_integration/assets/tests/test_plugin_runtime.py`
- `docs/knowledge/guides/hermes-operator.md`
- `docs/design/hermes-agent-integration.md`
- `docs/design/hermes-agent-integration-plan.md`
- `docs/public-api.md`

Protocol contracts and transport:

- `crates/client/src/discovery.rs`

- `crates/client/src/lib.rs`
- `crates/client/src/notifications.rs`
- `crates/client/src/transport.rs`
- `crates/paths/src/lib.rs`
- `crates/protocol/src/assistant.rs`
- `crates/protocol/src/compat/mod.rs`
- `crates/protocol/src/compat/v3.rs`
- `crates/protocol/src/envelope.rs`
- `crates/protocol/src/decimal.rs`
- `crates/protocol/src/limits.rs`
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
- `crates/overlay/tests/registry_contract.rs`

Assistant knowledge implementation work in progress:

- `crates/knowledge/src/lib.rs`
- `crates/knowledge/src/assistant.rs`
- `docs/knowledge/`

Design inputs:

- `docs/design/delegated-task-runs-rfc.md`
- `docs/design/universal-assistant.md`
- `docs/design/universal-assistant-plan.md`
- `docs/design/durable-session-workers-rfc.md`
- `docs/migrations/durable-session-workers.md`
- `docs/runbooks/durable-session-workers.md`
- `docs/public-api.md`

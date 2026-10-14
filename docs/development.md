# Development

Workspace layout, the CI gate set, fast test loops, and the release pipeline.
[AGENTS.md](../AGENTS.md) is the canonical instruction entry point; detailed
policy lives in its linked scoped guides.

The workspace is a Cargo monorepo (edition 2021, MSRV 1.96; CI and local
runs use the exact stable release pinned in `rust-toolchain.toml`, bumped with
`scripts/bump-rust-toolchain`) plus a Bun
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

Read **[AGENTS.md](../AGENTS.md)** first, then follow its task-specific
instruction routes.
Authoritative design lives in [docs/architecture.md](architecture.md);
the protocol contract in [docs/public-api.md](public-api.md).

## Contributor readiness

A fresh clone needs no maintainer caches and no host changes. Check the tools
of your contribution path and run the fixes it prints:

```bash
git clone https://github.com/zajca/pohunek.git && cd pohunek
scripts/dev-bootstrap --path rust          # rust, sdk, scripts, macos-lint, relay-db, native; repeatable
scripts/dev-bootstrap --json --path sdk    # one versioned JSON document, for agents
```

The script installs nothing: every failing row names the exact command or
step that fixes it. It needs `python3` >= 3.11 itself (`tomllib`). With an
explicit `--path`, the report also lists that path's focused checks, a
pointer to the final gates, and the CI jobs that hold the evidence this host
cannot produce (`checks` in `--json`).

Worktrees: `scripts/worktree-new <slug>` creates `pohunek-worktrees/<slug>`
beside the primary checkout on the branch `zajca/<slug>` (`--branch NAME`
picks another). It seeds `target/debug` from the main checkout by reflink
(btrfs/XFS) or not at all: it never falls back to a copy, so on a fresh clone
without a built main `target/`, or on a filesystem without reflinks, it fails
closed and says why. `scripts/worktree-new --no-seed <slug>` accepts a cold
first build instead; a plain clone works as well.

Focused checks per path (`scripts/dev-bootstrap --path NAME` is the source of
this table and prints a note per check):

| Path | Focused checks | CI-only evidence |
|------|----------------|------------------|
| `rust` | `cargo xtask affected` (`cargo ta`), `cargo t -p <crate>` | — |
| `sdk` | `bun install --frozen-lockfile`, `bun run typecheck`, `bun run lint`, `bun test`, `bun test sdk/ts/scripts` | `SDK workspace (macOS arm64)`, off macOS |
| `scripts` | `python3 -m unittest discover -s scripts/tests -p 'test_*.py'` (one file: `-p test_<name>.py`) | — |
| `macos-lint` | `scripts/darwin-check` | `platform contracts (arm64)` |
| `relay-db` | `python3 scripts/test-partitions run relay-db` with `POHUNEK_RELAY_TEST_DATABASE_URL` exported | `tests (relay DB, PostgreSQL)` |
| `native` | `cargo build --locked -p pohunek-daemon -p pohunek-session-worker -p pohunek-cli --bins`, then on Linux `POHUNEK_SYSTEMD_E2E=1 cargo test <suite> -- --ignored --test-threads 1` for each `<suite>` of `-p pohunek-platform --test systemd`, `-p pohunek-daemon --test systemd_durable_worker`, `-p pohunek-cli --test service_systemd`, `-p pohunek-daemon --test supervised_lifecycle`, `-p pohunek-cli --test service_upgrade`, `-p pohunek-cli --test workflow_systemd`; on macOS `cargo test --locked -p` each of `pohunek-platform`, `pohunek-daemon`, `pohunek-cli` | the other OS's suites, as a `ci-only` row: `platform contracts (arm64)` or `real systemd supervision` |

Final gates: run the applicable set once, on the final revision, before
calling the change done: the "Core gate set" of the
[gates skill](../.agents/skills/gates/SKILL.md) (plus its SDK section for SDK
changes), with the [verification guide](../.agents/instructions/verification.md)
as the full inventory.

A check this host cannot run stays unverified. `dev-bootstrap` reports it as
`ci-only` or `unavailable`, never `ok`, and names the CI job whose run is the
evidence; report it the same way, never as a green skip.

macOS: put the rustup proxies (`$CARGO_HOME/bin`, by default `~/.cargo/bin`)
first on PATH so rustup selects the `rust-toolchain.toml` pin, and install bun
at the `.bun-version` pin (`--path sdk` prints the installer). The system
`/usr/bin/python3` is too old for `tomllib`; put a Homebrew or uv-managed
`python3` >= 3.11 first on PATH. `scripts/dev-bootstrap --path native` reports
the launchd `gui/<uid>` domain as `launchd-gui-domain`; run it, and the
launchd suites, from the logged-in GUI session, because an SSH-only session
has no such domain. The `macos-lint` rows are `not-applicable` there:
`scripts/darwin-check` lints natively. The `platform contracts (arm64)` CI job
runs the `dev-bootstrap` scenarios and this native check on the macOS runner.
The `macOS tier packages (arm64)` job runs the macOS tests of the packages the
lane does not, on main and nightly only; `scripts/test-partitions` documents
the selections.

## Gates

CI treats warnings as errors. This list is the core of the set; the gates
skill and the verification guide linked above are the full, authoritative set.
Run the full set before calling anything done:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features
cargo build -p pohunek-session-worker --bin pohunek-sessiond  # daemon tests spawn it by path
cargo nextest run --profile ci --test-threads 4 --workspace --all-features
cargo test --doc --workspace --all-features  # nextest excludes doctests
cargo build --workspace --release
cargo xtask docs check          # knowledge bundle: schema/drift/secrets/runbooks
```

Routine loops (cargo-nextest profiles live in `.config/nextest.toml`; see the
[verification guide](../.agents/instructions/verification.md) for watcher and
CI-timing variants):

```bash
cargo t                        # all fast tests, no PTY/DB fixtures
cargo t -p pohunek-daemon       # fast tests in one crate
cargo ti                       # fast daemon/client/session-worker surface
cargo tw                       # unfiltered full suite, four test processes
bun test sdk/ts/sdk/test/config.test.ts -t "one case"  # one TypeScript test file, name pattern
bacon                          # watcher: profile-fast nextest loop (bacon.toml)
python3 scripts/test-partitions run cli    # exact CI shard (core/daemon/relay/cli/relay-db/heavy)
python3 scripts/test-partitions check      # verify all tests belong to exactly one shard
# macOS only, from a nextest archive: `check-macos` (`--scope lane` for the PR lane archive) and `run-macos` (PR lane), `run-macos-tier` (main/nightly); see verification.md, Fast loops
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

The `pohunek-cli` Hermes tests also need a test interpreter that stands in for
the pinned Hermes runtime: a canonical Python >= 3.10 that imports `yaml`, owned
by you or root with no group/other write bit on the file or any parent
directory. A distribution interpreter qualifies once PyYAML is
installed; the stock macOS `/usr/bin/python3` (3.9) and shared-tree Homebrew or
framework Pythons do not. Without it those tests fail with "no usable test interpreter",
never skip. Run `scripts/dev-bootstrap`: it checks `POHUNEK_PYTHON_BIN` (or, when
unset, `python3` on `PATH` and `/usr/bin/python3`) and prints the uv commands,
mirroring the CI step in `.github/workflows/ci.yml`, that install a private
CPython 3.12 with PyYAML and the `export POHUNEK_PYTHON_BIN=...` line to keep in
your shell profile.

CI runs four fast matrix shards and separate heavy jobs without waiting for
Clippy or release builds. Heavy uses four test processes and the real worker
binary. Relay heavy tests (the only PostgreSQL fixture consumers) run in a
dedicated `relay-db` job whose `POHUNEK_RELAY_TEST_DATABASE_URL` points at a
disposable PostgreSQL service; a `paths-filter` job skips that job — including
the Postgres service — when no relay-relevant file (`crates/relay*/**`,
`migrations/**`, `Cargo.lock`, the CI workflow) changed. The full `ci`/`local` profiles remain unfiltered. Existing ignored tests retain
their opt-in status. Each CI shard uploads a uniquely named JUnit artifact;
sequential local fast shards overwrite `target/nextest/ci/junit.xml`, while heavy
writes `target/nextest/heavy/junit.xml`. The runtime package jobs run their
`<runtime>_package` binary with `--run-ignored all` under the `package` profile
(one test at a time, no retries, no fail-fast, a hung test killed after two
minutes) and upload `target/nextest/package/junit.xml` as
`package-junit-<runtime>`, also when the tests fail. Each release evidence row
writes the wall clock and outcome of its consumer run as a one-testcase JUnit
report, uploaded as `matrix-junit-<runtime>-<target>`. `scripts/ci-timings junit`
reads all of them.

The official runtime package jobs (`pi-package`, `codex-package`,
`claude-package`, `hermes-package`) take their pull-request selection from
`cargo xtask package affected [--base REF] [--json]`, which the
`package-selection` CI job runs against the pull request's base commit. It
collects changed files like `cargo xtask affected` (committed diff against the
merge base, staged, unstaged, untracked) in the Git repository of the working
directory. A file below `runtime-packages/<p>/` or `compat/<p>/` selects package
`<p>`; prose (`docs/` outside `docs/knowledge`, root `*.md`, the GitHub issue
forms and pull request template) selects nothing; every other file (including
`compat/<d>/` of a `<d>` without a package directory) and every file a Rust source
embeds with `include_str!`/`include_bytes!` (for example
`compat/codex/subagent-hooks.json`) is a core change that selects every package.
The official packages are the directories under `runtime-packages/`. `--json`
prints `{"core_changed": bool, "packages": [...]}` on stdout and the per-file
reasons on stderr; any Git or file-system failure exits non-zero with nothing on
stdout. The selector then downloads the signed runtime catalog, the
attestations and the package archives of the GitHub Release `v<workspace
version>` (never "latest") with `gh release download` and the run's read-only
token into `$RUNNER_TEMP`, outside the checkout (the step holding the token runs
no checkout code; the workspace version is read in the step before), and runs `cargo xtask package
baseline --anchor packaging/runtime-catalog-anchor.json --json` on them (see
"Release baseline" in
[runtime package archive](knowledge/concepts/runtime-package-archive.md)). A
package is selected when the core changed, the package changed, or its
baseline is not `verified`; so a pull request skips a package only when the
package and the core are unchanged and the signed release proves the package's
bytes, lock and fixtures. A failed download, or one that runs longer than
the workflow's `PACKAGE_BASELINE_FETCH_TIMEOUT`, leaves the directory empty, every
package is `missing` and every package job runs, while the selector stays
green; only an unreadable checkout (the verifier exits non-zero, or verdicts of
another release or of an unknown shape) fails the selector. Between a version
bump and its release no baseline exists, so every package job runs. A package
job skips only when the selector succeeded and reported its package as `false`;
a failed selection, and push, schedule and dispatch runs, run every package
job. Cache state never enters a package job's condition.
Each package job builds its archive with `cargo xtask package build
--evidence-dir` and uploads the builder evidence (checksum, manifest projection,
provenance input, compatibility vector; see
[runtime package archive](knowledge/concepts/runtime-package-archive.md)) as
`package-evidence-<runtime>` right after the build, so it survives a failing
test run. When the selector verified the package's baseline, the job then
requires the SHA-256 of the archive it rebuilt to equal the baseline digest
(`<p>_baseline_digest`); the selector rebuilt the same commit with the same
builder and found the released digest, so a difference means the package build
is not reproducible and fails the job. The release build passes `build-packages` a separate evidence
directory and uploads it as `package-evidence`, which no release job downloads.

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
| `POHUNEK_CONSUMER_RUNTIME` | runtime id to exercise (`pi`, `codex`, `claude`, `hermes`, ...) |
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
verifies that the daemon and the worker execute the layout copies and that the
executed image hashes to the installed bytes (on Linux through `/proc/<pid>/exe`,
on macOS through the host process inspector); runs
every command as the layout's `pohunek` subprocess: `plugin install --catalog`,
`plugin list`/`inspect` (official, enabled, selected), the driver's
`integration install` when it has one, `host inspect local`
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
shows readiness, the marker that makes the model stub hold a reply (none when
the stub holds every reply until the gate opens), which integration reports
the conversation id (none, a hook that `integration install --profile`
registers in the profile's config home, or the Hermes operator plugin), whether
the resumed process reports itself again, a `prepare` function that writes the
upstream's hermetic configuration (throwaway home or config directory, the
loopback stub as its only model endpoint, every update, telemetry and
marketplace switch off) and returns the profile `[env]`, and the text of the
reply as it appears on screen. Reuse a stub from `crates/cli/tests/support/`
or add one beside them. Drivers exist for `pi`, `codex`, `claude` and `hermes`.

The Hermes driver writes a private Hermes home whose only model provider is
the loopback endpoint of `crates/cli/tests/support/hermes_provider.rs` (the
one the Hermes package tests use), with every other request going to that
endpoint as a fail-closed proxy, and sets `PYTHONDONTWRITEBYTECODE=1` and
`HERMES_DISABLE_LAZY_INSTALLS=1` in the profile. The endpoint accepts at most two
discovery rounds in total (the pinned release sends one per process) and at
most two turns, and holds every completion until the gate opens. After the package install the scenario runs
`integration install --agent hermes --hermes-home <home> --hermes-bin
<resolved upstream>` (the CLI refuses a path with a link component, so the
stage's `bin/hermes` link is resolved) and requires the install and `integration
status` results to report `provenance` `source=package` with the archive's
package id and digest, so the operator plugin comes from the installed
package. The plugin hook reports the conversation id, the session resumes by
it, and the resumed process reports itself again. The upstream is a
provisioned stage (`cargo xtask compat stage-upstream --runtime hermes`); a
used stage is not stable, so every run starts from a pristine copy that
`compat verify-stage` accepted.

CI runs the Hermes driver in row mode in the `hermes-package` job, after the
real Hermes session test: it restores and verifies the pristine stage, builds
the consumer with `scripts/release-workflow/build-consumer`, and runs it the
way `run-row` does (a scrubbed environment, the stage's `bin` first on `PATH`,
a short root outside any Git checkout) against the default-feature binaries the
job built, copied aside before a later cargo step rebuilds `target/debug`, and
the archive the job built from `runtime-packages/hermes`; the report must name
that archive's digest. The release rows and the archive smoke run the same
driver on the release binaries and the released Hermes package.

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
- **Integration/E2E tests, not unit tests** ([testing policy](../.agents/instructions/verification.md#testing-policy)): a
  test drives cooperating production components through a supported boundary,
  or the real product processes — never a unit test. Extending an existing
  suite does not exempt an added test from that boundary; the unit-tier
  tests still in the suite are #671 migration work, not everyday additions.
  The protocol and state machines have rich suites — extend them first.

## Release

`scripts/release` bumps the workspace version and refreshes the lockfile, then
requires successful main and nightly CI evidence for the exact release commit
before tagging `vX.Y.Z`. By default it pushes the release branch before waiting
for that evidence and pushes the tag after verification. `--no-push` checks
already-existing evidence and leaves the commit and tag local; it does not
dispatch CI or push either ref. The Release workflow
(`.github/workflows/release.yml`) re-runs the gates on the tag and then builds,
assembles and publishes the release in one fixed flow:

1. **Producers** upload workflow artifacts only: `release-build.yml` (reusable)
   builds the glibc and MUSL x86_64 CLI and daemon archives, the glibc relay
   archive, the official runtime package archives (every `runtime-packages/*`
   directory: `package verify`, then `package build`, named
   `pohunek-runtime-<runtime>-<version>.tar.zst` with a `.sha256`; the builder
   evidence goes to the separate `package-evidence` artifact) and the SDK
   tarballs; the macOS jobs build, ad-hoc sign and verify the Apple Silicon
   archives. No macOS job uses secrets or a protected environment.
2. **Evidence** (`release-evidence.yml`, reusable): one job per
   (runtime x target) row of `compat/matrix.json` extracts the binaries from the
   daemon ARCHIVE, stages the pinned upstream (npm, or a provisioner script,
   for which the job installs Python and uv), runs the out-of-process consumer
   suite and emits `attestation-<runtime>-<target>.json`, plus
   `junit-<runtime>-<target>.xml` with the timed consumer run (written also when
   the consumer fails, uploaded as `matrix-junit-<runtime>-<target>`, never a
   release input); the download-only
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
pages under `docs/` are bundled into every native component archive. Daemon archives also contain
`packaging/smoke-hermes-plugin-release`. Release automation provisions the
source-locked Hermes runtime without provider credentials, extracts each daemon
archive, builds the Hermes runtime package from `runtime-packages/hermes` with
`scripts/build-hermes-package`, runs the model-free compatibility gate against
the extracted binaries and that package, and executes the packaged smoke
script. The smoke takes the extracted daemon archive, the package archive, a
signed catalog that authorizes it, and the anchor that trusts the catalog's
signer; these archives are smoked before assembly adds the signed release
catalog, and the macOS archive never carries a package, so `cargo xtask hermes
throwaway-catalog` writes that catalog and anchor with a random signing key
that is never stored. Operators can repeat the same
script with an explicitly supplied, preinstalled pinned Hermes executable. It
copies the binaries into a private prefix with the anchor beside the daemon,
starts the daemon in an isolated temporary profile/state, installs the package
with `pohunek plugin install --catalog`, requires the Hermes executable rather
than downloading it, and fails if install, status, doctor, or uninstall fails
or an installed-state result does not report the package and its digest as the
plugin's provenance.

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
draft/publish semantics of the `publish` job. The rehearsal, like a release,
builds and tests every official package from its own commit: no release
workflow or script reads the per-pull-request selection or a release baseline
(`scripts/tests/test_release_workflow.py` pins this).

### Adding a runtime package or row

Add `runtime-packages/<runtime>/`, `compat/<runtime>/` (lock and npm project) and
the rows to `compat/matrix.json` (`cargo xtask compat matrix-check`), and a driver
entry for the consumer suite. The builder evidence that `build-packages` and the
package job write copies the lock's `upstream` object whole, whatever its kind
(npm package, provisioned runtime), and needs only `upstream.release` in it.
Nothing in `release.yml`, `release-build.yml` or `release-evidence.yml` names a
runtime: the package job, the row matrix and the smoke derive them from those
directories and files. A new target needs a runner
mapping in `scripts/release-workflow/row-matrix`, a build leg in
`release-build.yml` and the policy entries in `packaging/release-policy.json`.
The per-pull-request package job in `ci.yml` needs the runtime in the
`package-selection` loop, the selector outputs `<runtime>` and
`<runtime>_baseline_digest`, a "Match the release baseline" step after its
evidence upload, and an entry in `PACKAGE_JOBS` of
`scripts/tests/test_ci_tiers.py`, which fails until every `runtime-packages/`
directory has all of them. The `package-version-bump` job needs nothing per
runtime: it checks every official package against the last published release.

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

`cargo xtask compat stage-upstream --runtime <rt> --out <dir>` installs the upstream release a runtime's lock pins (network needed), from the committed `compat/<rt>/npm/package-lock.json` with `npm ci` or, for an `upstream.kind` `provisioner` lock (Hermes), with the repository script it names, run in place (a provisioned stage is never moved, and is verified from a pristine copy before each use because Hermes writes into its own checkout), and writes `STAGE.sha256` and `STAGE.links`; `cargo xtask compat verify-stage --runtime <rt> --dir <dir>` re-checks the tree offline. A lock edit changes attestation digests. See the knowledge page `concepts/upstream-staging.md`.

`cargo xtask release assemble` turns the producer artifacts into the release bundle: it requires exactly the inventory that `packaging/release-policy.json` and `compat/matrix.json` imply, recomputes every checksum, binary-set digest and attestation from bytes, signs the catalog, rebuilds the daemon archives with the catalog and packages and writes `release-inventory.sha256`; `cargo xtask release verify-inventory --dir <bundle> --anchor <file>` re-checks a bundle against the pinned trust anchor. See the knowledge page `concepts/release-bundle.md`.

After a published stable release, `.github/workflows/notify-tap.yml` (a `workflow_run` of `Release`, the only workflow with a secret, `TAP_DISPATCH_PAT`) sends the Homebrew tap `zajca/homebrew-pohunek` a `pohunek-work-release` repository dispatch for the `pohunek` formula and the tag, and the tap bumps it; pre-releases are skipped. `scripts/tests/test_notify_tap_workflow.py` pins it.

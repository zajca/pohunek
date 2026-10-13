# Knowledge Bundle Log

## Unreleased (2026-10-13, bounded PTY input stall, #867)

- `service.toml` schema 5 adds the required key `[input]`
  `write_stall_timeout_ms` (3000 on fresh installs). It is a whole millisecond
  count in `1..=600000`. A missing key makes the daemon refuse startup. Schema
  4 files, the layout `main` released before this change, are now an upgrade
  source like schemas 2 and 3: `pohunek service upgrade` migrates them to
  schema 5 keeping every recorded value and their `[waits]` table, and assigns
  the 3000 ms stall bound. Schemas 2 and 3 migrate straight to schema 5 with
  the same default. The value is the longest time one `session.input` or initial input
  write may wait without the agent reading a byte; it restarts after every
  accepted byte. Attached terminal input is not bounded.
- The daemon sends the bound to each worker in `Initialize`. A write that
  exceeds it fails with the new public code `session_input_stalled` (runtime
  class). Part of the plan may already have been written. `session.new` with
  initial input then removes the session it was creating; `session.input`
  leaves the session running.
- A worker that negotiated a protocol below 7 receives no bound and keeps
  unbounded writes.
- The 3000 ms default is below the client's 5 s request deadline, so a
  fire-and-forget caller receives the typed error instead of `request_timeout`.

## Unreleased (2026-10-13, doctor hints for unloadable packages, #991)

- `pohunek plugin profile list` reports a pinned profile whose pinned digest is
  installed but cannot be loaded as `package_unavailable`, when the pin names
  that digest's package. The detail names `pohunek plugin doctor` only; the
  migration is not offered because the pin already names the digest. A
  loadable package that serves another runtime, or a pin naming another
  package than the digest's, is still `needs_migration`.
- An unpinned profile over an official alias (`codex`, `claude`, `hermes`)
  whose official package (`pohunek.runtime.<alias>`, matched by package id) is
  installed but was left out of the runtime registry after a rebuild is refused with `runtime_incompatible` (it was
  `runtime_not_installed`). Its recovery names `pohunek plugin doctor` and the
  migration after the repair, not the install, which the package's root or
  identity check refuses. The refusal is decided from the package report the
  registry rebuild records; no protocol code was added.
- The runtime packages guide, the runtime package recovery runbook and
  `docs/public-api.md` list the new states and refusal.

## Unreleased (2026-10-13, package version bump gate, #894)

- `cargo xtask package bump` fails a runtime package whose archive differs
  from every released archive of its runtime while its `runtime.toml` version
  is not strictly above the released versions (semver). It reuses the verdicts
  of `package baseline` and adds the decisions `pass`, `undecided` (catalog
  absent) and `fail` (`version_not_bumped`, or a catalog that does not verify).
- The `package version bump` pull request job in `ci.yml` compares with the
  last published release at or below the workspace version.
- The Codex package is bumped to `1.1.0` for its transcript tree declaration.
- The rule is in the runtime packages guide; the archive concept describes
  `version_not_bumped` and the bump decisions.

## Unreleased (2026-10-13, workflow plugin jobs and services, #326)

- `workflow.toml` accepts `[jobs.<name>]` (`argv`, `every_s`, optional
  `timeout_ms`) and `[services.<name>]` (`argv`); the consent review and
  `inspect` show them.
- `pohunek workflow enable` installs and starts one user service manager unit
  per job and service (systemd user units, launchd agents) of the
  `pohunek service` installation; `disable` and `uninstall` stop and remove
  them. New error codes: `workflow_service_not_installed`,
  `workflow_unit_refused`, `workflow_units_failed`, `workflow_unit_stale`,
  `workflow_unit_unknown`.
- Every unit runs the hidden `pohunek workflow __exec`, which refuses residue
  units, records `overlap_skipped` while a run is active, runs a job once its
  wall-clock `every_s` has passed, applies a service's restart backoff, never
  forwards the origin pair or the service manager's environment, and stops
  the run's process group on `SIGTERM`, then exits 0 so a unit its service
  manager stopped reads as stopped (the run is recorded `interrupted`).
  systemd plugin units use `KillMode=mixed`, so a stop signals only the
  trampoline; the final `SIGKILL` still reaches every remaining process.
  Uninstall applies its own refusals first, then disables the plugin and
  removes its units; it fails while a unit cannot be removed. Installing a
  recorded archive again never touches its units. An enabling install
  checks everything and takes `units.lock` before it records the plugin; a
  failure after that leaves it installed disabled
  (`workflow_installed_disabled`, with the failure's code as `cause`, and
  a message that says whether undoing the failed enable succeeded). A job outside the `[jobs]` bounds is
  refused at run time too.
- Each plugin with units keeps the names of its units in a unit record under
  the store root. `disable` and `uninstall` stop and remove the recorded units
  as well as the manifest's, so a plugin whose installed files fail
  verification no longer leaves running units behind; `uninstall` refuses to
  remove the plugin record while units are still recorded.
- `enable` refuses an installation recorded for another pohunek version
  (`workflow_service_not_installed`, naming both versions), since its units
  would run that version's trampoline.
- A stop request that arrives while the trampoline verifies a unit ends the run
  before its child starts. A stop request during a refused service's restart
  pacing ends the trampoline as stopped (exit 0), not as a refusal. Run records gain the kinds `job` and
  `service` and the field `backoff_ms`.
- New owner knobs `[jobs]` (`timeout_default_ms`, `timeout_max_ms`,
  `min_interval_s`, `check_period_s`) and `[services]`
  (`restart_initial_ms`, `restart_max_ms`, `stable_after_ms`).
- The architecture concept says which login units exist besides the daemon.
- `uninstall` of an id with no record and no store root is refused as
  `workflow_plugin_unknown` before any lock is taken, the same answer as an
  existing store without the record. With `--purge` the unit lock is taken in
  creating mode, so an enabling install cannot record the plugin between the
  check and the removal; a recorded plugin is still disabled and its units
  removed first.
- A failure while marking a fresh plugin enabled that may have written the flag
  disables it again before `workflow_installed_disabled` is reported.
- `enable` records a plugin's unit names before it marks the plugin enabled.
  When the unit record cannot be written, the command fails with
  `workflow_store_failed` and changes nothing: an enabled plugin whose units
  keep running stays enabled instead of being disabled.
- The unit record is bounded at three times the manifest limit (192 KiB), so the
  record of a manifest at its size limit with short service names is read.
- On macOS, `__exec` ignores an inherited `XDG_RUNTIME_DIR` when the
  installation records the default runtime root
  (`/private/tmp/pohunek-<uid>`), and leaves it out of the run's environment;
  before, every run of such an installation was refused after
  `launchctl setenv XDG_RUNTIME_DIR`.
- A service's restart backoff counts the child's own lifetime, from its spawn
  until it ends, so verification and output draining never make an
  immediately failing service look stable; a run that started no child is
  short.
- Without a recorded `pohunek service` installation, `disable` and `uninstall`
  fail with `workflow_units_failed` and keep the unit record when the service
  manager cannot be reached while units are recorded.
- Known limit on macOS: when `__exec` itself is killed with `SIGKILL`, the
  run's process group can survive the unit; a later start can run alongside it
  and `disable` and `uninstall` do not find it. The guide documents it; a
  reconcile mechanism is a follow-up.

## Unreleased (2026-10-13, identity sequences ordered per process, #879)

- The worker orders identity claims and releases per reported process (pid and
  kernel start identity): a report must be newer than every claim and release
  accepted for that same process, and reports of different processes are never
  compared. A descendant's report of its own process, even at
  `sequence = u64::MAX`, no longer makes the launch root's later reports stale,
  so the root still establishes the launch identity and updates its native
  reference. Across processes the latest accepted claim is the active identity.
- The worker keeps the sequence of every live reporting process and never
  evicts it, so a released process cannot become active again through its own
  older claim delivered late. Marks of exited processes are pruned when room is
  needed; the PTY root and the launch process are always kept. When live
  reporters besides those two fill the bound (16), a further process's report
  is refused with the new reason `identity_reporters_exhausted` until one
  exits; an exited process its parent has not reaped counts as exited. Before
  the launch identity exists, a launch agent's report with a native reference
  is verified as a launch claim even at the bound and kept only when it
  establishes the launch identity, so a full table cannot keep a descendant
  launch process from being pinned. A release answered `ok: false` still
  orders its process's later claims.
- A report of another process accepted as the active identity clears the
  background owner's snapshot (`agent_changed`) whatever its sequence, since
  sequences of different processes are never compared.
- `docs/public-api.md`, the runtime packages guide and the sessions concept
  state the per-process order; the sequence contract is a value strictly
  increasing across every hook event that reports the same process.

## Unreleased (2026-10-13, workflow run --json and verbatim arguments, #325)

- `pohunek workflow run [--json] <plugin> <action> [<args>...]`: every
  argument after `<action>` reaches the action verbatim, including `--json`,
  `--`, `-h`, `--host` and empty strings, so pohunek's own options go before
  `<plugin>`. A `--` after the action is an argument for the action.
- `--json` captures the action's stdout up to its cap and prints one CLI
  envelope whose `ok` is the action's single JSON document byte for byte;
  stderr still streams. The context's `output` is `json`.
- New error codes, each an envelope without output bytes:
  `workflow_action_failed` (non-zero exit or a signal the CLI did not send;
  carries `exit_code` and `signal?`, and the CLI exits with the action's
  status), `workflow_output_invalid` (exit 0 but stdout is not exactly one
  JSON document, including arrays and objects nested deeper than 126 levels;
  exit 1) and `workflow_output_incomplete` (exit 0 but an
  escaped process held stdout open; exit 1). The run log gains the outcomes
  `output_invalid` and `output_incomplete`.
- The workflow plugins guide, `docs/public-api.md` and `docs/cli.md` describe
  it.

## Unreleased (2026-10-13, shell completion of workflow plugins, #325)

- Dynamic shell completion completes installed workflow plugin ids for
  `workflow inspect`, `enable`, `disable`, `uninstall` and `run` (`run` offers
  only enabled plugins), and the actions of the typed plugin for
  `workflow run <id>`, read from its installed `workflow.toml` after its files
  verify. The lookups read the local store only, without the daemon, within the
  completion deadline; they create, lock and write nothing, and a missing,
  corrupt or unsafe store completes nothing silently. A line with a remote
  `--host` completes no workflow plugin, matching `workflow_local_only`. Static
  scripts never read the store.
- `docs/cli.md`, `docs/features.md` and the `setup` and `workflow-plugins`
  guides describe it.

## Unreleased (2026-10-13, verify-inventory checks package digests before decoding, #972)

- `cargo xtask release verify-inventory` takes a required `--anchor <file>`
  and refuses with a trust-anchor mismatch when any daemon archive ships
  another anchor, before it verifies the catalog or reads a package. Each
  top-level package archive is then compared with the digest of the catalog
  verified under that anchor before it is decompressed, so a tampered package
  is refused as a package-archive mismatch without reaching the decoder
  (`concepts/release-bundle.md`).

## Unreleased (2026-10-13, managed hook wording, #151)

- The debug-daemon runbook and the public API call the scripts that
  `integration install` writes "the managed hooks" and name the compiled Codex
  and Claude integration handlers as what registers them; neither is called an
  adapter.

## Unreleased (2026-10-13, Hermes docs match the package-only runtime, #147)

- The Hermes operator guide (and the operator skill generated from it) states
  what the official package carries, the launch, resume and detection
  contract in `runtime.toml` and `detect.toml`, instead of a built-in contract
  it preserves.
- `docs/features.md`, `docs/README.md` and `docs/ROADMAP.md` describe Codex,
  Claude Code and Hermes as served only by their official runtime packages:
  only `shell` is built in, the Linux x86_64 daemon archives ship the
  packages, macOS daemons ship none until #929, and without its package a bare
  runtime is `agent_profile_not_found` and a host profile over it
  `runtime_not_installed`.

## Unreleased (2026-10-13, profile list reports unserved bases, #958)

- `pohunek plugin profile list` reports `builtin` only for a profile over
  `shell`, the one runtime built into the daemon. An unpinned profile whose
  base no installed package serves, such as `codex`, `claude`, `hermes` or a
  third-party runtime, is `package_not_installed`: its launches fail with
  `runtime_not_installed`. The detail names the official package install for
  an official alias, a package that serves the base otherwise, then
  `pohunek plugin profile migrate <name>`.
- When an installed package that may serve such a base cannot be loaded (its
  root or descriptor fails verification; for an official alias only that
  alias's official package, `pohunek.runtime.<alias>`, counts), the profile is `package_unavailable` and the
  detail names `pohunek plugin doctor`, which reports the fault and its
  repair.
- The `runtime-packages` guide, the `runtime-package-recovery` runbook and
  `docs/cli.md` list the state.

## Unreleased (2026-10-13, package archives decoded only after digest authorization, #967)

- `plugin install --catalog` looks the raw archive digest up in the verified
  catalog before it decompresses anything. Bytes the catalog does not list,
  such as a corrupted copy of a listed archive or a file that is not an
  archive, are refused with `package_untrusted` instead of
  `package_archive_invalid`; nothing reaches the zstd decoder. A listed archive
  that does not decode is still `package_archive_invalid`, and the identity,
  version, core and platform checks after decoding are unchanged.
- `package::archive_digest` computes the package digest of raw bytes without
  decoding them.
- `.cargo/audit.toml` ignores RUSTSEC-2026-0344 (ruzstd 0.9.0 decoder memory
  blow-up) until the package format migration to ruzstd >= 0.9.1 (#969).
- The `runtime-package-archive` and `runtime-catalog` concepts, the
  `runtime-packages` and `hermes-package` guides and the
  `runtime-package-recovery` runbook describe it.

## Unreleased (2026-10-13, workflow run, owner knobs and run log, #148 #325)

- New `pohunek workflow run <plugin> <action> [-- <args>...]` runs one
  declared action of an installed, enabled plugin in text mode, after
  re-verifying its files. It runs without a shell, in the plugin's `files/`
  directory, with stdin from `/dev/null`, in its own process group, and with
  an environment of exactly `POHUNEK_PLUGIN_ID`, `POHUNEK_PLUGIN_VERSION`,
  `POHUNEK_PLUGIN_DIGEST`, `POHUNEK_PLUGIN_CONFIG_DIR`,
  `POHUNEK_PLUGIN_STATE_DIR`, `POHUNEK_PLUGIN_CONTEXT`, `POHUNEK_CLI`, the set
  `HOME`/XDG roots, and the
  `POHUNEK_SESSION_ID`/`POHUNEK_DAEMON_ID` pair when both are set, so
  `plugin_self_target_denied` still guards the hosting session.
- `POHUNEK_PLUGIN_CONTEXT` is a compact schema-1 JSON object with the plugin
  identity, the invocation (`kind`, `name`, ULID `run_id`), the output mode and
  the CLI version and protocol range.
- Deadlines, per-stream output caps and the `SIGTERM`-grace-`SIGKILL` stop come
  from the new owner knobs file `<config>/pohunek/workflow.toml` (`[limits]`,
  `[logs]`; strict, owner-only, defaults when absent). The CLI also stops the
  action's group when it receives `SIGINT`, `SIGTERM` or `SIGHUP`. Exit
  statuses: the action's own, 124 at the deadline, 125 over a cap, 128 plus
  the signal number when interrupted.
- Every run of an installed plugin, refused or not, appends a content-free
  record to `<state>/pohunek/workflow-plugin-state/<id>/logs/runs.jsonl`,
  except runs that fail before the run log is opened (for example invalid
  owner knobs, an unknown plugin, or a store or path failure).
- The action's output is delivered up to its cap whatever the speed of the
  CLI's own stdout and stderr consumer, unless the CLI is signalled, which
  ends the run as `interrupted`. An output pipe that an escaped descendant
  holds open is cut after one second not spent waiting for that consumer,
  whether the descendant writes or not, with a stderr warning.
- New error codes `workflow_config_invalid`, `workflow_plugin_disabled`,
  `workflow_action_unknown`, `workflow_action_args_refused`,
  `workflow_origin_incomplete`, `workflow_timeout_refused`,
  `workflow_context_too_large`, `workflow_run_log_failed`,
  `workflow_spawn_failed`, `workflow_run_failed`, `workflow_action_timeout`,
  `workflow_output_limit` and `workflow_action_interrupted`. No protocol
  method, event or SDK type was added; `--json` for `workflow run` is not yet
  available.
- Documented in the `workflow-plugins` guide, `safety/trust-model`, the source
  map, `docs/cli.md` and `docs/public-api.md`.

## Unreleased (2026-10-13, Hermes operator plugin only from the package, #147)

- The CLI installs and updates the Hermes operator plugin only from the
  installed, enabled and selected official `pohunek.runtime.hermes` package. It
  carries no copy of the operator payload; the core hook reporter stays
  compiled in and is installed beside the package payload.
- Without an installed Hermes package, `pohunek integration install|update
  --agent hermes` fails with `hermes_package_not_installed` (class
  `configuration`) and changes nothing; its recovery hint names the official
  package install. `integration status` and `doctor` report the same code in
  `package_source_error`, and doctor fails `asset_source` with that hint.
  `hermes_package_source_missing` is gone: a removed recorded package is now
  `hermes_package_not_installed` with `package_source_missing=true`.
- An install an older CLI rendered from its compiled copy (marker version 1 or
  2, or version 3 with `source=embedded`) is never migrated automatically.
  Status and doctor report `hermes_package_not_installed` while no package is
  installed, `integration uninstall` removes it without a package, and
  `integration update` after the package install rewrites it with
  `source=package`.
- A macOS host has no source for the operator plugin until darwin packages are
  attested (#929).
- The regenerated operator skill changes the package's operator digest, so a
  package-backed install reports `outdated=true` once the new package is
  installed, until `pohunek integration update --agent hermes` rewrites it.
- The `hermes-operator` guide (regenerated Hermes skill, current bare-status
  wording), the `hermes-package` guide, the source map, `docs/install.md`,
  `docs/cli.md`, `docs/runbooks/hermes-operator-plugin.md`,
  `docs/migrations/hermes-operator-plugin.md`, `docs/architecture.md`,
  `runtime-packages/README.md` and `compat/hermes/README.md` describe it.

## Unreleased (2026-10-13, built-in Hermes removed from the daemon, #147)

- The daemon compiles no Hermes in: the official `pohunek.runtime.hermes`
  package, installed from the release catalog, is the only source of
  `hermes`, as the Codex and Claude Code packages are of `codex` and `claude`.
  The shell is the only built-in runtime. On a host without the package, a
  bare `hermes` is `agent_profile_not_found` and an unpinned `base = "hermes"`
  host profile answers `runtime_not_installed`; each recovery hint names
  `pohunek plugin install runtime/packages/hermes.tar.zst --catalog
  runtime/runtime-catalog.json` (the profile hint then names `plugin profile
  migrate <name>`). While the package serves `hermes`, an unpinned profile
  over it needs that migration.
- The release catalog lists the Hermes package for the Linux x86_64 targets
  only, so a macOS daemon has no source of Hermes (#929).
- A session launched from the built-in Hermes of an earlier release (pinned,
  or a v0.33.0 record without a launch binding) answers
  `runtime_builtin_removed` on resume, fork and every interaction (input,
  attach, resize, rename, metadata) while no package serves `hermes`; its hint
  names the install and `pohunek session new --agent hermes`. With the package
  installed the live session can be used again, resume answers
  `runtime_served_by_package` and fork `agent_fork_unsupported` (Hermes
  declares no native fork). `pohunekd upgrade-preflight` reports every live
  built-in Hermes session as `would_lose_recovery`
  (`runtime_source_not_served`) whether or not it could resume; with the
  package installed before the upgrade a non-resumable one is `adoptable`.
  No earlier release archive carries the package to install before the
  upgrade.
- Disabling or uninstalling the Hermes package leaves `hermes` unserved; no
  official alias has a built-in to fall back to.
- The compiled `hermes-v1` version-probe parser is removed. The compiled
  parsers are `semver-v1` and `semver-line-v1`; a descriptor naming any other
  parser id parses, but its runtime reports `supported: false` and a launch is
  refused with `agent_runtime_unsupported`. The probe sandbox sets no Hermes
  home or Python variables and passes the launch `PATH`.
- `daemon.doctor` checks the embedded manifest only for `shell`
  (`agent_manifest_embedded_shell`); `agent_manifest_embedded_hermes` is gone.
- `package.bind_profile` for a `hermes` base no package serves answers
  `package_profile_target_invalid`; `package_profile_base_builtin` is the
  shell's only.
- A `hermes` process outside pohunek or in the foreground of a shell session
  is recognized only through the installed package's detection manifest.
- The `no_capable_agent` hint names the official `codex`, `claude` or
  `hermes` package install from a release archive whose catalog lists the
  platform, or `--agent` with a capable host profile; a `hermes` on the
  daemon's `PATH` alone does not make the assistant capable.
- The v0.34.2 built-in Hermes descriptor and manifest are frozen as goldens in
  `compat/hermes/builtin-v0.34/`; the package tests compare the package with
  them, and a new clean-host scenario covers the refused bare launch, the
  assistant without a capable agent and the launch after the catalog install.
- `pohunek integration ... --agent hermes` stays the CLI-run Hermes operator
  plugin lifecycle; `hermes` stays a reserved runtime id. The hint of a bare
  `integration install` that answers `integration_no_managed_runtime` names
  only the official packages whose hooks the daemon installs (Codex, Claude
  Code), never `hermes`.
- Documented in `guides/hermes-package`, `guides/runtime-packages`,
  `concepts/agent-profiles`, `concepts/sessions`,
  `concepts/runtime-package-archive`, the `runtime-package-recovery` and
  `update-after-release` runbooks, the index, the source map, `README.md`,
  `runtime-packages/README.md`, `compat/hermes/README.md`, `docs/install.md`,
  `docs/architecture.md`, `docs/cli.md`, `docs/public-api.md` and
  `docs/acceptance/README.md`.

## Unreleased (2026-10-13, workflow plugin store and consent install, #148)

- New `pohunek workflow install|list|inspect|enable|disable|uninstall`. A
  workflow plugin is a package archive with a strict `workflow.toml` manifest
  (schema 1, `[plugin]` and `[actions.<name>]` with argv arrays), trusted by the
  digest the owner supplies with `--sha256` and installed only with `--yes`
  after a review of every action's argv.
- The CLI keeps workflow plugins in its own registry under
  `<state>/pohunek/workflow-plugins/` with per-plugin state and config
  directories; the daemon never reads them, and no protocol method, event or
  SDK type was added. Every workflow command is local-only.
- New error codes `workflow_local_only`, `workflow_archive_invalid`,
  `workflow_digest_mismatch`, `workflow_descriptor_invalid`,
  `workflow_plugin_unknown`, `workflow_plugin_installed`,
  `workflow_root_invalid`, `workflow_root_intact`, `workflow_store_busy`,
  `workflow_purge_incomplete`, `workflow_dirs_overlap` and
  `workflow_store_failed`; install without consent returns `consent_required`.
- Changing commands hold `store.lock` in the store root. `uninstall --purge`
  also purges the kept directories of an id without a record, and
  `uninstall --remove-modified` removes installed files that fail
  verification. Config directories live under
  `<config>/pohunek/workflow-plugin-config/`.
- A committed uninstall whose files could not all be deleted, an install whose
  durability is unconfirmed, and a stuck leftover of an earlier purge are
  reported as `warnings` on success; they never decide the result. A purge
  leftover of the plugin being purged keeps the purge failing
  (`workflow_purge_incomplete`). An install whose outcome cannot be read back
  fails with `workflow_install_uncertain` and keeps the plugin directories.
- Running actions, reactions, jobs and services are not yet available.
- Documented in the new `workflow-plugins` guide, the `runtime-packages` guide,
  `safety/trust-model`, the index and the source map, and in
  `docs/architecture.md`, `docs/cli.md`, `docs/public-api.md` and `README.md`.

## Unreleased (2026-10-12, Hermes package in runtime-packages, #147)

- `runtime-packages/hermes/` is the official Hermes runtime package:
  `runtime.toml`, `detect.toml`, the closed operator sidecar `operator.toml`
  and its payload in `operator/`. `compat/hermes/` holds the compatibility
  lock, the PTY goldens and the evidence notes. `cargo xtask hermes
  generate-skill` writes the operator skill and its checksum into the package.
- Releases build `pohunek-runtime-hermes-<version>.tar.zst` like every other
  official package. `compat/matrix.json` has Hermes rows for
  `x86_64-unknown-linux-gnu` and `x86_64-unknown-linux-musl`, so the signed
  catalog authorizes the reserved `hermes` id on those targets and every Linux
  daemon archive ships `runtime/packages/hermes.tar.zst`. The macOS daemon
  archive carries no runtime package (#929). The release-evidence `rows` and
  `smoke` jobs install Python and uv for a provisioner lock, and the archive
  smoke restores every staged runtime from an untouched copy and verifies it
  before each Linux daemon archive.
- The built-in Hermes runtime and the CLI's compiled operator payload stay as
  the source for a host without an installed, enabled and selected official
  package.
- The regenerated operator skill changes the operator payload digest, so an
  existing operator install reports `outdated=true` in `integration status`
  until `pohunek integration update --agent hermes` for its Hermes profile
  rewrites it.
- The Linux CI job `hermes-package` follows the runtime package selection,
  builds its own binaries and the archive with `cargo xtask package build
  --evidence-dir`, matches a verified release baseline digest, runs the
  compatibility gate and the real-Hermes package test, and uploads
  `package-evidence-hermes` and `package-junit-hermes`. The macOS Hermes job
  also runs when the selection did not succeed or selects Hermes.
- Documented in the new `hermes-package` guide, the `hermes-operator` guide
  (regenerated Hermes skill), the `runtime-packages` guide,
  `concepts/release-bundle`, the index and the source map, and in
  `runtime-packages/README.md`, `compat/hermes/README.md`, `docs/install.md`,
  `docs/architecture.md`, `docs/development.md`,
  `docs/runbooks/hermes-operator-plugin.md` and
  `.agents/instructions/verification.md`.

## Unreleased (2026-10-12, background work snapshot in the Claude hook, #890)

- `POHUNEK_INTEGRATION_VERSION` is 14. The Claude `stop` hook forwards each
  background task's sanitized `label` (every type), `program` (`shell` only,
  the command word's basename) and `agent_type` (`subagent` only) in the
  `subagent_snapshot` claim. The command and its arguments are never forwarded;
  the label and program limits come from worker-injected environment variables.
  The `session` hook forwards `session_source` (`startup`, `resume`, `clear`,
  `compact`) in its identity report.
- Documented in the `claude-package` guide. Concept and CLI pages follow with
  the later slices of #890.

## Unreleased (2026-10-12, built-in Claude removed, #146)

- The daemon compiles no Claude in: the official `pohunek.runtime.claude`
  package, installed from the release catalog, is the only source of
  `claude`, as the Codex package is of `codex`. On a host without it, a bare
  `claude` is `agent_profile_not_found`, an unpinned `base = "claude"` host
  profile and `integration.install` for `claude` answer
  `runtime_not_installed`, and each recovery hint names `pohunek plugin
  install runtime/packages/claude.tar.zst --catalog
  runtime/runtime-catalog.json` (the profile hint then names `plugin profile
  migrate <name>`). The built-in runtimes are `shell` and `hermes`.
- A session launched from the built-in Claude of an earlier release (pinned,
  or a v0.33.0 record without a launch binding) answers
  `runtime_builtin_removed` on resume, fork and every interaction (input,
  attach, resize, rename, metadata) while no package serves `claude`; with the
  package installed the live session can be used again and resume and fork
  answer `runtime_served_by_package`. `pohunekd upgrade-preflight` reports
  such a session as `would_lose_recovery` (`runtime_source_not_served`)
  whether or not it could resume.
- The release catalog lists the Claude Code package for the Linux x86_64
  targets only, so a macOS daemon has no source of Claude Code (#929).
- `integration.status`, `integration.doctor` and `integration.uninstall` that
  name `claude` work without the package: the core-owned Claude handler keeps
  the config home of the 0.34 built-in (`CLAUDE_CONFIG_DIR`, else
  `~/.claude`), in the runtime's home and, with `profile` or `all_profiles`,
  in the homes of host profiles over `claude`. Install never resolves through
  it.
- New `configuration/integration_home_ambiguous_owner`: an integration request
  without `agent` that reaches one config home through one handler for
  several runtimes (a package declaring `claude-hook-v1` and Claude's
  `[config_home]` under another runtime id) attributes the home to none of
  them. Each runtime reports the error for it: a bare install fails,
  `all_profiles` lists it in `failed`, and status and doctor report the home
  unresolved. A request naming `agent` addresses only that runtime.
- A foreground `claude` in a shell session and an unmanaged `claude` are
  recognized only through the installed package's detection manifest and
  `[transcripts]` table; without the package they are not observed as Claude
  Code.
- `daemon.doctor` checks embedded manifests only for `shell` and `hermes`.
  `pohunek integration --agent` offers only `hermes` as a built-in name;
  `claude` and `codex` are accepted as runtime ids.
- New `runtime/integration_no_managed_runtime`: a bare `integration install`
  on a host where no served runtime has daemon-managed hooks; its hint names
  the official package install and then `integration install --agent
  <runtime>`. A bare install resolves every selected home before it writes,
  so an ambiguous home refuses the request with no home written. The status
  warning of an unresolved home carries the error's hint; the integration
  doctor reports an ambiguous home as `config_root_invalid` with a remediation
  naming `--agent`, and `daemon.doctor` as a warning.
- `--all-profiles` covers host profiles over a served runtime that do not
  resolve (unpinned over a package-served alias, or pinned to a version that
  is not installed): status, doctor and uninstall reach their own `[env]`
  home, and install lists them in `failed`.
- A shell session hosting an agent whose runtime is not served (a Claude
  identity reported by leftover hooks without the package) stays mutable:
  only the session's own runtime must resolve; a nested agent is checked
  against the runtime-id grammar.
- The `no_capable_agent` hint names the official `codex` or `claude` package
  install from a release archive, a supported `hermes` on the daemon's PATH, or
  `--agent` with a capable host profile.
- Documented in `guides/claude-package`, `guides/runtime-packages`,
  `concepts/agent-profiles`, `concepts/sessions`, the
  `runtime-package-recovery` and `update-after-release` runbooks, the source
  map, `README.md`, `docs/install.md`, `docs/architecture.md`, `docs/cli.md`
  and `docs/public-api.md`.

## Unreleased (2026-10-12, Claude reporter templates, #146)

- The Claude reporter scripts are core-owned templates with the
  `@POHUNEK_AGENT_ID@` and `@POHUNEK_AGENT_NAME@` placeholders, rendered by
  the Claude handler from the runtime id `claude` and the name `Claude` like
  the Codex scripts. The installed bytes are unchanged (recorded in
  `crates/daemon/src/integration/reporter_golden/claude-*.sh`), so installed
  integrations stay current. Documented in the runtime packages guide.

## Unreleased (2026-10-12, subagent restart, Claude helper stop, main-turn snapshot and hook failure trace, #883)

- A `SubagentStart` with a newer sequence for a terminal subagent reopens its
  record: `running` with a new revision, `finished_at_ms` cleared,
  `started_at_ms` kept; the next `SubagentStop` completes it. This tracks
  Claude background agents resumed with `SendMessage` or woken by their own
  background shell. A start for a running subagent is idempotent.
- A subagent record's phase is the phase implied by the highest-sequence
  claim the worker admitted for its id, for every provider. A newer start for
  a running record or a newer stop for a terminal one is ignored but its
  sequence is persisted in the journal, so a delayed older claim cannot reopen
  or complete the record; a repeated Codex per-turn stop on a finished child
  only advances that sequence.
- `identity-subagent-v1` lists `claude` as a provider whose stop is admitted
  only after a recorded start. Claude's prompt-suggestion helper stop after
  every main turn creates no row, changes no count and evicts no history. The
  worker remembers its sequence in an in-memory set of at most 64 stops, drops
  an entry once a start for that id applies, and ignores (`stale_sequence`) a
  delayed start that is not newer than the remembered stop. A Codex stop
  without a start records a completed subagent (#916).
- The worker logs every ignored subagent claim as
  `worker.subagent.claim.ignored` (`duplicate_start`, `stop_without_start`,
  `stale_sequence`, `not_running`; INFO, hook answer `ok: true`) and every
  refused one as `worker.subagent.claim.refused` (`capacity`; WARN, hook
  answer `ok: false`).
- The Claude state hook runs on the main-agent `Stop` (a second managed `Stop`
  entry beside the notify hook) and sends the worker a `subagent_snapshot`
  claim with only the `id`, `type` and `status` of each `background_tasks`
  entry. It reads at most 1 MiB for this action (64 KiB for the others) and
  sends nothing for a `Stop` with a non-empty `agent_id` or a missing, invalid
  or over-256-entry list. `identity-subagent-v1` admits the snapshot.
- The worker reconciles the rows of the same reporting process with an older
  sequence and settles only rows whose end the snapshot proves: a running row
  missing from the snapshot whose newest listing by the reporter falls within
  its run and is older than the snapshot is `completed`, and any other missing
  running row is `completed` only when no task that can host an unlisted agent
  (any type except `shell`, `monitor` and `MCP task`) is listed and the
  reporter's newest snapshot is older than this one and newer than the row,
  because Claude builds the listing before the hook reads its clock. Only that
  newest evidence is kept, so a snapshot older than it, delivered out of
  order, settles nothing from it. A listed row,
  running or terminal, is left unchanged, so a fast agent's own stop completes
  it and a self-woken agent reopens through its own start; a completed row keeps its
  own sequence, so a resume start ordered before the snapshot still reopens
  it. It logs `worker.subagent.reconciled` (`snapshot_absent`) and
  `worker.subagent.snapshot.applied` with the task count `tasks`, the `hosts`
  flag and the row counts `listed`, `terminalized` and `deferred`. A
  never-listed row settles at the earliest at the second main-turn end after
  its start; a row whose reporter never ends another main turn stays
  `running` until the runtime ends (`lost`). Codex has no snapshot (#916).
- The sequence contract names the clock the Codex and Claude hooks read:
  `time.clock_gettime_ns(time.CLOCK_MONOTONIC)`, system-wide, because
  `time.monotonic_ns()` is defined only within one process.
- Once a Codex or Claude state hook has seen `POHUNEK_ENV=1` and a session
  id and started `python3`, it records every reason it fails to deliver a
  report as one line in the hook failure trace `pohunek-hooks.jsonl` in the
  pohunek log directory: `input_too_large`, `input_invalid`, `no_snapshot` (with
  `detail` `subagent_stop`, `tasks_missing`, `tasks_invalid` or
  `tasks_over_bound`), `worker_socket_missing`,
  `worker_instance_missing`, `agent_pid_invalid`, `agent_identity_unavailable`,
  `worker_unreachable`, `worker_timeout`, `worker_closed`, `worker_refused`,
  and `daemon_socket_missing`, `daemon_unreachable`, `daemon_timeout`,
  `daemon_closed`, `daemon_refused` and `daemon_protocol_invalid` for the
  daemon fallback. A line is one JSON object of at most 512 bytes with `ts`,
  `session`, `provider`, `action`, `reason` and an optional fixed `detail`
  code, and never carries payload content, input ids, task data, paths or
  environment values. The worker injects the reserved `POHUNEK_HOOK_TRACE_PATH`
  and `POHUNEK_HOOK_TRACE_MAX_BYTES` (1 MiB); the hook traces only when both
  are valid, writes its lines at exit under an exclusive lock that it tries
  without blocking (at most 5 attempts, 5 ms apart, else the run's lines are
  dropped), opens the file
  with `O_NOFOLLOW` and mode `0600`, writes only to an owner-private regular
  file of the user with one link, and rotates it once into
  `pohunek-hooks.jsonl.1`, so the trace holds at most 2 MiB. Tracing never
  changes the hook's exit code or output, a report delivered on its first
  route writes no line, and a hook without `python3` writes no trace.
- `POHUNEK_INTEGRATION_VERSION` is 13. `SubagentInfo` and the public wire
  protocol are unchanged; the private worker hook protocol carries
  `subagent_snapshot`. Documented in `concepts/sessions`, the `claude-package`
  and `codex-package` guides, the `debug-session-runtime` and
  `update-after-release` runbooks, the `runtime-packages` guide,
  `docs/architecture.md` and
  `docs/public-api.md` (hook schemas, `SessionInfo.subagents`,
  `subagent_state`, Provider Notification Hooks, Hook failure trace,
  Active-Agent Hook Payloads reserved environment, Claude notification hooks,
  integration version).

## Unreleased (2026-10-12, built-in Codex removed, #145)

- The daemon compiles no Codex in: the official `pohunek.runtime.codex`
  package, installed from the release catalog, is the only source of `codex`.
  On a host without it, a bare `codex` stays `agent_profile_not_found`, an
  unpinned `base = "codex"` host profile and `integration.install` for
  `codex` answer `runtime_not_installed`, and each recovery hint names
  `pohunek plugin install runtime/packages/codex.tar.zst --catalog
  runtime/runtime-catalog.json` (the profile hint then names `plugin profile
  migrate <name>`). The `agent_profile_not_found` hint of any other name lists
  `shell` or an installed runtime (`pohunek plugin list`) instead of fixed
  runtime ids.
- New `runtime/runtime_builtin_removed`: resume, fork and every mutation
  (input, attach, resize, rename, metadata) of a session launched from the
  built-in runtime of an official alias this daemon does not compile in
  (pinned, or a record without a launch binding) while no package serves the
  alias. Its hint names the package install and a new session. With the
  package installed, the live session can be used again and resume and fork
  answer `runtime_served_by_package`.
- `pohunekd upgrade-preflight` judges every live session's runtime source with
  the mutation guard, whatever its resume and fork capabilities: a session
  whose runtime resolves nowhere is `would_lose_recovery`
  (`runtime_source_not_served`), and its detail says every interaction is
  refused until the runtime (for a removed built-in, the official package) is
  installed.
- The release catalog lists the Codex package for the Linux x86_64 targets
  only, so a macOS daemon has no source of Codex (#929).
- `integration.status`, `integration.doctor` and `integration.uninstall` that
  name `codex` work without the package: the core-owned Codex handler keeps
  the config home of the earlier built-in, so leftover pohunek hooks are
  reported and removed, in the runtime's home and, with `profile` or
  `all_profiles`, in the homes of host profiles over `codex` (found from each
  profile's `base` and `[env]`, since such a profile does not resolve). Bare
  status and doctor cover only served runtimes.
- A foreground `codex` in a shell session and an unmanaged `codex` are
  recognized only through the installed package's detection manifest and
  `[transcripts]` table; without the package they are not observed as Codex.
- `daemon.doctor` checks embedded manifests only for `shell`, `claude` and
  `hermes`. `pohunek integration --agent` offers `claude` and `hermes` as
  built-in names; `codex` is accepted as a runtime id.
- Documented in `guides/codex-package`, `guides/runtime-packages`,
  `concepts/agent-profiles`, `concepts/sessions`, the
  `runtime-package-recovery` and `update-after-release` runbooks, the source
  map, `docs/install.md`, `docs/architecture.md`, `docs/cli.md` and
  `docs/public-api.md`.

## Unreleased (2026-10-12, missing program before the version probe, #145)

- `session.new`, `session.resume` and `session.fork` resolve the runtime's
  program before any version probe, for every runtime but a bare host shell
  (whose configured shell command its launch resolves), and answer
  `runtime/agent_binary_missing` when it does not resolve to an executable,
  before any session, worker, worktree or recovery write. That refusal's
  message and recover hint name only the program's file name. Only an
  executable that exists but that the probe rejects (another release,
  unparseable output, a failing or timed-out probe, an unknown parser) answers
  `agent_runtime_unsupported`. A missing Hermes executable, and a missing
  executable of a package runtime that declares a probe (the official Codex
  and Claude packages), therefore answer `agent_binary_missing`. Documented in
  `concepts/agent-profiles`, `concepts/sessions`, the
  `debug-session-runtime` runbook and `docs/public-api.md`.

## Unreleased (2026-10-12, Hermes release consumer driver, #147)

- The release consumer suite has a `hermes` driver. It installs the Hermes
  package from the catalog, runs `integration install --agent hermes` into a
  private Hermes home and requires the install and status results to report
  `provenance` `source=package` with the archive's package id and digest, then
  drives a real pinned Hermes through one turn against a loopback provider,
  stops it and resumes it by the conversation id the plugin hook reported. The
  Linux Hermes package CI job (`hermes-package`) runs it in row mode on the
  archive built from `runtime-packages/hermes`, after restoring and verifying a
  pristine stage. Documented
  in `docs/development.md` ("Release consumer suite") and the source map.

## Unreleased (2026-10-12, release baseline in CI package selection, #149)

- The `package-selection` CI job downloads release `v<workspace version>`'s
  runtime catalog, attestations and package archives with `gh release
  download` and runs `cargo xtask package baseline --json`. A pull request
  skips a package job only when the package and the core are unchanged and the
  signed release verifies the package; a failed download makes every package
  missing and runs every job, and only an unreadable checkout fails the
  selector. Each package job requires its rebuilt archive to hash to a verified
  baseline digest; a difference fails the job as a non-reproducible build.
  Release workflows never consult a baseline. Documented in
  `concepts/runtime-package-archive` ("Release baseline") and
  `docs/development.md`.

## Unreleased (2026-10-12, Claude package detection rules, #613)

- The official Claude package is version 1.0.1. Its detection manifest adds to
  the built-in one: the `◐`/`◑` running-turn title and the `esc to interrupt`
  footer read `working`, so a streaming reply reads working; the question
  form reads `blocked` when its footer wraps at 30 and 40 columns; the theme
  picker, the login method list and the folder trust dialog read `blocked`;
  the completed-turn line with ` · done <time>` is the completed-turn rule; the
  npm `claude.exe` process matches, and a `node`/`bun` script that merely names
  `claude` (`claude-companion.mjs`) does not. The built-in manifest is
  unchanged. The package tests enumerate these differences from the v0.34.2
  golden. Documented in the `claude-package` guide and the source map.

## Unreleased (2026-10-12, Pi dialogs, selectors and session-dir overrides, #547)

- The Pi package (version 1.0.2) reads Pi's dialogs as `blocked`: an
  extension's select, confirm, input or editor dialog (also a permission check
  raised mid-turn), the project trust prompt at startup, the missing session
  cwd prompt and every login step. A body that holds a selector list (a cursor
  row and a row indented by two spaces) withholds `idle`,
  so the model, session, tree, fork and settings selectors match no rule; a
  quoted draft (`> text`) stays `idle`. Pi's own confirms drawn like an
  extension dialog (project trust, missing session cwd, "Summarize branch?",
  "Import session"), the `/trust` selector and the login steps read `blocked`
  even when the owner opened them. `scripts/tests/test_pi_detect_regex.py`
  runs the generator in CI and requires `detect.toml` to hold its output. The
  1.0.1 manifest read 93 of 120 captured dialog screens and 36 of 50 selector
  screens as `idle`. Screens of a real Pi 1.0.2 at widths 20 to 200 are in
  `compat/pi/screens/overlays/`.
- A file existence check may declare `store_env` and `store_max_depth` (a
  variable that names the store itself, followed by the check),
  `relocating_args` and `relocating_settings` (store moves the check cannot
  follow). `session.new`, `session.resume` and `session.fork` refuse a launch
  that moves the store out of reach with the new code
  `runtime/agent_native_store_relocated` and a hint naming the variable. Pi
  declares `PI_CODING_AGENT_SESSION_DIR`, `--session-dir` and the `sessionDir`
  setting of the agent directory and the project. A settings path that is not
  a regular file (a FIFO, a directory) is refused without blocking. Recovery
  runs the same refusal, so a `sessionDir` written after launch, or the loss
  of the variable the launch had, refuses resume and fork; there the hint says
  to undo that change (remove the setting, or restore the variable to the path
  the session was launched with and accept the profile change). A crash of the
  check (on recovery the whole check, Claude's transcript check included) is
  `daemon/native_store_check_task_panicked`. The keys are frozen into the
  session's launch spec (store schema 7, a no-op migration from 6).
  Documented in the `pi-package` guide, `concepts/sessions`, the
  `debug-session-runtime` runbook, `docs/public-api.md`, `docs/install.md` and
  the source map.

## Unreleased (2026-10-12, provisioned upstream staging, #147)

- `cargo xtask compat stage-upstream` stages a lock whose `upstream.kind` is
  `provisioner`, a Python virtual environment at `venv/`: it runs the script
  `upstream.provisioner` names (only
  `scripts/<name>`, a regular executable file, never a link) in place at
  `<out>/<runtime>` with a cleared environment, links `bin/<binary>` to the
  entrypoint the script prints, copies the lock and script into `inputs/`,
  requires the entrypoint's first line to be exactly
  `#!<stage>/venv/bin/python` or `#!<stage>/venv/bin/python3` (uv writes the
  latter on macOS),
  writes `STAGE.sha256` and `STAGE.links`, and requires the banner as the first
  line of `--version`. A stage path at which uv would write a `#!/bin/sh`
  trampoline is refused before the script runs. `compat verify-stage` also
  refuses a provisioned stage whose `inputs/` differ from the committed files,
  a trampoline entrypoint, another interpreter and a moved stage. Hermes is staged this way; its
  stage is never moved. Hermes writes into its editable checkout on launch,
  and a daemon-launched session and the CLI's `-I` plugin probes write
  bytecode into it, so a stage is verified from a pristine copy before each
  use. The pinned Hermes CI jobs stage through the command, cache a copy
  taken before any consumer runs, run `verify-stage` on a cache hit, and
  restore and verify that copy again before each later consumer. Documented in `concepts/upstream-staging`,
  `compat/hermes/README.md` and `docs/development.md`.

## Unreleased (2026-10-12, Hermes gate and smoke from the package, #147)

- Every `pohunek integration` result for Hermes reports `provenance`: the
  source the installed plugin's ownership marker records, as
  `{"source":"package","package_id":"pohunek.runtime.hermes","package_digest":"sha256:<hex>"}`
  or `{"source":"embedded"}`, and `null` when no managed plugin is installed
  (also after `uninstall`). The human form prints a `source:` line. Documented
  in the `hermes-operator` guide (regenerated Hermes skill).
- `cargo xtask hermes compatibility` takes `--bin-dir` (the directory of
  `pohunek`, `pohunekd`, and `pohunek-sessiond`) and `--package` (the Hermes
  runtime package archive, built by `scripts/build-hermes-package`) in place of
  `--pohunek-bin`. It copies the binaries into a private prefix with a
  throwaway trust anchor, signs a catalog for exactly that archive, starts the
  copied daemon, installs the package with `pohunek plugin install --catalog`,
  and fails unless install, status, and doctor report the archive digest as
  the provenance (`source=embedded` fails). `cargo xtask hermes
  throwaway-catalog` writes such a catalog and its anchor for the release
  smoke. `packaging/smoke-hermes-plugin-release` ships in the daemon archive
  and takes the extracted daemon archive, the package archive, the catalog, the
  anchor, and the Hermes executable. Documented in the source map.

## Unreleased (2026-10-12, CLI session turn waits, #858)

- `pohunek session wait <target>...` waits for turn events through
  `session.wait_turn` when no legacy flag is given, also with no flags at all:
  `--until` (comma list or repeated, default all of `turn_completed`,
  `agent_blocked`, `approval_required`, `exited`), `--after <cursor>`, and
  `--timeout <duration>` with a unit (`90s`, `5m`; default the daemon default).
  It takes up to 32 targets on one host; targets on different hosts, several
  targets in legacy mode, and legacy flags mixed with turn flags are usage
  errors before any request. `--json` prints the daemon result; the human form
  prints the event or reason, one `progress:` line per target, and a final
  `cursor:` line. Every reason exits 0. The legacy predicate wait is selected by
  any of its flags and is unchanged. Documented in `docs/cli.md`,
  `concepts/sessions`, the `debug-session-runtime` runbook, a manager-agent loop
  in the `agent-skill` guide (regenerated skill artifact), and a note in the
  `hermes-operator` guide that the plugin's wait tool is the short predicate
  wait (regenerated Hermes skill).

## Unreleased (2026-10-12, built-in parity goldens, #145)

- `compat/codex/builtin-v0.34/` and `compat/claude/builtin-v0.34/` hold golden
  copies of the built-in Codex and Claude descriptors and detection manifests
  of release 0.34.2. The package tests compare the official packages with these
  goldens; the 0.34.2 descriptors declare no transcript tree. While the daemon
  embeds the built-ins, the tests also require the built-in manifests to equal
  the goldens byte for byte and the built-in descriptors to be the goldens
  followed by the package's transcript tree. Documented in the `codex-package`
  and `claude-package` guides and the source map.

## Unreleased (2026-10-12, upgrade test through the Claude package, #146)

- The release upgrade test installs the official Claude package from the
  previous release's archive under its signed catalog, pins the fake-agent
  profiles with `plugin profile migrate`, and starts its session from the
  package; it asserts the preflight judges that session `adoptable` and that
  the store keeps the same package pin across adoption and `resume`.
  `scripts/upgrade-test` refuses a previous release without
  `runtime/packages/claude.tar.zst` (older than v0.34.2) or with an expired
  catalog. The `update-after-release` runbook describes moving Claude and Codex
  work onto the official package before upgrading to a release that does not
  serve the built-in runtime.

## Unreleased (2026-10-12, Hermes lock shape, #147)

- `compat/hermes/compatibility-lock.json` has the official-package lock shape:
  `schema`, `package` (`pohunek.runtime.hermes`), `runtime` (`hermes`), an
  `upstream` object of kind `provisioner` that holds the release and its source
  provenance, `supported` (`[0.20.0, 0.20.1)`, equal to the package descriptor's
  version probe range) and `verified_with`; the launch contract is `launch`.
  `cargo xtask compat attest` and `compat verify` read it like the npm locks
  (`compat matrix-check` once `runtime-packages/hermes/` exists), and
  `compat stage-upstream` refuses it for having no `upstream.npm`.
  Documented in `concepts/upstream-staging` and `compat/hermes/README.md`.

## Unreleased (2026-10-12, TypeScript turn waits, #858)

- The TypeScript SDK adds `client.sessionWaitTurn(params)` for
  `session.wait_turn`, on a dedicated connection whose deadline follows
  `timeout_ms` or, when it is omitted, `MAX_SESSION_WAIT_TURN_MS`; generic
  `call("session.wait_turn", ...)` takes the same path. `@pohunek/protocol`
  exports `MAX_SESSION_WAIT_TURN_MS`, `MAX_SESSION_WAIT_TURN_TARGETS`, and
  `MAX_SESSION_TURN_CURSOR_BYTES`. The testkit fixture daemon serves the method
  and adds `scenario.recordTurnEvent`. Documented in the `ts-sdk` guide,
  `concepts/sessions`, `sdk/ts/sdk/README.md`, and `docs/public-api.md`.

## Unreleased (2026-10-12, runtime package builder evidence, #149)

- `cargo xtask package build --evidence-dir <D>` writes the builder evidence of
  an official package next to its archive: `<archive>.sha256`,
  `<rt>.manifest.json`, `<rt>.provenance-input.json` (source commit and Git
  tree, builder, archive entries, the lock's whole `upstream` object of any
  kind) and `<rt>.compat-vector.json` (the package half of a compatibility
  attestation),
  as deterministic JSON. A package tree, lock or matrix that differs from
  `HEAD`, a missing or mismatched lock and an evidence directory inside the
  package are refused with nothing written. The per-PR package jobs upload it
  as `package-evidence-<rt>`, the release build as `package-evidence`; it is
  not a release asset. Documented in `concepts/runtime-package-archive` and
  `concepts/release-bundle`.

## Unreleased (2026-10-12, service.toml turn-wait timings, #858)

- `service.toml` schema 4 adds `[waits]`: `turn_default_ms` (the timeout of a
  `session.wait_turn` call that names none, 300000 on fresh installs) and
  `turn_max_ms` (the ceiling a requested timeout may not exceed, 1800000).
  Both are whole milliseconds in `5000..=3600000`, and a default above the
  ceiling makes the daemon refuse startup. `pohunek service upgrade` migrates
  schema-2 and schema-3 files with the same backup and rollback transaction,
  assigning the defaults of the missing tables and keeping every recorded
  value; `config_schema_migration` reports `2` or `3`. Documented in
  `guides/setup`, the `update-after-release` runbook and `docs/public-api.md`.

## Unreleased (2026-10-12, notification providers by runtime id, #145/#146)

- Notification source ranking follows the hook schemas: a provider ranks as a
  provider hook exactly when a registered hook schema lists it as an identity
  provider (today `shell`, `codex`, `claude`, and `hermes`, matched
  case-sensitively). Hook admission on `notification.create` checks those
  providers against the session's hook schema and additionally admits a
  session's own runtime, which does not change its rank. Hermes and shell hook
  reports therefore rank above user/external reports: inside the debounce
  window a pending Hermes attention report is kept over a later user report
  for the same session, and inside the dedupe window it upgrades a projector
  record and dedupes against an earlier provider-backed record.
- The default notification policy has no per-provider entries; every provider
  uses the base `enabled` flags until an override is set. A policy file the
  daemon already persisted keeps the entries it holds, so on such a host a
  `--provider default` toggle does not reach `codex`, `claude`, or `hermes`
  until their own entries are changed too (`policy set --provider <runtime-id>`).
  The SDK testkit fixture daemon serves the same empty default.
- `notification.create` judges a session hook claim by the provider name the
  record is stored and ranked under, and refuses with `hook_not_admitted` a
  raw provider that only normalizes to a hook provider (for example
  `"hermes\u0007"` or `" codex"`), so such a spelling cannot bypass the
  session's hook schema.
- `pohunek notifications policy set --provider` accepts `default` or any valid
  runtime id, installed or not (for example `pi`); a new override starts from
  the base flags. Documented in `concepts/sessions`, the `debug-daemon`
  runbook, `docs/cli.md`, and
  `docs/public-api.md`.

## Unreleased (2026-10-12, resumable Pi forks, #546)

- A runtime with `[native_reference] strategy = "assigned"` may declare
  `assign_fork = true` (only next to `[fork] supported = true`, otherwise
  `descriptor_invalid`). `session.fork` then generates a fresh reference for the
  fork and appends the rendered `launch_args` after the `[fork]` args; the fork
  holds it with provenance `assigned` from creation, so it resumes into its own
  conversation after a stop or a daemon restart. The official Pi package
  declares it from package version 1.0.1 (`pi --fork <source> --session-id
  <fork>`, verified against Pi 1.0.2, which writes the fork's
  `<time>_<fork>.jsonl` at once); `plugin update` installs 1.0.1 beside 1.0.0. The flag is
  frozen into the session's native launch spec (store schema 6, a no-op
  migration from 5), and `PackageRuntimeInfo` gains `fork_launch_args`, shown by
  `plugin inspect` as the fork's `then adds:` line. Documented in
  `guides/pi-package`, `concepts/sessions`, `concepts/agent-profiles`,
  `docs/install.md`, and `docs/public-api.md`.

## Unreleased (2026-10-12, session turn waits, #858)

- New protocol method `session.wait_turn` (Rust SDK `Client::session_wait_turn`)
  waits for the next `turn_completed`, `agent_blocked`, `approval_required`, or
  `exited` event of one or more sessions after an opaque cursor, bounded by an
  agent-chosen timeout (daemon default 5 minutes, ceiling 30 minutes). Results
  carry the next cursor and per-session progress; reasons are `event`,
  `timeout`, `cursor_reset`, and `targets_ended`. Turn events come only from
  admitted provider hook reports for held sessions and from the daemon
  projector, and are recorded after the attention debounce whether or not the
  notification policy keeps an inbox record; a `turn_completed` whose window
  ends while the session works is dropped as a stale stop. `host.inspect` advertises the method with
  `session_wait_turn_supported`. Documented in `concepts/sessions`, the `debug-session-runtime`
  runbook, and `docs/public-api.md`.

## Unreleased (2026-10-12, notifications watch filters, #858)

- `pohunek notifications watch` accepts `--session <id>`, `--kind <kind>`, and
  `--status <status>` (`--unread` is `--status unread`), with the values of the
  matching `notifications list` flags. The daemon subscription stays global;
  the filters run client-side on the record each `notification_created` or
  `notification_updated` event carries, on every host with `--all-hosts`, in
  human and JSON output. An update that marks an earlier `turn_completed`
  `acknowledged` therefore does not pass `--unread`. While any filter is set,
  `notification_deleted` events are dropped, because they carry only the
  notification id, and `--status deleted` is refused. Without filters every
  event is printed. Documented in the
  `debug-daemon` runbook, the `remote-hosts` guide, `docs/cli.md`, and
  `docs/public-api.md`.

## Unreleased (2026-10-12, sessions pinned to an unserved runtime source, #145/#146)

- A session is observed, resumed and forked only through the runtime source its
  launch pin names. A built-in pin is served only by a built-in of the same
  package identity the registry serves now; an official package that serves the
  alias, a built-in the daemon does not ship, or a package digest that is not
  installed never stands in. Resume and fork of a built-in session of an alias
  an official package serves fail with `runtime_served_by_package`, whose hint
  names `pohunek session new --agent <runtime>` and states that no session is
  rebound. A live session adopted at startup with such a pin is observed with
  the generic shell manifest (or its host profile's manifest override), its
  persisted input framing and its journaled hook schema; hook reports and
  processes naming its runtime are not attributed through the package, and
  stay unattributed until the next restart even when the pinned source is
  served again. A worker of a release before 0.34.0 journals no hook schema, so
  such a session with a journaled active identity is not adopted
  (`worker_identity_unverified` in the preflight, a `conflict` with
  `active_identity_provider_invalid` at startup). A recovery binding whose
  launch spec the migration left unresolved is completed only from the
  definition its pin names and otherwise stays unrepaired with a
  `native_recovery` warning. The upgrade preflight reports such a session
  `would_lose_recovery` with `runtime_source_not_served`. Documented in
  `guides/runtime-packages`, `concepts/runtime-package-archive` and
  `update-after-release`.

## Unreleased (2026-10-12, single worker-instance marker, #493)

- `POHUNEK_WORKER_INSTANCE_ID` is the only worker-instance marker the process
  inspectors, the runtime sweep, the managed Claude/Codex hooks and the Hermes
  reporter read. A live worker whose journal names a release before 0.33.1 (or
  none) marks its children with another name, so the upgrade preflight reports
  it `would_not_be_adopted` with `worker_marker_legacy`, and startup leaves it
  running as `incompatible` with the same reason; `session stop`, input,
  resize and attach on it keep the code `worker_protocol_incompatible` but name
  `worker_marker_legacy` in `msg` with a `recover` hint. A sweep never
  signals a worker child (`POHUNEK_WORKER_ID`, set only by a worker) that
  carries the swept session's `POHUNEK_SESSION_ID` but no worker-instance
  marker; a lifecycle hook the daemon runs for the session has no worker
  marker and is ignored. Such a child keeps the cleanup unconfirmed
  (`runtime_lost_cleanup_unconfirmed`, removal refused) and names such
  processes by pid in the `session.runtime.sweep_unmarked` WARN. The runtime
  inventory lists a refused worker as `incompatible` with the same reason. Installed hooks of the previous release differ from the
  shipped assets and are reported for reinstall. Documented in
  `update-after-release`, `debug-session-runtime` and `concepts/sessions`.

## Unreleased (2026-10-12, runtime-declared transcripts, #487)

- A runtime descriptor may declare `[transcripts]` (`parser`, a compiled parser
  id, and `dir`, a plain relative path below `[config_home]`); the external
  observer takes each transcript root and its parser from it instead of naming
  Claude and Codex. The built-in and official `codex` and `claude` descriptors
  declare `jsonl-head-v1` with `sessions` and `projects`, so the observed roots
  are unchanged; a runtime without the table (Pi) contributes no root, and a
  tree that equals or nests with another runtime's is attributed to neither
  (warning `external.transcript_roots.ambiguous`, code
  `transcripts_ambiguous_owner`). An
  unknown parser, an unsafe `dir` or a table without `[config_home]` is
  `descriptor_invalid`. Documented in the `runtime-packages` guide, the
  `sessions` and `agent-profiles` concepts and the public API.

## Unreleased (2026-10-12, coalition exclusion in the runtime sweep, #821)

- The worker journals its kernel coalitions as the optional `worker_coalitions`
  (`<resource>:<jetsam>`, decimal; additive, no journal schema bump, so a worker
  of the previous release lacks it). On macOS the sweep uses them for a hidden
  process whose fork lineage is open (a launchd creator number, or a creator
  that exited): when the kernel puts the process in a resource and a jetsam
  coalition that both differ from the worker's, it cannot have descended from
  the worker and is ignored, not reported. A launchd job, a system helper such
  as `mdworker_shared` or `biomesyncd`, or another session's agent started after
  the worker therefore no longer leaves a removal or a lost-runtime cleanup
  unconfirmed. Equal, unreadable or missing coalitions prove nothing and keep
  the process a candidate; a lineage descendant whose coalitions differ is
  skipped, never selected. Nothing is signalled that was not before.

## Unreleased (2026-10-12, the sweep waits for unreadable candidates, #821)

- The runtime sweep no longer leaves a cleanup unconfirmed because of a hidden
  process that is only passing: when a pass signals nothing and every remaining
  candidate is an unreadable-marker process, the daemon re-reads the process
  table for up to the sweep grace (`sweep.grace_ms`) before it reports those
  candidates. A process caught between images, or an orphan of a creator that
  has just exited and is about to follow it, no longer blocks a removal or a
  lost-runtime cleanup; a candidate that outlives the grace stays listed and the
  cleanup unconfirmed, exactly as before.
- The wait is shared by every runtime one operation sweeps: the startup
  reconciliation, one background re-check of a session, and one removal or stop
  cleanup each wait at most one sweep grace in total for such candidates, so
  that wait adds at most one grace to startup readiness however many crashed
  runtimes a persistent candidate blocks; the runtimes decided after it stay
  lost with their cleanup unconfirmed.

## Unreleased (2026-10-10, fork lineage decides withheld environments, #795)

- The runtime sweep decides macOS 27 platform binaries by fork lineage instead
  of listing each as an unreadable candidate. The kernel serves a boot-wide
  creation number for every same-user process, withheld environment or not, and
  records the creation number of the creator. That creator number is rewritten
  to launchd's when the process re-executes after being reparented, so a
  launchd creator is ambiguous and never excludes a process. The worker journals
  its own number as the optional `worker_spawn_id` (decimal string, no schema
  bump), and the daemon gives it to every journal-backed sweep only when the
  journal's boot identity is the current boot's. A process the worker reached
  through its creators is reaped like a marked one; a process created before the
  worker, or under an older non-launchd creator, is ignored and not reported;
  every other hidden process (created after the worker by launchd, a re-executed
  orphan, one whose creator exited after the worker started) stays an
  unreadable candidate and the cleanup unconfirmed. After a lost runtime on
  macOS 27 the runtime's own processes are reaped without
  `--accept-unconfirmed-cleanup`; the flag may still be needed for the
  remaining candidates, which the refusal lists by pid and command name, and
  for a runtime whose worker journaled no creation number (started by the
  previous release). Linux keeps its start-time ordering. Documented in the
  `debug-session-runtime` runbook, `concepts/sessions`, `concepts/architecture`
  and `docs/cli.md`.

## Unreleased (2026-10-10, macOS 27 withheld environments, #790)

- On macOS 27 the kernel withholds the environment of restricted Apple platform
  binaries (`/bin/sh`, `/bin/zsh`, `/bin/sleep`). The Darwin inspector now reads
  such a process as unobservable instead of unmarked, so a lost-runtime sweep
  lists every one of them as an unreadable candidate and the cleanup stays
  unconfirmed (`session rm` refuses until they are inspected or
  `--accept-unconfirmed-cleanup` is given). The runbook `debug-session-runtime`
  names the case.

## Unreleased (2026-10-08, digest-bound release evidence, #150)

- The Release workflow is rewired: producers upload workflow artifacts only,
  the reusable `release-build.yml` and `release-evidence.yml` build the
  packages and run the compatibility rows, provenance, catalog signing and the
  archive smoke, and a single `publish` job creates a draft release, uploads
  and verifies the complete inventory and then publishes it. `ci.yml` rehearses
  the same flow on pull requests with a throwaway key.
- `packaging/smoke-archive` runs a finished daemon archive in a network- and
  PID-isolated namespace: it verifies the staged upstreams, installs every
  shipped package from the archive's own signed catalog and anchor, and runs
  the real-runtime consumer suite against the archive's binaries.
- `cargo xtask compat stage-upstream|verify-stage` install and verify the pinned
  upstream runtime releases from `compat/<rt>/npm` lockfiles with an integrity
  per package; the locks gain `integrity`, `scripts` and `version_output`. The
  concept page `upstream-staging` describes the stage layout and its POSIX
  verifier.
- `cargo xtask release assemble|verify-inventory` builds and checks the release
  bundle from the producer artifacts: it recomputes every digest, verifies the
  attestation of each matrix row, signs the schema 2 catalog and rebuilds the
  daemon archives with a `runtime/` directory. `packaging/release-policy.json`
  is the single source of the expected inventory; the concept page
  `release-bundle` describes the layout, the macOS-without-packages rule and
  the inventory the publisher verifies.
- Runtime catalog schema 2: a signed `release` block (version, commit, binary
  sets per target) and, per entry, `runtime_api` and per-platform attestation
  digests; the signing domain separator is v2, schema 1 documents are refused.
  The daemon does not enforce the new fields yet.
- `cargo xtask compat attest|verify|matrix-check`, the canonical binary-set
  digest and `compat/matrix.json` (runtime x target rows, suite version)
  define the compatibility attestation each release row emits; the new concept
  page `release-attestation` describes the tuple and the trust model.
- The release consumer suite (`crates/cli/tests/release_consumer.rs`) runs one
  runtime out of process through the exact release binaries and writes the
  consumer report that `compat attest` consumes.

## Unreleased (2026-10-08, digest-bound release evidence, #150)

- Runtime catalog schema 2: a signed `release` block (version, commit, binary
  sets per target) and, per entry, `runtime_api` and per-platform attestation
  digests; the signing domain separator is v2, schema 1 documents are refused.
  The daemon does not enforce the new fields yet.
- `cargo xtask compat attest|verify|matrix-check`, the canonical binary-set
  digest and `compat/matrix.json` (runtime x target rows, suite version)
  define the compatibility attestation each release row emits; the new concept
  page `release-attestation` describes the tuple and the trust model.

## Unreleased (2026-10-08, digest-bound release evidence, #150)

- Runtime catalog schema 2: a signed `release` block (version, commit, binary
  sets per target) and, per entry, `runtime_api` and per-platform attestation
  digests; the signing domain separator is v2, schema 1 documents are refused.
  The daemon does not enforce the new fields yet.

## Unreleased (2026-10-08, production catalog trust anchor is checked in)

- Decision: the release trust anchor `runtime-catalog-anchor.json` has two
  roots, both valid from 2026-10-08T00:00:00Z: the CI secondary root
  (`e68204fe...cd771`, until 2028-10-08) signs routine release catalogs and the
  offline primary root (`f52cf66e...989fe`, until 2036-10-08) signs only CI-key
  endorsement and CI-revoking catalogs. The CI root is rotated before its
  `not_after` by a primary-endorsed `key_chain` record and a new anchor in the
  next release.
- The finished anchor is checked in at `packaging/runtime-catalog-anchor.json`
  from public key files under `packaging/catalog-trust/`, with a test that
  regenerates it through `catalog anchor` and requires byte equality, so the
  shipped file is reproducible from reviewed public inputs. The runtime catalog
  concept page names the roots, windows and rotation rule (#655).
- Daemon release archives carry the anchor at the archive root beside `pohunekd`;
  CLI and relay archives carry none, and staging a daemon archive without the
  checked-in anchor is refused.
- The installer copies the anchor into the version directory beside the daemon, so
  the `catalog_trust_anchor` check is `ok` after a release install; an invalid
  staged anchor fails the install before anything is published, and a different
  anchor on republish is a version conflict.

## Unreleased (2026-10-08, catalog anchor writes several roots)

- `cargo xtask catalog anchor` takes a repeatable `--root
  <public-key-file>:<not-before>:<not-after>` instead of the single-key
  `--public-key-file/--not-before/--not-after` flags, so one anchor lists the
  offline primary root and the CI secondary root (owner decision on #150), each
  with its own window, plus the revoked key ids. Root count, repeated keys and
  empty windows are refused by the anchor validation with no file written; the
  output is sorted by key id. The runtime catalog concept page states the form
  and the custody roles (#650).

## Unreleased (2026-10-07, stop and remove need no runtime definition)

- `session.stop` and `session.remove` no longer require the session's agent
  runtime to resolve, so the owner can end the worker of and delete a session
  whose runtime is not installed or whose recorded kind is a historical label.
  Resume, fork, input and every other mutation stay refused with
  `runtime_not_installed` or `agent_kind_unsupported`; external sessions stay
  read-only. The public API reference and the agent-profiles concept page state
  the rule.

## Unreleased (2026-10-07, outdated integration hook assets are flagged after an upgrade)

- Decision: after a daemon upgrade the managed hook assets are flagged loudly and
  reinstalled by one documented owner command; the daemon and the installer never
  rewrite files in an agent's config home on their own, because those homes are
  user-owned and an install also edits the provider's registration files.
- Claude and Codex status already compared the full installed script with the
  embedded one, so content drift at an unchanged version marker was reported as
  outdated; the doctor classifies it as `asset_modified`. Hermes compared the
  installed plugin only with the checksums its own ownership marker recorded, so
  an untouched install from an older release passed. Hermes status now reports
  `outdated`, and the Hermes doctor has an `asset_current` check (16 checks).
- A launch (new, resume or fork) whose agent has outdated Claude or Codex hook
  assets carries a session warning of the existing `hook` kind naming the install
  command. Decision: no new `SessionWarningKind`, because warnings are persisted
  in the session record and decoded as a closed enum by released clients and by
  an older daemon reading the store, so a new variant would need a protocol and
  store schema bump with an adapter; `hook` already means a hook that could not
  do its job. The warning is recomputed at each launch, so it stays current. The installer wrapper prints the doctor and
  install commands after a successful install or upgrade, and the update runbook
  has a "Managed hook assets after an upgrade" section.

## Unreleased (2026-10-07, package test daemons use the test-host process view)

- The shared package test harness builds its daemon with
  `SessionRegistry::new_production_with_inspector` and the test-host process
  view, so an unreadable same-user process of a loaded test host no longer makes
  a session removal refuse. Production daemons keep the real host view.

## Unreleased (2026-10-07, Claude hook reporter behind a wrapper shell)

- The managed Claude hook scripts skip a wrapper shell when they pick the process
  that reports: Claude runs hooks as `/bin/sh -c`, and where that is dash the
  shell stays between Claude and the script, so the worker rejected the
  SessionStart report and no native reference was recorded. The Claude package
  guide records the finding.

## Unreleased (2026-10-07, Codex conversation id becomes the native reference)

- The session worker verifies a launch claim from a provider-named direct child
  of the launch process in the provider's hook-helper role (`codex app-server`), so the conversation id Codex reports from its
  `app-server` child becomes the session's native reference and `session resume`
  works. The sessions concept page states which process may report the id, and
  the Codex package guide replaces its known-gap statement with the verified
  behavior.

## Unreleased (2026-10-07, Claude Code runtime package source)

- Added the Claude Code runtime package guide and the package source
  (`runtime-packages/claude`, `compat/claude/compatibility-lock.json`,
  `compat/claude/screens/`): the descriptor and detection manifest equal the
  built-in Claude files, plus a `semver-line-v1` probe for `<release> (Claude
  Code)` from 2.1.289. The package can be served as the official `claude` runtime
  only through a signed catalog, no release catalog or signing key exists yet,
  and the built-in Claude runtime stays in the daemon. The guide records the
  first-run state files of a Claude home, what was verified against a real
  Claude Code 2.1.289 (hooks from the launched process, resume and fork argv,
  subagent hooks) and the gaps found: the working title uses a glyph no rule
  names, narrow question forms and the trust dialog are not classified, and a
  forked session keeps its source's native reference so its own conversation id
  is rejected.
- The source map lists the package source, the lock, the screens, the tests, the
  Messages stub, the shared process guard and the `claude-package` CI job.

## Unreleased (2026-10-06, Codex hook reporter ancestry)

- The real-Codex hook test requires the reporting process to descend from the
  launched process instead of being its direct child, because an npm install
  launches Node, then the native binary, then the app-server. The guide records
  the observed process tree of both installs and which processes the package
  matchers accept.

## Unreleased (2026-10-06, Codex package tests install the package)

- The Codex package tests install the built archive through a signed catalog
  with a throwaway key and trust anchor, so the real-Codex tests launch through
  the installed package and its version probe, and an always-running test drives
  that probe with unsupported and supported banners. The fixture kills every
  process it started (worker, Codex, detached app-server) on success and on
  failure. The guide and the package README state that the package is official
  only through a signed catalog and that no release catalog or key exists.

## Unreleased (2026-10-05, Codex runtime package source)

- Added the Codex runtime package guide and the package source
  (`runtime-packages/codex`, `compat/codex/compatibility-lock.json`,
  `compat/codex/screens/`): the descriptor and detection manifest equal the
  built-in Codex files, plus a `semver-line-v1` probe for `codex-cli 0.160.x`.
  The package can be served as the official `codex` runtime only through a signed
  catalog, and no release catalog or signing key exists yet. The guide
  records what was verified against a real Codex 0.160.0 and two gaps: the real
  folder-trust dialog wording is not matched by `workspace_trust_prompt`, and
  Codex runs its hooks from an app-server process below the launched one, so the
  hook-reported conversation id is not recorded as the native reference.
- The source map lists the package source, the lock, the screens, the tests, the
  Responses stub and the `codex-package` CI job.

## Unreleased (2026-10-06, trust anchor ACL policy)

- The catalog trust anchor and the catalog signing key file are judged on macOS
  extended ACLs as well as mode bits, through the shared
  `pohunek_platform::filesystem::acl_grants_change` (anchor: no allow entry that
  lets another principal change the file) and `acl_grants_access` (signing key:
  deny-only ACLs only). An unreadable ACL fails closed. Documented in the
  runtime catalog concept.

## Unreleased (2026-10-05, reporter templates)

- The Codex reporter scripts are core-owned templates with `@POHUNEK_AGENT_ID@`
  and `@POHUNEK_AGENT_NAME@` placeholders, rendered from the runtime's id and
  display name before staging. Rendering refuses a value outside the safe
  character set. The installed bytes are unchanged, so trust hashes and
  installed integrations are unaffected. Documented in the runtime packages
  guide and the source map.

## Unreleased (2026-10-05, catalog trust anchor)

- The daemon reads its catalog trust anchor from `runtime-catalog-anchor.json`
  beside its executable (public keys, windows, revoked key ids), with an
  integrity policy on the file and its directories. A missing file keeps
  `official_trust_unavailable`; a file or location that cannot be trusted gives
  the new `official_trust_anchor_invalid` and a failing `catalog_trust_anchor`
  doctor check, while the daemon keeps serving local trust. `cargo xtask catalog
  build|sign|verify|anchor|public-key` builds, signs (with an owner-private key
  file named by path) and verifies catalogs and writes anchor files. No key is
  shipped or compiled in; custody and release staging stay with #150. Documented
  in the runtime catalog concept, the runtime packages guide, the public API
  reference and the source map.

## Unreleased (2026-10-05, line version probe)

- Runtime descriptors can declare `version_probe = { parser = "semver-line-v1",
  args, min, below, line }`, where `line` is a literal template
  (`codex-cli {version}`, `{version} (Claude Code)`, `Hermes Agent v{version}
  {annotation}`) the first output line must match, so the official runtimes'
  real `--version` banners are readable by a package descriptor. Pre-release
  suffixes never match. Templates are validated with the package. `semver-v1`
  is unchanged. Documented in the public API reference, the agent profiles
  concept and the source map.

## Unreleased (2026-10-05, official runtime aliases)

- A catalog-authorized official package may serve `codex`, `claude` or `hermes`
  in place of the built-in runtime; local, linked and explicit-digest packages
  and the shell stay refused. Disabling or uninstalling the package returns the
  alias to the built-in. A session launched from the built-in runtime is refused
  resume and fork with the new `runtime/runtime_served_by_package` error while a
  package serves its runtime, keeping its binding. The runtime packages guide,
  runtime catalog and archive concepts and the public API describe it.

## Unreleased (2026-10-05, preflight hook schema)

- The upgrade preflight resolves the hook schema of a worker whose journal
  carries none from the runtime definitions (built-in runtimes and the package
  store, read-only), so a live agent session that reported its active identity
  is adoptable. `worker_identity_unverified` remains only for a runtime whose
  definition cannot be read.

## Unreleased (2026-10-05, config-home id and observer roots)

- `host.inspect` runtime entries may carry `config_home_id`, an opaque keyed
  identifier of the config home a launch of the entry gives its agent (bare
  runtimes that declare `[config_home]` and each profile), so clients group
  profiles by account without learning a path. External observation now watches
  the transcript tree of every distinct config home of the host, re-read on every
  pass, instead of one root per provider taken from the daemon's own environment.
  The agent profiles, sessions and secrets pages and the public API describe it.

## Unreleased (2026-10-05, config homes)

- A runtime descriptor may declare its config home in `[config_home]` (a
  variable name and a home-relative default); the built-in `claude` and `codex`
  declare `CLAUDE_CONFIG_DIR` / `.claude` and `CODEX_HOME` / `.codex`, moved out
  of code. One resolver gives the directory a launch would give the agent: the
  variable from the daemon's base environment overridden by the profile's
  `[env]`, else the default below `HOME`. A value that is not absolute is
  refused, never expanded. The daemon's own process environment no longer steers
  install or status, so they agree with the launched agent.
- `integration.install`, `status`, `doctor` and `uninstall` accept `profile` and
  `all_profiles` (local control socket only), report the `home` of each result,
  run one transaction per distinct home and list a failing home in `failed`.
  `pohunek integration` gains `--profile` and `--all-profiles`. The agent
  profiles page starts the subscription-switching guide, the public API, CLI,
  runtime packages and secrets pages describe the surface.
## Unreleased (2026-10-05, upgrade test)

- CI upgrades a session started on the previous release's published binaries
  to the current build through `pohunek service check` and `service upgrade`
  and asserts the session survives (`scripts/upgrade-test`, the `upgrade`
  workflow, and `release.yml` before any publishable binary is built). The
  update-after-release runbook and the source map describe it.

## Unreleased (2026-10-05, frozen profile revision)

- A session launched from a host profile freezes the profile's keyed revision
  into its resume binding (store schema 3, optional `profile_revision`).
  `session.resume` and `session.fork` resolve the profile once and relaunch only
  while the revision matches: an edited profile or a legacy session fails with
  `agent_profile_changed`, a missing one with `agent_profile_missing`, and an
  unreadable key with `agent_profile_revision_unavailable`. The owner's
  `accept_profile_change` (`--accept-profile-change`) relaunches under the
  current profile and re-freezes it; `session.resume` still accepts a bare
  session id, and the decision is honored on the local control socket only
  (`agent_profile_change_local_only`). The profile environment now travels in a
  redacting `LaunchEnv` carrier. The public API, CLI, architecture, secrets and
  agent profile docs describe the behavior.

## Unreleased (2026-10-05, hook schema gaps)

- A hook schema now declares the subagent sequence rule next to the subagent
  fields, and a schema without the rule admits no subagent record. The daemon
  import applies the schema's ancestry matcher to identities and its field and
  outcome set to subagents. The integration update check derives the operations
  of the staged asset set from the staged registration and the embedded scripts'
  action tables. The public API and the sessions and runtime packages guides
  describe the rules.

## Unreleased (2026-10-05, compatibility-gated selection)

- A package version whose integration (handler id and hook schema id, or none)
  differs from a version of the same package that a live, lost or resumable
  session or a host profile pin still references stays installed but unselected:
  `package.select` is refused with `package_integration_incompatible`,
  `package.install` with `select: true` installs it unselected, and
  `package.list`, `inspect` and `doctor` report `selection_blocked`. The state
  is derived from the registry and the retention scan, clears when the last
  reference is gone, and never selects the package on its own. The runtime
  packages guide and the public API describe the rule, `PackageRuntimeInfo`
  gained `hook_schema`, a profile pin of an incompatible version is refused, a
  launch that raced a select gets `runtime_package_changed`, and the
  integration update path checks the asset set against the schemas of retained
  versions as well.

## Unreleased (2026-10-05, integration handlers)

- Integration install, status, doctor, and uninstall dispatch through the
  compiled handler a runtime definition names (`integration.handler`) instead of
  the runtime id. The runtime packages guide and the public API describe the
  closed handler set, the staged update transaction (stage, compatibility
  check, atomic activation, rollback to the exact prior tree), the
  `integration_update_incompatible` error, and that `hermes-hook-v1` is
  registered with its lifecycle in the CLI. The source map lists the handler
  sources. `pohunek integration --agent` accepts any
  runtime id (built-in names unchanged), and status and doctor recovery commands
  name the runtime that was addressed.
## Unreleased (2026-10-05, upgrade preflight)

- The update runbook describes the read-only upgrade preflight that
  `pohunek service check|upgrade` run with the new binaries: the store dry run,
  the per-session verdicts (`adoptable`, `would_lose_recovery`,
  `would_not_be_adopted`) and their reason codes, what it cannot see, the error
  codes `service_upgrade_sessions_at_risk`, `service_upgrade_store_unusable` and
  `service_upgrade_preflight_failed`, and that `--accept-runtime-loss` is the
  one consent flag shared with the legacy migration path.
- The CLI reference, the setup guide, the macOS runbook, the session-runtime
  runbook and the public API cross-reference it.

## Unreleased (2026-10-05, hook schemas)

- The hook schema is enforced on the public socket (`session.report_agent`,
  `session.release_agent`, `session.report_native_id`, hook-claim
  `notification.create`) as well as on the worker socket.

- Runtime descriptors name a core-owned hook schema next to the integration
  handler (`[integration] handler`, `hook_schema`). The runtime packages guide
  and the public API document the closed schema set (`identity-v1`,
  `identity-subagent-v1`), the install-time refusals, and that a runtime without
  an integration accepts no hook report. The sessions concept describes how the
  schema reaches the worker, is journaled, and validates daemon imports,
  including the re-projection of a worker that journals no schema.

## Unreleased (2026-10-05, Pi detection and probe PATH)

- The Pi package guide describes detection as one bottom-anchored editor frame
  (upper border, draft, lower border, footer) read for both states, with busy
  borders recognised at every terminal width (their closing rule can be one
  column), the busy indicators (`Working`, compaction, retry), the 20-column
  floor for idle, the rule that draft and transcript text never decides, and
  that the working and idle rules cannot both match. It
  also states that the version probe runs under the launch `PATH`, profile
  override included.

## Unreleased (2026-10-05, client-side protocol window)

- The Rust client and the CLI advertise the protocol window `3..=4`
  (`CLIENT_PROTOCOL_VERSIONS` is `SUPPORTED_PROTOCOL_VERSIONS`) and translate
  through the `v3` adapter against a daemon of the previous release, so a
  staggered multi-host upgrade no longer needs one pass for them. A method new
  in the current protocol fails on the client with `daemon/daemon_protocol_too_old`
  (host, daemon protocol, required protocol, upgrade hint); an untranslatable
  payload fails with `daemon/version_translation_failed`. Discovery classifies
  peers for the asking client's range. The TypeScript SDK advertises the same window
  and carries a port of the adapter (`sdk/ts/sdk/src/compat.ts`) with a typed
  `daemonProtocolTooOld` error; its key names and introduced-method list are
  generated from the Rust adapter. The update runbook gains "Mixed releases
  across hosts"; `public-api.md` and the architecture "Protocol window" section
  describe the client side. The source map lists the client fixture test.

## Unreleased (2026-10-04, upgrade window and store schema)

- The update runbook states the upgrade window (release N carries N-1 for live
  workers, the worker journal and public-protocol clients; persisted state
  migrates from any older kept schema), the `metadata.jsonl.pre-schema-<old>`
  backup, and the recovery from a newer-schema refusal. The daemon debug
  runbook points at it, and the source map lists the schema and shape-guard
  sources.
## Unreleased (2026-10-04, conflict watch and stop exit)

- Documented the WARN logged for every `conflict`, `lost`, `reconnecting`, or
  `incompatible` classification, the background re-check of a `conflict` and
  what ends it, adoption of a live worker after an upgrade (a binding without a
  launch spec does not block it), and `session stop` of a `conflict` with its
  journal and job proof and error codes.

## Unreleased (2026-10-04, public protocol window)

- Documented the public protocol window: a daemon accepts protocol `3..=4`
  through shape-only edge adapters while clients advertise only their own
  version, `daemon.health` and `host.inspect` report the negotiated version, and
  the update runbook now says to upgrade daemons before their clients (a new
  client against an N-1 daemon remains #527). The source
  map lists `crates/protocol/src/compat/`.

## Unreleased (2026-10-04, Pi runtime package)

- Added the Pi runtime package guide: package source layout
  (`runtime-packages/pi`, `compat/pi`), explicit-digest install, the assigned
  `--session-id`/`--session`/`--fork` contract, the session-file existence check
  verified against Pi 1.0.2, detection facts, limits, and how CI keeps the
  descriptor, the supported range and the tests in sync.
- The source map lists the package sources, the compatibility lock, the real-Pi
  test and the `pi-package` CI job.

## Unreleased (2026-10-04, data-driven version probe)

- Runtime descriptors can declare `version_probe = { parser = "semver-v1", args,
  min, below }`: the daemon probes the program with the declared argv in the
  sandboxed probe environment and accepts a release in `[min, below)`, so a
  package moves its supported range without a daemon release. Documented in the
  public API reference and the source map.
## Unreleased (2026-10-04, legacy resume binding migration)

- Documented that the store migration maps v0.33.0 flat recovery fields and
  repairs v0.33.1 records that lost their native launch spec, and that an
  unrepairable record logs a WARN, carries a `native_recovery` session warning
  and is never resumed automatically. The public API lists the new warning kind.

## Unreleased (2026-10-04, README and reference pages)

- The README is an agent-focused entry point; the human reference lives in
  `docs/features.md`, `docs/install.md`, `docs/cli.md`, `docs/sdk.md`, and
  `docs/development.md`, which every release archive carries next to the
  README. The source map lists those pages, and the macOS install runbook names
  them among the archive contents.

## Unreleased (2026-10-04, daemon package host)

- Documented the daemon side of runtime packages: the `<state>/plugins` store,
  `PackageSource` loading, explicit reload, pin resolution by digest,
  verification before launch and integration changes, the
  `runtime_incompatible` error and retention-checked uninstall.

## Unreleased (2026-10-04, runtime catalog)

- Added the runtime catalog concept: the signed catalog that authorizes official
  runtime packages, canonical signed bytes, key rotation and revocation,
  anti-rollback, reserved runtime ids, and explicit-digest local trust. The
  source map lists the new `crates/package` files.

## Unreleased (2026-10-04, runtime package archive)

- Added the runtime package archive concept: the canonical deterministic
  `tar.zst` format, the strict reader's rejection rules, the named size limits,
  and `cargo xtask package build|verify`. The source map lists the new
  `crates/package` files.

## Unreleased (2026-10-05, reported references supersede assigned ones)

- A validated report replaces an assigned native reference: the sessions page,
  the public API reference and the runtime packages guide state that an
  `assigned` runtime with an integration handler and hook schema follows
  `/clear` and in-session resume, that the replacement sets the provenance to
  `reported` in the session, its record and its resume binding, that resume and
  fork then use the reported reference without the existence check, and that an
  assigned value never replaces a reported one. Report sequences are ordered
  per transport (worker claim or public report) and per runtime generation, the
  worker's verified launch process (not only the root child) is the process
  whose conversation switches count, and the session store schema is 4. The
  worker journals the launch process's latest reference apart from the claim
  lease, every replacement carries an ordering key, and a recovered session
  keeps the key of its record.

## Unreleased (2026-10-04, assigned native references)

- Runtime definitions declare how core obtains the native session reference in
  a required `[native_reference]` table: `hook`, `assigned` or `none`. The
  public API reference and the sessions page state the corrected rule for
  runtime packages without an integration handler (resume and fork only through
  an assigned reference), the assigned launch template and existence check, the
  `assigned`/`reported` provenance, and the `agent_native_reference_missing`
  error. The agent-profiles page notes that a profile inherits an assigned
  base's assignment. The rule that a later reported reference supersedes an
  assigned one is not part of this change.

## Unreleased (2026-10-03, native launch spec)

- Replaced the profile `[resume] mode`/`ref_kind` keys and the `[fork]` table
  with a typed native-session launch spec: `[resume] reference_kind`, `args`,
  and optional `fork_args`, each argv with one whole-token `{reference}`.
  `fork_args` is accepted only on bases with compiled fork support (`claude`)
  until runtime packages declare their own; `codex` and `hermes` profiles that
  set it are rejected. `resumable = false` now also removes fork, since fork requires resume. The
  agent-profiles page documents the built-in specs, the profile form, and the
  `invalid_profile` rejections; the public API `capabilities` note and the
  sessions fork paragraph follow the frozen spec.

## Unreleased (2026-10-03)

- Documented the macOS release shape: `aarch64-apple-darwin` CLI and daemon
  archives are ad-hoc signed (`signing adhoc` in the `MANIFEST`), not notarized,
  and every release asset, including the `.sha256` checksum files, has a
  GitHub build-provenance attestation
  (`gh attestation verify <file> --repo zajca/pohunek`). The install-on-macos
  runbook covers the Homebrew formula (`zajca/pohunek/pohunek`, with
  `pohunek service install|upgrade|uninstall` around brew operations), the
  `curl -fLO` archive path, and troubleshooting for Gatekeeper on browser
  downloads and the Keychain re-prompt after an upgrade. The source map drops
  the Developer ID keychain and notarization scripts and describes
  `packaging/macos/sign`, `verify-signed --adhoc`, and `package --adhoc-release`.

## Unreleased (2026-09-29)

- Moved the `pohunek attach` reconnect settings from `launcher.conf` to the
  core-owned `<config_dir>/attach.conf` (same keys, same validation, installed
  as a commented template by `pohunek setup config`). Core never reads
  `launcher.conf`; an operator who tuned `attach_reconnect_seconds`,
  `attach_reconnect_interval_seconds` or `attach_reconnect_max_attempts` there
  must copy the keys into `attach.conf`. The attach terminal behavior and the
  reconnect settings are documented in the sessions concept page.
- Removed the rofi/sway launchers from the bundle: the launcher guide and the
  launcher debug runbook are deleted, the launcher scripts leave the source map,
  and `pohunek setup` keeps only `config` and `completions` (a bare `setup` is
  `setup config`). `pohunek doctor` and `daemon.doctor` no longer report
  `bin:rofi`, `bin:swaymsg`, `bin:python3`, `bin:timeout`, `terminal`,
  `launcher_scripts` or `sway_include`; hook interpreter readiness stays in
  `integration.doctor`. The launchers are developed in `zajca/pohunek-work`.
- Documented the upgrade handoff for launcher assets that earlier releases
  installed: the update-after-release runbook lists the exact
  `<data_dir>/bin` scripts and the `<config_home>/sway/config.d/pohunek.conf`
  drop-in to remove (or hand over to the `pohunek-work` setup); core deletes
  nothing. Corrected the `stale-installation` and `config-not-installed` eval
  scenarios to describe only signals `pohunek health` and `prompt_not_found`
  actually produce.
- Removed the native GUI from the bundle: the GUI setup guide, the GUI review
  guide, the GUI attach-template section of the environment guide, the GUI
  provider-token section of the secrets policy, and the macOS GUI app bundle
  runbook parts. The native GUI is developed in `zajca/pohunek-work`; the public
  API reference lists it as an external client and names the core tests that
  cover `host.discover`, `subscribe`, and `worktree.remove`.
- Removed the web control center from the bundle: the web control center guide
  is replaced by the TypeScript SDK guide (package surfaces, WebSocket relay
  transport contract, loopback test relay, runtime paths), and the macOS
  install and update runbooks, the concept pages, the remote-hosts guide, and
  the source map no longer describe the web backend or its archive. The web
  control center is developed in `zajca/pohunek-work`.
- Stated the UI-less core policy in the architecture concept page: core ships no
  user interface, external clients in `zajca/pohunek-work` use public contracts
  only (CLI `--json`, and the public protocol through the Rust crates pinned by
  git tag and the TypeScript SDK release tarballs pinned by URL and integrity),
  move in lockstep with the protocol version, and link core crates that are a
  pinned, not a stable, API. Issue/PR providers live only in `zajca/pohunek-work`.
- Documented the owner backend's optional rotating owner-private log files
  (`POHUNEK_BACKEND_LOG_DIR` and its limits, failure fallback, torn-line
  recovery, repair at open, startup failures recorded in the files).
- Documented that the Bun owner backend resolves the daemon socket with the shared
  runtime-path contract (including the macOS default and the native socket path
  limit) and Node discovery for the dev stack.
- Documented that the `netbird` CLI is resolved by the trusted-executable policy
  (process `PATH`, then the macOS fallback directories) in the process that runs
  it, and that the doctor `netbird_cli` check uses the same lookup. Added the
  macOS host section of the remote-hosts guide (listener binding, sleep and wake,
  troubleshooting).
- Added the macOS install runbook: archives and what each installs, verification
  with `shasum`/`codesign`/`spctl`, daemon install, start/status/stop with the
  launchd label, GUI and web backend, upgrade without lost sessions, rollback,
  uninstall (refused with live sessions, `--stop-sessions` and `--purge`
  explicit), log locations, logout/reboot/sleep semantics, and Gatekeeper
  remediation that never disables it.
- Documented the owner web backend's macOS archive and launchd agent: separate
  client service labelled with the daemon namespace, `plutil`-written property
  list, allowlisted non-secret environment copied from `backend.env`, bounded
  private logs, `install.sh --uninstall` keeping configuration and logs, and the
  shared `packaging/verify-archive` preflight.
- Documented the macOS GUI app bundle (`Pohunek.app`, identifier
  `io.github.zajca.pohunek.gui`), how it finds the installed CLI, and the
  Developer ID signing and notarization pipeline of the release workflow
  (owner-protected `macos-signing` environment, ephemeral keychain, notary API
  key; missing credentials fail the macOS release jobs and nothing unsigned is
  published). macOS is not yet a published platform.
- Documented the native macOS package tooling: the deployment-target file, the
  Mach-O audit (arm64 only, deployment target, system libraries only, no
  build-machine paths), the unsigned development package, and the CI acceptance
  that installs, upgrades with live sessions, refuses bad archives, and
  uninstalls from the extracted archives against real launchd.
- Documented release archive integrity: every archive carries a `MANIFEST`
  (component, version, target, signing state, SHA-256 of each member) and
  `packaging/install-daemon.sh` verifies it, the host OS and architecture, the
  macOS minimum version, and member permissions before it runs any archive
  binary or changes anything; archives are deterministic (`packaging/archive`).
- Documented the reserved delegated task error contract: the `task_*`,
  `worktree_*` and `check_*` codes, their fixed text, `worktree_in_use` shared by
  `session.remove` and `worktree.remove`, and `task.*` still answering
  `method_not_found`.
- Documented the executable search-path policy: the `service.toml`
  `[environment] search_path` key, the `service_search_path_unavailable` error
  code, and the environment-resolution guide.
- Documented macOS platform diagnostics and setup: the platform-specific
  `pohunek doctor` and `daemon.doctor` check lists (Linux-only launcher probes
  omitted on macOS; `runtime_dir_private`, `socket_path_length`,
  `filesystem_access` with the Privacy & Security explanation,
  `worker_executable`, `launchd_domain`, `launchd_job`, `login_shell`,
  `terminal`, `desktop_notifications`, `keychain`), executable resolution that
  requires an execute bit, and `pohunek setup` skipping the sway/rofi steps on
  macOS with a successful, typed `skipped` outcome.
- Documented transcript indexing for external observation: a bounded
  reconciliation pass every 30 seconds (and on lost-event hints) converges the
  index independently of the inotify or FSEvents watcher, which only reduces
  latency; the per-pass scan bounds; and the `external_transcript_watcher_unavailable`
  and `external_transcript_watcher_degraded` causes.
- Documented `pohunek integration doctor` and `pohunek integration uninstall` for
  Codex and Claude (`integration.doctor` and `integration.uninstall`): stable
  findings with remediation, the macOS `python3` stub check that never runs the
  stub, the socket path limit check, and marker-owned, rollback-protected removal.
- Documented the Claude and Codex integration installer transaction: a
  per-config-directory lock file, rollback of committed files on failure, the
  distinct `integration_install_in_progress`, `integration_destination_collision`,
  and `integration_recovery_required` errors, an absent agent config directory
  reported as informational `not_installed` with recovery `none`, and the macOS
  remediation for symlinked config roots and Darwin socket path limits.
- Documented that Hermes hook identity reports use the platform's process start
  identity on Linux and macOS, matching the daemon's derivation.
- Documented that the managed agent hook scripts run Python in isolated mode, so
  modules in the agent's working directory and `PYTHON*` variables cannot affect
  the hook interpreter's configuration or module resolution.

## Unreleased (2026-09-28)

- Documented that a `session.remove` refused with `runtime_supervision_ambiguous`
  because same-user processes with unreadable environments may belong to the
  runtime now lists them (pid, start identity, command name; at most eight,
  then a count) in the error message and carries a `recover` hint.
- Documented `pohunek session rm --accept-unconfirmed-cleanup`, which calls the
  new `session.remove_accepting_unconfirmed` method (params `SessionId`, result
  `SessionRemoveResult`): per-call consent to remove a session although only
  unreadable-environment processes keep its marker sweep unconfirmed. Plain
  `session.remove` keeps refusing. The accepted processes (at most 64) are never
  signalled and are reported as `accepted_unconfirmed_processes`; reconciliation
  and the retention sweep never consent, and every other unconfirmed reason
  still refuses.

- Documented `pohunek service lock -- <command>`, which runs a command under
  the service transaction lock and lets its `pohunek service` calls adopt
  the lock with the holder token in `POHUNEK_SERVICE_LOCK_TOKEN`
  (`service_inherited_lock_invalid` for a token that proves no live holder),
  and `pohunek service check`, which runs the install or upgrade preflight,
  including the stale daemon-job check, without changing anything. Adopting
  commands hold `service-install.lock.adopted` shared, the holder waits for
  them before it releases the lock, adopted transactions are serialized by
  `service-install.lock.inherited`, and the command runs in its own process
  group (in the terminal's foreground when `service lock` owns it) so each
  signal reaches it exactly once; `Ctrl-Z` stops and `fg` resumes the lock
  and its command together, like a job.
- Documented that `packaging/install-daemon.sh` runs its legacy retirement and
  final install or upgrade under that lock and runs `service check` before it
  touches the legacy install.

## Unreleased (2026-09-27)

- Documented that a failed `session new` whose worktree compensation cannot
  finish stays listed as `reconnecting` with `create_compensation_pending`
  and is retried by the running daemon, and that `session new` and `session
  fork` fail with `migration_manifest_missing` while unimported legacy resume
  bindings exist.
- Documented that a session becomes `lost` only after its ended job is retired
  (otherwise `reconnecting`, `runtime_supervision_unavailable`, retried), and
  that reconciliation finishes an interrupted removal through the same
  retirement and cleanup as `session rm`, keeping the session listed and
  retried until they succeed.
- Documented that `session rm` of a `conflict`, `reconnecting`, or
  `incompatible` session retires the worker job of its recorded generation
  through the service manager before deleting the record, refuses a record that
  names no generation with its runtime code, and keeps the record with
  `runtime_supervision_unavailable` when retirement fails.
- Documented that `service uninstall` clears the transaction record before
  `service.toml`, uninstalls a registered pending install whose `service.toml`
  is gone, and refuses with `service_outdated_journals` while a journal of an
  earlier schema names a worker that may still run; such journals also keep
  every version directory during an upgrade.
- Documented the worker session ID grammar: `s-<ULID>` or `s-<digits>` with 1
  to 20 digits.

## Unreleased (2026-09-25)

- Documented that `migration preflight` takes an explicit `--socket <path>` so
  the archive installer can dial the legacy daemon after renaming its control
  socket aside as a connect barrier, retire it in a fail-closed order
  (barrier, preflight, worker inventory, stop, post-stop re-inventory), and
  abort without removing legacy files when a not-inactive template worker
  survives.

## Unreleased (2026-08-12)

- Documented that a Darwin process between images no longer fails a process
  table read: it is listed without a command line, its markers are
  unobservable, and the runtime sweep skips it as unreadable.
- Documented that cwd evidence is ordered by observation time: a procwatch read
  taken before an OSC 7 hint arrived no longer moves the session back.
- Documented native worker supervision (#100): every worker generation is its
  own systemd transient unit (`pohunek-<ns>-worker-<session-id>-<generation>.service`
  in `pohunek-<ns>-sessions.slice`, systemd 255 or newer) or launchd job
  (`io.github.zajca.pohunek.<ns>.worker.<session-id>.<generation>`, private
  definition, no `KeepAlive`); the `pohunek-session@.service` template and the
  `packaging/systemd` files are gone. Added `pohunek service
  install|upgrade|uninstall|status`, the versioned
  `<prefix>/libexec/pohunek/<version>/` layout, `service.toml`, the install
  journal, and `pohunek daemon start --dev-subprocess`; `pohunek daemon start`
  now requires an installed service.
- Documented the lifetime scope: terminal close and screen lock are safe;
  logout and reboot end workers, which the next login reports as `lost` with
  `runtime_lost` and never restarts. Added the reconciliation reasons
  `runtime_supervision_unavailable`, `runtime_supervision_ambiguous`,
  `runtime_identity_mismatch`, `runtime_lost_cleanup_unconfirmed`,
  `worker_job_absent`, and `stale_worker_generation`, and the
  manual macOS acceptance procedure in `docs/acceptance/`.
- Documented that agents no longer inherit the daemon's whole environment:
  the worker builds the child environment from an empty base plus the
  allowlisted `BaseEnv` (`[environment] allowlist` in `service.toml`), `TERM`,
  the profile environment, and its `POHUNEK_*` identity, and always strips
  service-manager variables.
- Documented portable PTY readiness in the session worker: one `poll(2)`
  implementation over the PTY master and a cancellation self-pipe on Linux and
  Darwin, with the worker's complete test suite running on native macOS CI.
  Darwin kernel peer identity is recorded as in place; launchd, clients,
  packaging, and native acceptance remain deferred through #100-#105.
- Documented that both process backends keep reporting an exited but unreaped
  process with its identity, marked as no longer running.
- Documented automatic session retention: the conservative opt-in default
  policy, the separate terminal and lost TTLs, `session policy get`/`set`,
  `session retention sweep --dry-run`/`--apply`, and the eligibility rules that
  keep external, conflicting, incompatible and in-TTL sessions untouched.
- Documented the sweep's unsaved-work hold and its truthful counters: a matched
  session whose owned worktree has uncommitted, untracked or unpushed work — or
  a state git cannot report — is held rather than removed and carries a `hold`
  reason, `worktrees_cleaned` counts only checkouts confirmed gone with a
  surviving checkout reported as `worktrees_failed`, and
  `session retention sweep` exits non-zero on a partial failure.
- Documented the native Darwin process backend and the single shared host
  inspector: `libproc` process records, `sysctl` `KERN_PROCARGS2` argument and
  environment regions parsed separately, bounded allocations and traversal,
  checked kernel start-identity encoding interpreted identically by daemon and
  worker, controlling-terminal foreground groups resolved without assuming the
  group equals the root process, and an event-driven kqueue `NOTE_EXIT` exit
  watch that re-verifies process identity after registration. Complete macOS
  host, WebUI, packaging, and release support remains deferred through #98-#105.
- Documented the Darwin process-inspection privilege boundary: ownership decided
  by the unprivileged short process record, another user's process reported as
  absent while a refused fact about an owned process stays a denial, and the
  three facts macOS withholds without a privilege pohunek does not request
  (a foreign process's working directory and argument vector, and a
  code-signing-restricted process's environment). An argument vector an `exec`
  or an exit withholds leaves that one process without a command line instead of
  failing the inventory it appears in.
- Documented the implemented provider-neutral relay account linking: identity is
  exactly issuer plus immutable subject, provider profile attributes never link
  accounts, both the current relay actor and the new identity prove themselves in
  one audited transaction completable only through its own browser or device
  channel, and a link or unlink takes effect on current credentials and sessions
  immediately. Recorded the HTTPS start/poll/status/cancel/unlink surface with no
  CLI subcommand, the durable PostgreSQL guarantees, and the user-facing safety
  rules. Kept host links, routing, attach, team administration, provider
  verification, and team clients explicitly deferred.
- Documented the shared secure runtime-path and portable filesystem foundation:
  preserved XDG durable locations, the owner-private macOS runtime default,
  fail-closed path validation, byte-bounded Unix sockets, atomic no-replace
  installation, and explicit uncertain-durability handling. Complete macOS
  host, WebUI, packaging, and release support remains deferred through #97-#105.
- Added the bundled agent-skill guide: safe agent CLI operation covering
  discovery, explicit targeting, JSON state reads, event subscriptions,
  send-and-wait flows, worktree diff inspection, destructive-op caution, and
  owner-first approvals. The guide is the hand-authored source for the
  checked-in CLI skill artifact guarded by xtask drift checks. The
  `pohunek agent-skill` command prints that artifact verbatim, or with
  `--json` as one envelope carrying the skill text and its `content_sha256`;
  the command is fully local and the global `--host` flag is accepted and
  ignored.
- Documented the accepted complete macOS host/client contract and GitHub-owned
  #94-#105 delivery tracking through the issue hierarchy, milestone, and
  Project. Recorded the shipped shared-platform/Linux migration and native
  Darwin contract CI while keeping full macOS support explicitly unshipped
  until native release acceptance closes.
- Documented the implemented reduced relay foundation: PostgreSQL fencing and
  recovery, protected initial Owner/service-account provisioning, generic OIDC
  browser and device authentication, bounded HTTPS account and credential
  lifecycle, and the native HTTPS/keyring credential CLI. Kept host links,
  routing, attach, account linking, complete team administration, provider
  verification, and team clients explicitly deferred.
- Documented shipped host-local stable identity and safe governance inspection:
  explicit never-enrolled state, exact principal-or-team owner, checked
  revisions, quarantine, owner-private persistence, doctor checks, CLI, SDK,
  GUI, and transparent owner-WebUI boundaries. Kept relay, team, enrollment,
  transfer, and share APIs explicitly unshipped.
- Added the accepted optional team-relay concept and separated its future
  protocol-v4, multi-team, `HostShare`, and trusted-relay boundaries from the
  currently shipped owner-only protocol v3 and transparent Bun web backend.
- Documented the simplified session-first native GUI: prioritized cross-host
  groups, modal session detail, direct lifecycle actions, and removal of the
  Agents monitor, provider browsers, review, and worktree-management surfaces.
- Documented standard Tab/Shift+Tab form focus, Enter/Ctrl+Enter submission,
  modal focus containment, and mouse selection/copy for native GUI detail text.
- Documented source-locked, credential-free Hermes compatibility CI and
  extracted release-archive plugin smoke verification.
- Documented configurable Hermes plugin timeout, output, screen, and concurrency
  bounds plus protocol-range repair during integration update.
- Added the Hermes operator guide: explicit managed runtime selection, isolated
  plugin lifecycle, access and host policy, complete typed tool surface,
  origin-session protection, bounded control-loop recovery, and payload-free
  lifecycle reporting.
- Clarified that native GUI releases target glibc x86_64 because Wayland and
  graphics libraries remain dynamic runtime dependencies; MUSL archives remain
  available for the CLI and daemon.
- Documented durable per-session PTY workers, daemon restart reconciliation,
  runtime state and events, explicit native recovery, systemd diagnostics, and
  the one-time legacy migration boundary.
- Documented browser session lifecycle and metadata management, the host-scoped
  Projects screen, and daemon-authoritative worktree removal safeguards.
- Documented the M1 web control center, browser-safe TypeScript SDK entry,
  backend deployment boundary, and fixture-backed development stack.
- Documented the self-contained Linux x86_64 web release archive and its
  systemd user-service installation flow.
- Documented that the native GUI `Start session` and Review "Dispatch as
  session…" agent pickers use the selected host's launchable runtime inventory,
  including resolvable profiles. The GUI fails closed when that inventory is
  unavailable instead of deriving launch permission from the name-only
  `supported_agents` field or falling back to built-in base kinds.
- Cross-linked agent profiles from the GUI Session Launch section.
- Documented fail-closed daemon shutdown when an overlay listener supervisor
  exits or panics unexpectedly after readiness.

## 0.18.3

- Documented native GUI keyboard list navigation and configurable
  `[keybindings]` overrides in `gui.toml`, including default binding names,
  key-string syntax, and same-context conflict validation.
- Linked GUI keyboard/config/reducer/provider source files from the assistant
  source map.

## 0.15.3

- Documented session notification dedupe/debounce behavior for
  `turn:<session_id>`: turn-completed creates share the debounce window,
  resolve on session resume, supersede older unread turn rows, and are collapsed
  by visible attention records.

## 0.15.2

- Documented the attention notification debounce window
  (`attention_debounce_secs`), the deferred-create behavior for
  `agent_blocked`/`approval_required` (`created: true` with a minted id, held
  pending until it flushes or is suppressed by an in-window resolve), and its
  relationship to `attention_dedupe_window_secs` in the sessions concept, the
  debug-daemon runbook, and the public API reference.

## 0.14.5

- Documented the durable cross-host notification Inbox, CLI/API surface,
  provider hook requirements, source-priority dedupe, retention, and trust-model
  boundary.

## 0.7.4

- Documented that the native GUI `Start session` runtime picker supports the
  built-in `shell`, `codex`, and `claude` choices.

## 0.7.3

- Documented that `pohunek-gui` is Wayland-only and fails fast when
  `WAYLAND_DISPLAY` is missing or empty.

## 0.7.2

- Documented explicit `session.resume` and the GUI "Open in terminal" behavior
  for terminal sessions with native resume metadata.

## 0.7.1

- Documented that release archives include the native `pohunek-gui` binary and
  linked release packaging files from the assistant source map.

## 0.7.0

- Added native GUI provider-integration guidance for Linear and GitHub, including
  `gui.toml` provider configuration, keyring and `gh` boundaries, request
  staleness handling, provider launch flow, `link.*` metadata, and PR status
  degradation rules.
- Linked the new GUI provider implementation and provider test files from the
  assistant source map.

## 0.5.0

- Added GUI setup guidance for `pohunek-gui`, including `gui.toml`, external
  attach delegation, Wayland startup diagnostics, project/worktree method
  boundaries, and secret-handling rules.
- Added native GUI prompt-management guidance covering host-resolved
  prompt/action browse, preview rendering through `crates/prompt`, and
  `session.new input` launch behavior.
- Linked GUI setup from the knowledge index and source map.

## 0.3.3

- Added the Phase 1 hand-authored knowledge skeleton for the Universal Pohunek
  Assistant.
- Reserved `index.md` for navigation and `log.md` for bundle history.
- Kept generated reference content out of the committed tree.

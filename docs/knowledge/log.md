# Knowledge Bundle Log

## Unreleased (2026-10-05, Pi detection and probe PATH)

- The Pi package guide describes detection as one bottom-anchored editor frame
  (upper border, draft, lower border, footer) read for both states, lists the
  busy indicators (`Working`, compaction, retry), says draft and transcript text
  never decides, and states that the version probe runs under the launch `PATH`, profile override
  included.

## Unreleased (2026-10-04, upgrade window and store schema)

- The update runbook states the upgrade window (release N carries N-1 for live
  workers, the worker journal and public-protocol clients; persisted state
  migrates from any older kept schema), the `metadata.jsonl.pre-schema-<old>`
  backup, and the recovery from a newer-schema refusal. The daemon debug
  runbook points at it, and the source map lists the schema and shape-guard
  sources.

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

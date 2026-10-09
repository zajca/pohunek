# Knowledge Bundle Log

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

---
type: Runbook
id: runbook/update-after-release
title: Update after release
description: Reconcile setup assets, host capabilities, projects, and attach config after updating Pohunek.
source_kind: manual
intents: [update, setup, debug, help]
since: 0.3.3
---

# Update After Release

Use this runbook after replacing an installed Pohunek binary from a component
release archive or rebuilding it from source.

## Current public protocol v4 boundary

A daemon of the current release accepts public protocol `3..=4`: clients of the
previous release (protocol 3) keep working against it, and a range outside that
returns `daemon/version_mismatch` before any method runs. The CLI and Rust client
of the current release speak `3..=4`, so they also work against a daemon that only
speaks v3, and so do TypeScript SDK clients (see "Mixed releases across hosts"
below). A client and a daemon more than one release apart are refused.
Before replacing any component, inventory every CLI, SDK, custom client, managed
hook and local or overlay-reachable daemon that must talk to another peer. Drain
cross-host automation, upgrade the daemons first, and then verify every host with
`pohunek health --json` and `pohunek host inspect <host> --json`. The reported
`protocol_version` is the version negotiated for that connection (`4` for a
current CLI).

Protocol v4 spells the worker instance identifier `worker_instance_id` where
v3 spelled it `runtime_id`. The daemon translates that spelling for protocol 3
clients only; a v4 client or hook never sends it. Do not downgrade one peer
independently: a v3 client is served by a v4 daemon, a v4 CLI or
SDK client works against a v3 daemon. Managed Codex and Claude hook assets carry
`POHUNEK_INTEGRATION_VERSION=15`. `pohunek service upgrade` reinstalls the
ones it lists and reports each result; follow "Managed hook assets after an
upgrade" below for every other install. An older hook still sends the old
key, which the daemon accepts only inside a session whose launch baked protocol
3; in any other session its native-identity reports are rejected. Hooks of a
session launched before the upgrade keep reporting with the protocol version
baked into its launch environment while the daemon serves that version. State
hooks older than version 11 sent an integer `seq` that the daemon rejects, so
their public-socket reports were dropped, and always spelled the native-id worker
key in the protocol 4 form. Notification hooks older than version 10 send a bare
integer `v` instead of the `{minimum, maximum}` range, so the daemon drops their
notifications until they are reinstalled. Claude assets older than version 12
register no state hook on `Stop`, so they send no main-turn subagent snapshot
and stale Claude subagent rows are not reconciled until they are reinstalled; a
session worker started by a release without the snapshot claim refuses it, so
those sessions keep their rows until their own stops arrive. Codex and Claude
state hooks older than version 13 write no hook failure trace
(`pohunek-hooks.jsonl`), state hooks older than version 14 send their Stop
snapshot without the `label`, `program` and `agent_type` fields, Claude state hooks
older than version 15 register nothing for `UserPromptSubmit`, so a waited input
gets no prompt acceptance and ends in `session_input_timeout` unless activity
answers it (reinstall the hooks of every Claude and Codex home: the version
marker is shared, so `integration status` reports all of them `outdated` until
then), and a session worker started by a release without the
trace sets neither `POHUNEK_HOOK_TRACE_PATH` nor `POHUNEK_HOOK_TRACE_MAX_BYTES`,
so its hooks write no trace either until the session gets a new worker.
Earlier protocol transitions (integer-v1 to range negotiation,
the v3 overlay-routing change) do not widen the supported range.

## Managed hook assets after an upgrade

The managed hook scripts and registrations in each agent's config home belong to
the owner. `pohunek service upgrade` reinstalls the hooks of the homes it lists
once the new daemon is ready:

- The list holds one entry per config home that already holds daemon-installed
  hooks, host-profile homes included. `pohunek service check` and `pohunek
  service upgrade` print one `plan integration <agent> reinstall after the
  upgrade` line per entry, so the check previews the homes the upgrade will
  reinstall.
- The Hermes homes that carry the pohunek marker are reinstalled too. A Hermes
  home without the marker is reported as a `gap` with the command to run; the
  upgrade never writes into it.
- The upgrade report lists each result as `installed`, `failed` or `gap`. A
  failure is reported per home and never rolls the upgrade back.

The installer wrapper (when it does not go through `service upgrade`) and daemon
start do not rewrite hooks. A release can change them without changing
`POHUNEK_INTEGRATION_VERSION`, so after every upgrade compare the installed
assets with the ones the new daemon embeds. This finds the homes `service
upgrade` reported as a `gap` or `failed`, homes that held no hooks before the
upgrade, and installs that bypassed `service upgrade`:

1. Run `pohunek integration doctor` on the daemon host (add `--profile <name>`
   or `--all-profiles` for a host profile's own config home). It exits non-zero
   when any asset is outdated. An asset is outdated when its version marker
   differs from the expected one and also when its bytes differ from the
   embedded script at the same version, reported as `asset_modified` with the
   install command as remediation. Without `--agent` it covers only the
   runtimes the daemon serves, so run it with `--agent claude` or `--agent
   codex` on a host where that package is not installed.
2. Reinstall each agent it names: `pohunek integration install --agent claude`
   and `--agent codex` (with the same `--profile` or `--all-profiles` selector).
   The install replaces the managed files atomically and keeps unrelated user
   hooks.
3. For Hermes, run `pohunek integration doctor --agent hermes --hermes-profile work`
   (or `--hermes-home <absolute path>`; use the target you installed with). The
   `asset_current` check fails when the installed plugin was written by an older
   release even though no file was edited; `integration status` shows
   `outdated=true`. Run `pohunek integration update --agent hermes --hermes-profile work`
   with the same target; it needs `--confirm-modified` only when `modified=true`.
   A v0.33.0 Hermes plugin with a `3..=3` policy can continue using its tools
   through the current CLI's `3..=4` protocol window, including its old
   `--runtime-id` flag and timed-out start recovery. The update refreshes the
   plugin assets and policy to the current shape; it is not required solely to
   restore tool access during that window. A policy outside the window is
   rejected as `pohunek_cli_incompatible` and needs an explicit update. An
   upgrade never rewrites a user-modified Hermes plugin or policy.

The installer wrapper prints these commands after a successful install or
upgrade. A new, resumed or forked session whose agent has outdated Claude or
Codex hook assets carries a session warning of kind `hook` (shown by
`session new`, `session list` and `session inspect`) that names the exact install
command; the session still starts. Outside `service upgrade`, files in a user-owned config home change only by an
explicit owner command, never as a side effect of an installer run, a daemon
start or a launch.

## Mixed releases across hosts

During a staggered multi-host upgrade a CLI of the current release may talk to a
daemon of the previous release on another host over an overlay. Nothing has to be
upgraded in one pass for that pair. The CLI and Rust client advertise the protocol
window `3..=4`; the older daemon answers in protocol 3 and the client translates
requests, results and subscription events through the same shape-only adapter the
daemon uses. The CLI's `--json` output uses the current shape plus the old
runtime-identity spelling at typed session sites while v3 remains supported.
The command
`pohunek host inspect <host> --json` reports `protocol_version` `3` for such a host, `pohunek host list`
shows it as `reachable`, and multi-host commands include it.

A method added in the current release does not exist on the older daemon. The
client refuses it before sending anything, with
`daemon/daemon_protocol_too_old`, whose message reads "host '<host>' runs
protocol 3, but package.list needs protocol 4". The `recover` hint names the fix:
upgrade the pohunek install on that host, then retry. Today the `package.*` methods are the only
ones in this class; the rest of the public methods work across the pair.
A payload the adapter cannot translate fails with
`daemon/version_translation_failed` instead of being passed through; upgrade the
host the same way.

The TypeScript SDK (and so the web and desktop UIs built on it) behaves the same
way: it advertises protocol `3..=4`, translates a protocol 3 daemon's payloads,
and refuses a method new in the current release with the same typed
`daemon_protocol_too_old` error (`ClientError.kind === "daemonProtocolTooOld"`,
with `tooOld.host`, `tooOld.daemonVersion` and `tooOld.requiredVersion`). Only a
peer two releases apart still has to be upgraded: the window is exactly one
release wide and the client is refused with `daemon/version_mismatch`.

## Upgrade window and the metadata store

The contract is that release N stays compatible with release N-1 for live
workers (private worker protocol versions `PREVIOUS_VERSION` and
`CURRENT_VERSION`), for the worker journal, and for public-protocol clients;
nothing else is shimmed. The current state differs for the last two. The worker
journal reader accepts an explicit list of schemas, which today holds only the
current one. The public protocol window is `3..=4`: protocol 3 clients and tooling keep
working against a daemon of this release, and the CLI and Rust client of this
release work against a protocol 3 daemon, as do TypeScript SDK clients
([#527](https://github.com/zajca/pohunek/issues/527)); a peer two releases
apart is refused. The
daemon's persisted state, `<data_dir>/metadata.jsonl`, does migrate from any
older kept schema, so skipping releases is safe for it. Each release's notes name every schema or
protocol constant that changed (`STORE_SCHEMA_VERSION`, `PROTOCOL_VERSION`,
`MIN_PROTOCOL_VERSION`, the worker protocol `CURRENT_VERSION` and
`PREVIOUS_VERSION`, `WORKER_JOURNAL_SCHEMA_VERSION`,
`EXPECTED_INTEGRATION_VERSION`); read those lines before updating.

A session launched from a host profile by a previous release that did not
freeze the profile revision has none recorded, so a plain `pohunek session resume <id>` of it
fails with `agent_profile_changed` ("no recorded revision"). Check the profile
first, then resume it with `pohunek session resume <id> --accept-profile-change`:
the relaunch uses the current profile and freezes its revision, and later plain
resumes work. The local daemon honors the flag; a remote host refuses it. The
upgrade's own profile pins are not such a change: a built-in session whose
profile differs only by the `package` and `digest` keys the upgrade writes
resumes with a plain `resume`, and a refusal of that resume is a defect to report.

CI proves the window on every change to the daemon, worker, protocol, or CLI
crates and on every release tag: the `upgrade` workflow stages the previous
release's published binaries, whose host profiles run the built-in Claude and
Codex runtimes with the release's managed hooks. It starts a stopped and a live
Claude session, then runs `pohunek service check` and one
`pohunek service upgrade` of the new build without `--accept-runtime-loss`. The
step installs the release's official packages, pins every host profile to them,
and reinstalls the hooks. The test asserts that the live session is adopted by
the same worker and keeps `screen`, `input`, hook reports and notifications
working, that the stopped session resumes with a plain `resume`, which must
succeed, onto the package digest its profile names, that `stop` and a plain
resume of the adopted session relaunch through the package, that new Claude and
Codex sessions launch through the packages, and that the previous release's own CLI
still talks to the new daemon. A failure there means a live session would not
survive the upgrade or a profile would not launch; fix it before releasing. The
run keeps the previous release's store and protocol outputs as the
`upgrade-goldens-<tag>` artifact.

On its first start after an update the daemon migrates an older store before
it serves anything and first copies the original to
`<data_dir>/metadata.jsonl.pre-schema-<old>`, where `<old>` is the oldest
schema it found (owner-only; an existing backup is never overwritten). Keep
that file until the updated daemon has run correctly for a while, then delete
it (`pohunek service uninstall --purge` removes it with the store); to roll
back, stop the daemon, put the old binaries back, and copy the backup over
`metadata.jsonl`.

A daemon that finds a store written by a newer release refuses to start with a
`StoreSchemaError` ("has schema version N, newer than the schema version M this
daemon supports") and leaves the store untouched. This happens after a
downgrade or after running an older binary against a newer data directory.
Recovery: install the newer release again, or restore the
`metadata.jsonl.pre-schema-<old>` backup taken by the migration (losing the
changes made since it). A store older than every migration the daemon keeps is
refused the same way; migrate it with an intermediate release.

1. Download the component archive for the binary being updated: CLI (`pohunek`),
   daemon (`pohunekd`, `pohunek-sessiond`, and the `pohunek` CLI that installs
   them as a native service).
2. Run `pohunek doctor --json` to confirm the current binary can find required
   paths and state directories.
3. Run `pohunek health --json` to confirm the daemon responds with the expected
   version and protocol compatibility.
4. Run `pohunek host inspect local --json` to inspect local runtimes and
   capabilities.
5. Review config changes before applying `pohunek setup config --force`; default
   setup config should not overwrite existing files.
6. Attach reads its reconnect settings (`attach_reconnect_seconds`,
   `attach_reconnect_interval_seconds`, `attach_reconnect_max_attempts`) only
   from `<config_dir>/attach.conf`; a value left in `launcher.conf` is
   silently ignored. If you tuned those keys, copy them into `attach.conf`
   (`pohunek setup config` creates a template with every key commented at its
   default).
7. Upgrading from a release that installed launchers: core owns no launcher
   assets, and it deletes none, because you may have customised them. Remove
   the files the former launcher scripts and sway drop-in setup steps wrote, or
   keep them by installing the launchers from `zajca/pohunek-work`, whose setup
   command owns those scripts and keybindings from then on. The paths are
   the same on Linux and macOS (`XDG_DATA_HOME` and `XDG_CONFIG_HOME` when
   set, otherwise `~/.local/share` and `~/.config`):

   ```sh
   for name in lib.sh pohunek-rofi pohunek-rofi-issue pohunek-launch-issue pohunek-launch-pr; do
     rm -f "${XDG_DATA_HOME:-$HOME/.local/share}/pohunek/bin/$name"
   done
   rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/sway/config.d/pohunek.conf"
   swaymsg reload
   ```

   Run `swaymsg reload` only on a host that runs sway. A bare `pohunek setup`
   on macOS never wrote the sway drop-in and skipped the scripts, so only an
   explicit launcher scripts install leaves files there. The old drop-in bound
   `$mod+p` and `$mod+i` to the removed scripts; drop any `include` line for
   the `config.d` directory that you added only for it. Move the three
   `attach_reconnect_*` keys from `launcher.conf` into `attach.conf` as in the
   previous step; core never reads `launcher.conf`, so leave or delete it.
8. For important projects, verify `pohunek project show <id-or-label> --json`
   and resolved actions with `pohunek project actions <id-or-label> --json`.

For the Hermes M2 runtime, inspect the `hermes` entry after upgrade. Only the
official Hermes package serves it, so install that package from the new
release's daemon archive first (`pohunek plugin install
runtime/packages/hermes.tar.zst --catalog runtime/runtime-catalog.json
--yes`). It is launchable only with `version: "0.20.0"` and `supported: true`; a missing
binary has no version-policy result, while a wrong or unparseable version is
reported as unsupported. The model-free compatibility check, `cargo xtask
hermes compatibility --bin-dir ABS --package ARCHIVE`, needs the pinned Hermes
executable on `PATH`, an absolute directory `ABS` of canonical built `pohunek`,
`pohunekd`, and `pohunek-sessiond` executables, and the Hermes runtime package
archive built by `scripts/build-hermes-package`; it installs the plugin from
that package and validates committed evidence. Refresh goldens with `cargo xtask hermes refresh-goldens
--hermes-bin ABS`, where `ABS` is the absolute path to the pinned Hermes
executable. The harness runs that real Hermes process in a real PTY but replaces
the model API with a repository-owned, deterministic IPv4-loopback mock.
It requires no provider credentials and incurs no provider cost.
Credential-source suppression normally produces no Copilot startup probe. If
the pinned background exchange still starts, the mock admits at most its
three-attempt budget of `CONNECT api.github.com:443` requests plus a
three-attempt `CONNECT api.githubcopilot.com:443` fallback budget. Fast process
shutdown may shorten
those probes or interleave them with scenario traffic. The mock validates the
two exact request lines and matching `Host` headers, returns HTTP 403 before TLS
begins, and therefore receives no authorization header or token. An over-budget
attempt, any other `CONNECT`, extra header, or absolute-form external request
fails closed. Each of the six model-bearing classic scenarios must then make
this exact localhost sequence: five ordered detection GETs to `/api/v1/models`,
`/api/tags`, `/v1/props`, `/props`, and `/version`, each receiving a
deterministic HTTP 404; then exactly one `POST /v1/chat/completions`. Discovery
is not cached across those processes. The isolated config statically pins
`pohunek-compat-v1`, `context_length: 64000`, and `discover_models: false`, so
Hermes does not request `/v1/models` and the mock does not permit that path.
The isolated home is preseeded with fresh `models_dev_cache.json` and
`cache/model_catalog.json` files, and remote model-catalog refreshes are
disabled. Its isolated `auth.json` suppresses every Copilot credential source,
including the `gh auth token` fallback. A repository-owned noncredential value
is selected before that subprocess; its pinned three-attempt token exchange is
the locally denied probe described above. An unreachable isolated D-Bus address
also prevents child processes from opening the operator's desktop keyring.
Harness-owned HTTP(S) proxy variables point at the loopback mock and
exempt only localhost. The exact denied Copilot probe is the only admitted
non-local proxy authority and never opens a tunnel. This is a fail-closed
application-level defense, not OS-level network containment. Exact response
evidence is the pinned streaming response frame's
ordered rounded header, exact content, and rounded footer render events across
prompt-toolkit redraws.
The `prompt-ready` and `exit` classic scenarios issue no model API requests.
The mock validates this application-level sequence, the POST model and last user
prompt, and the terminal tool where required. The refresh uses an isolated home without reading the
real Hermes home or `state.db`. Review every refreshed fixture and leave no
pending golden records before release.
The refresh also sets `HERMES_SKIP_NODE_BOOTSTRAP=1` and gives only the TUI
process an empty isolated `PATH`. A missing Node/npm runtime is recorded as the
recognized local `unsupported` state; the harness must never install TUI
dependencies or contact a package registry. Classic terminal-tool captures keep
the normal executable path for their exact repository-owned commands.

Do not downgrade a host from M2 to M1 after it has persisted a Hermes session.
M1 can preserve unknown provider values neutrally on the wire, but it cannot
operate the M2 Hermes runtime or safely rewrite its persisted launch identity.
Recover by upgrading forward to the matching M2-or-newer component set.

For a daemon archive upgrade, run its installer
(`packaging/install-daemon.sh`, which runs
`pohunek service upgrade --from <archive-dir>`) rather than replacing only `pohunekd`. Before it changes anything, the upgrade
runs the [upgrade preflight](#upgrade-preflight) with the new binaries and
refuses when a live session would not survive the switch. It then copies the new
binaries and the release's catalog trust anchor into their own
`<prefix>/libexec/pohunek/<version>/` directory (the daemon reads the anchor
only at startup, so the restart below is what makes a new anchor effective),
switches `active_version` in `service.toml`, rewrites the daemon job to the new
version, and restarts only the daemon. Worker jobs are separate native jobs
(systemd transient units or launchd jobs, one per worker generation) that keep
running the versioned `pohunek-sessiond` they started from, so their PID, PTY,
and child PID remain unchanged. Version directories that a live worker journal,
a registered worker job (even one that has not started its process yet), or a
running process still references are kept; the others are removed and listed
in the upgrade report. When worker jobs cannot be discovered (including when a
worker unit or launchd definition of this installation is malformed), a worker journal
cannot be read, or a journal written by an older pohunek under an earlier
journal schema names a worker that may still run, every version is kept.
Rerunning `pohunek service upgrade` for the already active version restarts
nothing but repeats this cleanup, so a version kept earlier is removed once
nothing references it any more. This cleanup sees only the journals and worker
jobs of its own installation namespace, so a prefix belongs to exactly one
namespace (`<prefix>/libexec/pohunek/installation_owner`): an upgrade,
uninstall, or cleanup of another namespace fails with `service_prefix_owned`
before it changes anything. The owning installation's uninstall releases the
prefix. If the named namespace no longer has an installation (for example its
state directory was deleted by hand), delete that record and rerun. A version
directory that already exists is reused only when it and its binaries are
owned by you, mode `0755`, free of symbolic links, and single-linked;
otherwise the command fails with `service_version_untrusted` naming the
offending entry, and it does so again before a later command runs a binary from
the installed directory. Make sure nothing runs from it, remove it, and rerun. The
installer counts the daemon as ready only when `daemon.health`
reports the new version on a connection whose kernel peer credentials name the
daemon job's running main process, as the service manager reports it. A
manually started daemon of the same build that holds the socket while the
supervised job crash-loops therefore never makes the step ready: the command
keeps polling and then fails with `service_daemon_not_ready`, whose detail names
both processes (`daemon socket is served by pid X; the supervised job runs pid
Y`, or `has no process`). Stop the stray daemon and rerun the command. After
health returns:

1. Compare `pohunek service status --json` before and after the upgrade for an
   important live session: its `workers` entry keeps the same `generation` and
   `pid`, and `versions` still lists the old version as referenced.
2. Inspect that session and confirm the same `worker_id` and `worker_instance_id`.
3. Treat `runtime.state=incompatible`, `conflict`, or `lost` as a diagnostic
   state. Do not restart or kill the worker merely to make the status disappear.
4. If concurrent reconciliation or lifecycle work returns
   `runtime/session_runtime_commit_stale`, refresh the session with
   `pohunek session inspect <target> --json`. The losing operation was not
   published; retry only from the runtime identity, decimal generation, and
   state now reported as authoritative. This code is not a post-rename
   durability warning: the daemon internally logs and applies a commit whose
   rename succeeded but parent-directory sync remained uncertain.

An interrupted install or upgrade is journaled in
`~/.local/state/pohunek/service-install.json`; running the same command again
resumes it, and a different command rolls it back first — but only while the
transaction has not reached its `registering` step, because only then can
nothing have been registered. `pohunek service status --json` reports it as
`pending_transaction`. A transaction that passed its `config` step may already
run its daemon with live workers, so it is never rolled back directly: an
interrupted install met by `pohunek service upgrade`, or by `service install`
with another version or prefix, fails with `service_install_pending` and
changes nothing, and `pohunek service uninstall` removes an interrupted
install — or an interrupted upgrade — through
the full session-checked uninstall instead (`--stop-sessions` stops the live
sessions), without restoring the previous daemon. Whether the upgrade's daemon
was replaced or the replacement was interrupted before its service-manager
call makes no difference: the uninstall reaches the daemon that actually
serves the supervised job, as long as its version is one the configuration or
the pending transaction proves belongs to the installation — the active one,
or the previous version of a pending upgrade at or after `registering`. A job
actually serving anything else fails at once with
`service_daemon_version_unexpected`, before the uninstall touches a session —
even when no session or worker is live at all.
Rerun `pohunek service install` (or `packaging/install-daemon.sh`) to finish
it: the same version and prefix resume. The same rule covers an install whose
step fails at or after `registering` without an interruption: a service-manager
call can time out after it really registered the daemon job, and that daemon
may already own live sessions, so the install keeps its record, daemon job, and
`service.toml` and fails with `service_install_incomplete` instead of rolling
back. Rerun `pohunek service install` with the same version and prefix to
finish it, or run `pohunek service uninstall` to remove it after its session
check. An upgrade that fails before `ready` still rolls back to the previous
version. A rollback journals that it has begun before it changes anything; if
it fails or is interrupted partway (for example the restored daemon never
becomes ready), the record stays and the next `install`, `upgrade`, or
`uninstall` finishes the rollback first — rerunning the same upgrade then
starts it over instead of resuming it. `packaging/install-daemon.sh` asks
`pohunek service status --json` first and runs `service install` whenever the
pending transaction is an install, `service upgrade` otherwise when
`service.toml` exists, and `service install` on a fresh host; a failing status
query aborts it before anything changes. Only one service
transaction runs at a time: each holds `~/.local/state/pohunek/service-install.lock`,
a second one fails with `service_transaction_in_progress` (status then reports
`transaction_in_progress: true`), and a crashed holder's lock is released
automatically, so rerunning the command is always safe.

`pohunek service lock -- <command> [args...]` holds that lock while it runs
`<command>` and exits with the command's status (128 plus the signal number
when a signal ended it). The lock stays with that process: it records itself
and a fresh random token in the owner-private
`~/.local/state/pohunek/service-install.lock.holder` and passes the command
only the token, in `POHUNEK_SERVICE_LOCK_TOKEN`. Every `pohunek service
install`, `upgrade`, `uninstall`, `check`, or nested `lock` the command runs
adopts the lock with that token instead of waiting for it, while any other
service command is refused with `service_transaction_in_progress` until the
command exits. Adoption requires the lock to be held, the holder record to
carry the same token, and the recorded holder process to still run; anything
else fails with `service_inherited_lock_invalid`, never with a lock of its
own. Each adopting command also holds
`~/.local/state/pohunek/service-install.lock.adopted` shared for its whole
run. When the command exits, the holder waits until every adopter it left
running has finished, then removes its record and releases the lock; a
command that tries to adopt only after that is refused. If the holder dies
first, adopters still running keep every other transaction out with
`service_transaction_in_progress` until they end. Adopting commands that run
a transaction also hold `service-install.lock.inherited` exclusively, so two
of them under one holder never run at once: the second fails with
`service_transaction_in_progress`. The command runs in a process group of its
own and, when the lock process owns the terminal, in the terminal's
foreground; `SIGTERM`, `SIGINT`, and `SIGHUP` sent to the lock process or its
group are forwarded to the command's group, so each reaches the command
exactly once. On a terminal it behaves like a job: `Ctrl-Z` stops the command,
the lock process takes the terminal back and stops too, so the shell reports
the job stopped, and `fg` (or `SIGCONT` to the lock process) gives the
terminal back to the command and continues it. If execution fails, the error retains the original spawn failure and reports the owner PGID, initial terminal ownership, foreground PGIDs before and after recovery, and each attempted recovery syscall result (numeric errno on failure). These observations do not retry the handoff or change its ownership rules. One signal received while the
lock process waits for adopters ends that wait. If the lock process cannot tell whether an adopter still runs, it
removes its record, releases the lock, and fails. `pohunek service check [--prefix <dir>] [--json]`
runs, without changing anything, every check the install (while an install is
pending or nothing is installed) or upgrade of this version makes before its
first effect — `HOME` and the XDG roots, the prefix, every directory it writes,
a pending transaction it would refuse, the recorded installation, the prefix
owner, and, for an install that starts over, a daemon job registered without
`service.toml` (`service_daemon_job_present`) — and fails with the error and
code that command would fail with. For an upgrade it also runs the
[upgrade preflight](#upgrade-preflight) and accepts `--accept-runtime-loss`.
`packaging/install-daemon.sh` runs its whole legacy retirement and the final
`service install|upgrade` under `pohunek service lock -- <installer>`, and runs
`service check` before it touches the legacy install.

`pohunek service uninstall` refuses while sessions are live and lists them.
`--stop-sessions` stops every session through its worker first; the session
store, journals, and host identity are kept unless `--purge` is given. Purge
also removes the store's schema backups, crashed `metadata.jsonl.tmp.<pid>.<sequence>`
copies, and imported legacy-manifest archives in `migrations/`. It removes only
recognized artifact names; a symlink at a recognized temp or archive name is
unlinked without following its target. Unrecognized data and migration entries
are kept and reported (`kept_purge_entries` as byte-escaped display strings in JSON). A pending migration
manifest is retained. The prefix ownership record is removed after the prefix's version directories and
CLI copy (it stays while a version is kept for a worker journal or job of this
installation). The transaction record (`service-install.json`) is cleared just before
`service.toml`, which is removed last, so if an uninstall fails partway or is
interrupted, rerunning `pohunek service uninstall` finishes the cleanup. A
pending install or upgrade that reached its `registering` step is uninstalled
through the same live-session check even when its `service.toml` is already
gone: the record's version and prefix identify the installation. For a pending
upgrade this restores no previous daemon and runs no rollback: the previous
reader may already be incompatible with the migrated store and `uninstall`
exposes no `--accept-runtime-loss`, so `uninstall --stop-sessions --purge`
removes the pending upgrade and the installation even when
`pohunek service upgrade` to the previous version would be refused with
`service_rollback_store_unusable`; without `--stop-sessions` it still refuses
with `service_live_sessions` while any session is live.
A pending install that stopped before `registering` never started its daemon,
so `uninstall` rolls it back; with `--purge` it then purges the durable
metadata too (refusing with `service_daemon_job_present` while any daemon job
is registered, and with `service_orphan_workers` while a worker still runs),
and it keeps the transaction record until that purge finished, so a rerun
resumes it. Without an installation or a pending install, `uninstall --purge`
fails with `service_not_installed` and purges nothing.

A worker journal written by an older pohunek under an earlier journal schema
blocks the uninstall with `service_outdated_journals` while its worker may still
run: its recorded phase is not final and its recorded worker process is running
or cannot be identified. The error lists each journal with its schema version
and worker pid. This daemon cannot stop such a worker, so `--stop-sessions` does
not help: end those sessions with the pohunek version that started them, or
terminate the listed worker pid, then rerun the uninstall. A journal with no
readable worker pid keeps blocking until you delete it, after confirming that no
older `pohunek-sessiond` is still running. Before `service upgrade`,
`service uninstall`, or `service status` touches anything, it verifies
`service.toml` against the running user and the canonical `XDG_STATE_HOME`
and `XDG_RUNTIME_DIR` roots; a moved root or another user fails with
`service_config_invalid` naming the differing key, so the command can never
address another installation's jobs or socket, and `status --json` never
reports the recorded namespace and prefix beside another environment's
transaction store.

Before any effect, including rolling back a pending transaction, `service
install`, `upgrade`, and `uninstall` also refuse a `--prefix` that
`service.toml` would reject (`cli_usage`: relative, `.` or `..` components,
repeated or trailing `/`, non-UTF-8, or too long; pass the one normalized
spelling) and an `XDG_*` root or `HOME` that is not UTF-8
(`service_environment_not_utf8`), which the daemon's job definition cannot
carry. `install` and `upgrade` additionally require `HOME` to be set
(`missing_env`) and every bootstrap root to be an absolute normalized path,
with `HOME` an existing directory (`service_environment_invalid`): the daemon
starts each session worker in `HOME` and refuses to become ready without it,
even when `--prefix` and every `XDG_*` root are given explicitly.

The first worker-aware release is a destructive compatibility boundary because
a legacy daemon cannot transfer an already-open PTY. Let all legacy sessions
finish before installing. The installer moves the legacy daemon's control
socket aside, runs `migration preflight --socket <moved-socket>` against it,
and only then stops the daemon and re-checks template workers in every state
except `inactive`; any surviving worker aborts without removing legacy files.
`--accept-runtime-loss` is informed consent to lose those existing PTYs and to
proceed after the post-stop check, not a recovery command. See
[debug session runtime](debug-session-runtime.md). The same flag is the one
consent for the service-managed upgrade below; there is no second flag.

When the assistant feature is available, its update intent should use bundle
version metadata and `changed_in` frontmatter to explain version-specific
changes before recommending edits.

## Roll back after a completed 0.36 upgrade

Rolling back from a completed 0.36 upgrade to 0.35.2 is not supported (the
procedure below is written for 0.35.2 and its package versions). 0.36 writes
`service.toml` at schema 7, migrates the session store to schema 11, records its
own runtime catalog as the high-water mark, and selects official runtime
packages that 0.35.2 cannot read:

- Codex 1.3.0: its `detect.toml` has a `[program_status]` table and
  `overrides_program_status`, and its `runtime.toml` has `ready = "manifest_idle"`;
- Pi 1.0.4: its `detect.toml` has a `[program_status]` table;
- Claude 1.1.0: its `detect.toml` has a
  `[program_status]` table. Claude 1.0.2 has no such table and loads in 0.35.2.

The 0.35.2 detect-manifest parser rejects the table and the key, and its runtime
definition parser rejects the `ready` value. A failed `pohunek service upgrade`
is not affected: it rolls back automatically and restores the previous packages,
the catalog high-water mark and the host profiles byte for byte.

If you must go back after a completed upgrade, run these steps in this order.
Steps 1 and 2 need the 0.36 binaries still installed:

1. Check that `pohunek plugin list` still shows the 0.35.2 versions (Codex
   1.1.1, Claude 1.0.2, Pi 1.0.3). Select each one:
   `pohunek plugin select pohunek.runtime.codex --version 1.1.1`, the same for
   `pohunek.runtime.claude --version 1.0.2`, and for `pohunek.runtime.pi
   --version 1.0.3` when Pi was updated. For Codex and Claude this needs the
   0.35.2 versions still installed: 0.36 cannot reinstall an uninstalled one,
   because it refuses the 0.35.2 catalog as older than its high-water mark and
   refuses a digest-trusted install of an official runtime. Without them, the
   only way is forward. Pi is not a reserved runtime id in 0.36 and 0.35.2 does
   not plan it, so a missing Pi 1.0.3 does not block steps 3 to 5; reinstalling
   it by digest under 0.36 is not traced.
2. Run `pohunek plugin profile migrate <name> --yes` for every profile over
   those runtimes, so the profile pins the selected version.
3. End all sessions, then run `pohunek service uninstall` without `--purge`: it
   keeps the store, the packages and the profiles. The 0.35.2 `service upgrade`
   cannot read the 0.36 `service.toml` (`UnsupportedSchema`).
4. Move `<data_dir>/metadata.jsonl.pre-schema-10` over `metadata.jsonl`
   (`mv`, so the backup is consumed: a later upgrade keeps an existing backup
   instead of writing a fresh one, and a second rollback would restore the old
   one). Sessions recorded under 0.36 are lost. If there is no backup, the store
   was empty when 0.36 first started and nothing was migrated: move
   `metadata.jsonl` aside instead, so 0.35.2 starts from an empty store.
5. Run the 0.35.2 archive's `packaging/install-daemon.sh`.

Ignore that run's advice to select the newer package again or to leave pinned
profiles alone: both assume the newer package loads. Do not roll the binaries
back first. 0.35.2 then refuses the install or upgrade
(`service_upgrade_runtimes_refused`, `release_runtime_catalog_unavailable`, because its
verifier rejects the lower catalog sequence), and it can neither select the
older Pi nor migrate a profile while Pi 1.0.4 is selected.

## Upgrade preflight

`pohunek service upgrade` and `pohunek service check` of an existing service
run a read-only preflight with the new binaries before any effect: the new
`pohunek` runs the new `pohunekd upgrade-preflight`, an internal command of the
one release archive that prints a single JSON report. It reads the metadata
store (migrated in memory only), the worker journals and the process table. It
takes no lock and writes, signals and stops nothing, so it is safe while the
previous daemon runs. It does not connect to worker sockets, because the
running daemon holds each worker's single controller slot; it judges the
identity, subagent state and protocol a worker would report from what its
journal recorded, with the same functions adoption applies (including the check
that the launch and active identity processes are still in the PTY tree,
`launch_identity_process_invalid`, `active_identity_process_invalid`,
`identity_process_root_missing`; a process table that cannot be read fails
closed with `identity_process_inspection_failed`). A record with a pending stop,
removal or undelivered create is reported too (`stop_or_removal_intent_pending`,
`create_rolled_back`), because startup finishes those instead of adopting. The
real blind spots are what only a service manager or the live socket can show:
the supervisor job's identity and socket failures at connect time. A worker
that journals no hook schema (a previous-release worker) is validated with the
schema its runtime declares, read without side effects from the built-in
runtime definitions and the installed package store; a live agent session that
reported its active identity is therefore adoptable. Only a runtime whose
definition cannot be read leaves the schema unavailable
(`worker_identity_unverified`). The previous daemon has no facility that pauses
session creation, so the upgrade runs the preflight twice: once before any
effect, and again after staging, immediately before the daemon is replaced. A
session created in between is caught by the second run; the window that remains
is the replacement itself, where the new daemon's own adoption decides and fails
closed (conflict or lost, with a WARN and a recovery hint). An interrupted
upgrade whose daemon replacement has not completed (including one interrupted
just before the service-manager call) is judged again when it is resumed, so
state that changed meanwhile needs consent. A refusal never touches the running
daemon: one that happens before the replacement is rolled back without
restarting it, and one at a resumed upgrade that may still replace it keeps the
transaction, so rerunning with `--accept-runtime-loss` (or after the sessions
end) resumes it. A rerun for the already active
version skip it, as does a resume after the replacement.

Cancelling an interrupted upgrade before the daemon registration step usually
leaves the previous daemon in place. A configuration migration stops
it earlier, before the rewrite; rollback from that `stop_daemon` step or later
must restart it. Before such a restart, rollback judges the previous daemon
against the current store and live workers before changing the service. A
previous daemon with `upgrade-preflight` judges its own reader and
adoption; v0.33.0 and v0.33.1 have no such command, so only their schema-1
store is admitted, and each record must deserialize the way those releases
read it: a line their reader would silently skip on its next whole-store
rewrite — a known record kind with a missing or ill-typed field, or an agent
kind that is not a runtime id — refuses the rollback before anything changes.
Live worker recovery needs `--accept-runtime-loss`. A newer, corrupt, or
unreadable store refuses rollback even with that flag. The
installer stops the new daemon and judges again before starting the old one,
so a write during the first check cannot become an unsafe downgrade. If that
second check refuses, the daemon remains stopped and the transaction remains
for recovery. `service check` applies the same read-only judgment; successful
`--json` reports include the rollback preflight and
`accepted_runtime_loss` in `rolled_back`. Restore the schema backup after
stopping the daemon, or keep the newer release installed, to recover from an
unreadable store. Worker journals that cannot be read are never treated as
"no live workers": a journal root that cannot be scanned yields an
`evidence_unavailable` entry, and while a store line that names no session
cannot be read, an unreadable or unsupported journal of an unknown session is
listed as at risk. `service uninstall` keeps its own `--stop-sessions` rule
and removes a pending upgrade at or after `registering` without restoring the
previous daemon, as described above.

The report has two parts. The store part is the dry-run startup migration:
`missing`, `up_to_date`, `would_migrate` (with the schema it migrates from and
to, and the record count) or `refused`. A refused store (newer than the
binary, no migration path, an invalid `schema_version`, a migrated body over the
16 MiB store limit, or unreadable) means
the new daemon would not start. The upgrade then fails with
`service_upgrade_store_unusable`, and `--accept-runtime-loss` never overrides
it: fix or restore the store, or install the matching newer release.

The sessions part lists every live session with one verdict and a stable reason
code. A session whose stored runtime is already lost or terminal is not listed:
the upgrade is not what ends it.

- `adoptable`: the new daemon adopts the worker and keeps its native recovery.
  A session launched from the built-in Codex, Claude Code or Hermes of an earlier
  release is adoptable when the official package that serves its runtime id is
  installed and adopts it, live or stopped (its pin names that package id; the
  package version may differ). Resume and fork then run through the package.
  A stopped session launched from an official package is
  `adoptable_by_newer_package` when a strictly newer version of the same
  package adopts it. The `detail` names both versions. See "Stopped sessions on
  an older official package version" below.
- `would lose recovery`: the worker is adopted but the session cannot resume
  natively (`native_recovery_unmappable`, `resume_record_unreadable`,
  `native_recovery_unrepairable`, `runtime_source_not_served`). A binding that
  needs the runtime registry to complete its launch spec is repaired first, by
  the function startup uses, over the profiles and runtimes the new daemon will
  have; nothing is written. It is `native_recovery_unrepairable` only when that
  repair fails: the agent no longer resolves, runs on another runtime, declares
  no native resume or another reference kind, or its launch pin names a source
  the new daemon does not serve. The `detail` names the reason. Restore the
  profile or runtime it names, or accept the loss; accepting it is one-time.
  `native_recovery_unverified` appears only when the installed daemon predates
  this repair and judges a rollback. `runtime_source_not_served` means the session's launch pin names a
  runtime source the new binary does not serve and no installed package adopts:
  a built-in runtime it does not ship, or a package digest that is not
  installed. Resume and fork resolve exactly the pinned source, so nothing
  else stands in and no command rebinds the session. Finish the work in the
  live session, then continue in a new one with `pohunek session new --agent
  <runtime>` (the `detail` names the runtime), or accept the loss. When the
  session's runtime resolves nowhere on the new binary (the built-in Codex,
  Claude Code or Hermes without the official package, or a package that is
  not installed), the
  session is reported this way even if it cannot resume, because every
  interaction with it (input, attach, resize, rename, metadata) would be
  refused too: install the runtime first (the `detail` names the official
  package install), then finish the work. A session whose host profile resume
  would refuse is reported with resume's code (`agent_profile_missing`,
  `agent_profile_changed`, `agent_profile_revision_unavailable`): restore the
  profile, or accept the loss. A stopped session record the new binary cannot
  load is `record_unreadable` in the stopped list; its recovery is lost at
  startup.
- `already not resumable`: a stopped session whose conversation is absent from
  its declared store (`agent_native_reference_missing`), or whose stored
  reference is malformed, is refused by resume on the running release too. The
  upgrade does not change that, so it needs no `--accept-runtime-loss`; the
  summary counts it apart from the sessions at risk. A relocated store
  (`agent_native_store_relocated`) or a store-condition refusal is `would lose
  recovery` with resume's code: the running release may detect those
  differently, so the preflight cannot prove it refuses too. A reported reference
  keeps its check only where the preflight can prove it identical: a session
  pinned to a package digest, or a built-in that a 0.34.0 to 0.34.2 store (schema
  5) adopts into the official package whose check equals the v0.34.2 golden. Any
  other session the upgrade adopts from the built-in runtime is `would lose
  recovery`, with the same code and a detail that names the reason. Sources
  outside those are fail-closed, so the verdict is `would lose recovery` there.
- `would not be adopted`: the new daemon would not manage the worker
  (`record_unreadable`, `worker_without_session_record`, `store_unusable`,
  `worker_generation_missing`,
  `worker_journal_schema_unsupported`, `worker_journal_unreadable`,
  `worker_journal_missing`, `worker_journal_ambiguous`,
  `worker_already_ended`, `worker_not_running`, `worker_process_unverifiable`,
  `runtime_identity_mismatch`, `worker_generation_not_advanced`,
  `worker_child_missing`, `worker_not_initialized`,
  `worker_protocol_incompatible`, `worker_marker_legacy`). Evidence the
  preflight cannot read counts against the session.
  `worker_marker_legacy` means the worker journal names a release before
  0.33.1, or none: that worker marks its children without
  `POHUNEK_WORKER_INSTANCE_ID`, so the new daemon could never sweep or report
  them and refuses to adopt the worker. Stop the session with the running
  release before upgrading, or accept losing its runtime.
  `worker_without_session_record` applies to a live worker journal with no
  store record only while a store line that names no session cannot be read,
  because that line may be the worker's own record.

A live worker journal with no store record and no such unreadable line is
listed under `unmanaged_workers`: the running daemon does not manage that
worker either, so the upgrade leaves it as it is and does not refuse.

When a live or stopped session is at risk, the command exits non-zero with
`service_upgrade_sessions_at_risk`. Upgrade and rollback refusals count live
and stopped at-risk sessions separately and label each listed session with its
kind, verdict, code and detail. Let those sessions finish (or end them) and rerun, or pass
`--accept-runtime-loss` after reviewing the list. With the flag the upgrade
proceeds, and the `--json` report carries the full `preflight` object and
`accepted_runtime_loss: true`. A preflight that cannot run at all (missing or
failing `pohunekd`, an unparsable report) fails with
`service_upgrade_preflight_failed`, which the flag does not override.

The flag also records what it accepted. When the upgrade completes, every
at-risk verdict the daemon binds to an acceptance scope (`acceptance_scope`,
`v1:` and a digest of the session, its generation, worker instance, launch pin,
native reference and capabilities) is written to
`<state>/pohunek/service-accepted-risks.json`, replacing the file of the
previous upgrade. At the next upgrade the same session, list, verdict, code and
scope is reported as previously accepted at upgrade to that version and does not
block, so the flag is needed again only for what is new. Nothing about a session
changes: the recovery that still works keeps working, and accepting never
retires it. The upgrade and `check` output group the verdicts as
`previously_accepted`, `durable_acceptance` (accepted now and recorded) and
`one_time_acceptance` (accepted now, but the verdict carries no scope, such as a
probe-budget verdict, so the next upgrade asks again); `accepted_runtime_loss`
still means the flag accepted a remainder. `service check` reads the file and
never writes it, and a rollback, a refusal or a failed upgrade leave it as it
was. A ledger for another version, namespace, metadata store, scope scheme or
schema (for example after a rollback or a downgrade) is ignored with a warning
and the sessions need the flag again; a corrupt or unsafe file fails with
`service_accepted_risks_invalid`, which names it. Remove the file to forget every
acceptance. The first upgrade judged by a daemon that reports scopes still needs
the flag once.

On Linux the preflight and the runtime step run with the environment the
daemon job gets from the systemd user manager and its own unit, read over the
session bus on every gate (nothing is reused from an earlier gate): the `PATH`
`service.toml` records, else the manager's `Environment` `PATH`; either one is
replaced by a drop-in of the daemon unit that sets `PATH` (a warning names the
drop-in when it replaces a recorded one). A drop-in can also set the allowlisted
store variables such as `CLAUDE_CONFIG_DIR`, which then replace the manager's, and
it must not set or remove a bootstrap key (`HOME`, `XDG_*`) or reset
`Environment=`, because the unit file the install writes gives the job those. If
the manager or the unit cannot be read, or a setting cannot be judged, the command
stops with `service_manager_environment_unavailable` before any effect; the flag
does not override it, whether or not the job records its `PATH`. The rules are
under "The journal of an interrupted upgrade" below.

On macOS a job takes the store variables from the launchd domain. When launchd has
no `gui/<uid>` domain (no GUI session), `service check` warns that native
references were judged at the default store locations. When that judgement covers
a stopped session, `service upgrade` or a rollback stops with
`service_manager_variables_unread` before any effect; log in to a GUI session and
rerun, or pass `--accept-runtime-loss` to accept that a session judged that way
may lose its recovery. Without stopped sessions the upgrade proceeds with the
warning.

Sessions whose runtime has ended but that keep native resume or fork are judged
by the same rules and listed under stopped sessions (`stopped_sessions` in the
`--json` report, and a `stopped` section in the text output). A stopped session
that would lose its resume or fork is at risk and needs `--accept-runtime-loss`
like a live one.

An owner-requested stop or removal is not a runtime-adoption loss, even when
the runtime is still alive: the new daemon finishes the existing intent. The
preflight remains read-only and does not relax process-identity, unreadable
evidence or foreign-process checks. A stop still retains native recovery, so
its resume or fork is judged separately and can remain at risk. A removal
keeps no recovery to preserve. Finishing the intent itself is reported with
the non-blocking `adoptable` verdict and `stop_or_removal_intent_pending` code.

When the preflight budget (20 seconds by default) runs out before a stopped
session's native reference is checked, the session is `would_lose_recovery` with
`reference_unverified_within_budget`, and its detail gives how many sessions are
unverified this way. Run `pohunek service check --preflight-budget <SECONDS>`
(up to 300), then `pohunek service upgrade --from <archive dir> --preflight-budget
<SECONDS>` with the same value; the CLI's preflight deadline grows with the
budget. The installer (`packaging/install-daemon.sh`) passes no budget, so an
installer user runs the archive's own CLI directly:
`<archive dir>/pohunek service check --preflight-budget <SECONDS>`, then
`<archive dir>/pohunek service upgrade --from <archive dir> --preflight-budget <SECONDS>`.
Unlike the wrapper, a direct run does not take the service transaction lock
around both commands and does not print the hook asset step afterwards
(`pohunek integration doctor`, then `pohunek integration install --agent <agent>`
for each outdated agent). `--accept-runtime-loss` accepts such sessions only
after the check has been reviewed.

### Official runtime packages during an upgrade

The preflight also carries the release's official runtime plan, from
`pohunekd ensure-official-runtimes --dry-run` run against the release's
`runtime/` tree before any effect. The plan lists the packages the host uses
with the reasons (`profile`, `session`, `integration`), the host profiles it
pins, the integration hooks it reinstalls, and every refusal. Live and stopped
sessions are judged against the runtime registry as it will be after the plan
is applied, so a host on an older release whose sessions the release's packages
adopt is not refused for them. `service check` and `service upgrade` print the
plan as `plan` lines and each refusal as a `refused` line.

A plan with refusals (for example, no package for this platform) fails with
`service_upgrade_runtimes_refused` before anything is stopped, and
`--accept-runtime-loss` does not override it. The error lists every refusal. On
a fresh upgrade the early gate refuses it before the binaries are published; a
resumed upgrade, or a plan that changes between the two gates, is refused at the
later gate, which keeps the transaction for a rerun or rolls it back. A plan that changes
something is applied after the old daemon stops: the package registry, the
catalog state and the host profiles are backed up owner-private first, and the
backup is restored when anything fails. Once the new daemon is ready, the
integration hooks the release lists are reinstalled. A failure there is reported
and does not roll the upgrade back. A marked Hermes home is updated with the
`hermes` executable on the daemon job's `PATH`, the one the runtime plan ran
with, not the `PATH` of the shell that runs the upgrade. A Hermes home without
the pohunek marker is reported as a gap with the command that installs it;
nothing is written into it. A host without a `~/.hermes` directory reports
`integration hermes  gap: Hermes is not installed (no <path>), skipped`; a Hermes home that
exists but is unsafe (for example a dangling symlink) is still a failure.

The manual migration below remains the fallback for a host that cannot run the
upgrade's runtime step, for example to move its sessions before an upgrade.

A host whose previous release installed the packages itself (its own `service
install` pinned its profiles to them) gets the release's packages too. The plan
reports the release's package as `present` when its digest is the one the host
serves, and as `install` otherwise. A host profile pinned to an older version of
the same official package is planned as `repin` and pinned to the release's
package by the apply (`repinned`). Its entry names the previous and the new
version and digest. A pinned profile the step does not move is listed as
`kept`, with a reason:
- `pinned_to_newer_version`: the profile's package is newer than the release's;
- `pinned_to_non_official_package`: a local or linked package;
- `pinned_to_other_package`: another package id;
- `pinned_version_unknown`: the pinned digest is not installed, or its version
  is not a semantic version;
- `pinned_to_same_version`: another digest of the release's version;
- `pinned_differs_from_kept_selection`: the step keeps a selected version of
  the runtime that cannot be ordered against the release's, and the profile is
  pinned to another official version of the same package; it is never repinned.
  Beside a kept selection a pin with one of the reasons above (unknown digest,
  non-official or other package) reports that reason instead;
- `pinned_target_not_official`: the release's package is recorded as not
  official on this host.

In `--json`, a `repin` entry carries `previous_digest`, `previous_version` and
`version`; a `kept` entry carries `kept_reason`, its pinned `digest` and, when
the registry records it, `previous_version`. A package entry whose selected
version is newer carries `replaces_newer` with that version's `digest` and
`version`. The report layout is `report_version` 2.

A kept profile changes nothing and does not refuse the plan. Move it with
`pohunek plugin profile migrate <name> --digest <digest> --yes` when you want to.

When the selected official package of a runtime is newer than the release's
package (installed with `plugin update` from a later archive), the plan says
that the upgrade re-selects the release's lower version. The upgrade still does
that. Select the newer version again afterwards with `pohunek plugin select
<package> --digest <digest>`. Profiles pinned to the newer version keep their
pin.

When the selected official version of any runtime, `claude`, `codex` and
`hermes` included, cannot be ordered against the release's (for example `2.0` against
`1.0.3`), the upgrade keeps the selected version, installs nothing for that
runtime and reports the gap `plugin_update_version_unordered`. The plan names
the retained package and the version the release offers, as it does for Pi.
When the registry already records the release package's id and version under another
digest (a rebuilt archive that kept its version string; the record may be selected or
not, enabled or not), the upgrade cannot install the release's archive beside it
(`package_identity_installed`). It keeps the enabled selection that serves the runtime,
installs nothing and reports the gap `plugin_update_same_version_other_digest`. The fix
at the source is a release that bumps the version. The detail names
`pohunek plugin uninstall <package> --digest <digest> --yes` of the recorded package only
when that package is the verified, selected one of a reserved runtime that a profile,
session or hook makes the plain upgrade plan, and no session or host profile pins it and
no unpinned profile is bound to it by the upgrade (the next upgrade then installs the
release's archive). The profile the earlier upgrade bound normally pins it, no command
releases that pin, and the detail then names no command. For Pi it never names one: the
upgrade does not install a Pi package again once none is installed.
When no enabled official selection serves beside the recorded package (nothing is selected,
or the recorded package itself is the selected one and its root is missing or does not load), the upgrade installs nothing,
leaves the runtime and reports the gap `official_package_not_serving`; for a runtime
that is used (a Pi always), the detail names the removal of the obstacle (the selected package, or the
recorded one when nothing is selected) with `pohunek plugin uninstall <package> --digest
<digest> --yes`, and `--remove-modified` for an altered root, which is a refusal. A host
profile or session record that pins the obstacle is named first, with how to release it, because the registry
refuses the removal while it stands. The detail ends with what follows: run the upgrade again, or for a Pi
that keeps no enabled official selection `pohunek plugin install runtime/packages/pi.tar.zst
--catalog runtime/runtime-catalog.json --yes` (`plugin update` with the same arguments when
another Pi version is installed), because the upgrade does not plan such a Pi. Another selected
version that does not load, or a non-official selection, gets `plugin install` or
`plugin select` next to that removal while nothing pins the recorded package.
A selected package you disabled is never replaced, selected or enabled, whatever its
version is (older, newer, equal under another digest or unordered; the release's own
package counts too): the plan plans nothing for that runtime and reports the gap
`official_package_not_serving` with `pohunek plugin enable <package> --digest <digest>`.
Enable it and the next upgrade proceeds as for any enabled package. A disabled
package whose root is missing gets `plugin install` of that version's archive in the
hint before the enable, one whose root is altered is refused with
`plugin uninstall --remove-modified`, and one that cannot be enabled because another
package serves the runtime id is the gap `runtime_served_by_other_package` (a refusal
without `--detect`); a Pi runtime the plan does not judge gets only a gap that points to
`pohunek plugin doctor` in those states. The release's own package, recorded and
disabled while another enabled version is selected, is not enabled or selected by the
upgrade either: while an enabled selection loads, it keeps serving, with its package entry and
integrations, and the gap names `plugin enable` for the release's digest. The case
table "Disabled release package: case table" in `docs/public-api.md` gives the outcome for
every state of the selected version and of that package's root, including when an
altered root is refused with `plugin uninstall --remove-modified`; a selected Pi version
newer than the release's that loads is kept silently, with no gap, and a selected version that
cannot be ordered against the release's and loads is kept with the gap `plugin_update_version_unordered`. The
detecting plan of `pohunek setup` drops `official_package_not_serving` together with a
runtime it drops (nothing uses it, no profile has it as its base, and its program is
off the search path), whatever the state of its package. The same holds for a selected version that is enabled but
cannot be loaded: the plan names the repair (`plugin install` of that version, or
`plugin select` of another). A selected version whose root is present but altered
is refused, and the refusal names `plugin uninstall --remove-modified`. Unpinned profiles
of a reserved runtime are bound to the kept version. `pohunek setup` prints the same
"keeps the selected ...; the release offers ..." line as `service check`.
`pohunek plugin select <package> --digest <digest>` moves it on purpose.

### A recorded search path that no longer matches your profile

The upgrade preflight finds runtimes with the `PATH` the daemon job carries,
which `service.toml` records at install. Each recorded entry is the canonical
path that was validated at install, so an entry that resolved through a Nix
profile link names the store path of that generation. After a Nix profile update
the entry still holds the old generation, and a runtime found there can be below
the range the release requires although the profile now provides a current one.
The refusal is `service_upgrade_sessions_at_risk` and offers
`--accept-runtime-loss`, which would drop resume and fork for the listed
sessions.

On macOS, where the install discovers the path through your login shell, the
refusal also carries a warning that names the recorded path the upgrade judged
with. It runs no login shell. (A Linux install records no path, because the
user manager's `PATH` applies, and gets no such warning.) If a runtime moved
since the path was recorded, do not accept the loss. Use the
`pohunek` of the release you are installing (`./pohunek` in its unpacked
archive), because the installed one may predate the command. Compare with
`pohunek service refresh-path --dry-run`, which runs the login-shell discovery
as install does and prints the directories it would add and remove, record the
current path with `pohunek service refresh-path`, and rerun the upgrade. The
command changes only the recorded path in `service.toml`, takes the transaction
lock only after the discovery, and refuses while an install or upgrade record
is pending. Nothing is stopped or restarted: `service check` and `service
upgrade` judge with the new path at once, and the running daemon job gets it at
its next replacement. Do not use `--accept-runtime-loss` for this cause, since
it would drop resume and fork for the listed sessions although the runtime is
current. `refresh-path` refuses while a transaction record is pending
(`service_refresh_blocked_by_transaction`), and an interrupted upgrade leaves
one. First rerun the command `pohunek service status` names (it resumes or
rolls the record back), then run `pohunek service refresh-path` and the
upgrade. Only when the resumed upgrade is refused again with the same warning
the record cannot be refreshed around: stop the live sessions, save a
copy of `service.toml`, remove the record with `pohunek service uninstall` (it
checks live sessions first), then install the version that is installed now from
its own archive with the same `--prefix`: a fresh install discovers the current
path, so no `refresh-path` is needed. Reapply the non-default settings from the
saved copy, but not `search_path` under `[environment]`: it is the stale value
and the upgrade would refuse again. Then rerun the upgrade.

`refresh-path` rewrites only a `service.toml` of the schema its own release
writes, and reports `service_refresh_config_schema` for another one without
touching the file. A file written by a newer release is refreshed by that
release's own `<prefix>/bin/pohunek service refresh-path`. A file of an older
schema is refreshed the same way when the installed release has the command
(v0.39 and later); for a release without it, use the uninstall and fresh
install above, with the same save, stop and reapply steps.

### Stopped sessions on an older official package version

A stopped session launched from an official package is pinned to that package's
digest. On resume or fork, a strictly newer version of the same package adopts
it, so a release that widens a package's supported agent range also reaches
sessions launched before it. All of these must hold:
- both versions are official (recorded from the signed catalog) and serve the
  same runtime id under the same package id. A package installed by explicit
  digest or linked never adopts a session, even under the official package id;
- the newer version is selected and enabled, and its root verifies;
- the older version is still installed, and the newer one declares the same
  integration handler, hook schema and native reference check;
- the newer version honors the launch the session froze: the same reference
  kind, the same assigned reference strategy, and fork support when the frozen
  launch forks;
- for a session launched from a host profile, the profile now pins the newer
  version (the upgrade repins it, see above). A profile still pinned to the
  older version keeps the session on it.

This covers Pi too. The upgrade's runtime step manages `claude`, `codex` and
`hermes`, and also an official Pi package that is installed, selected and
enabled: it installs and selects the release's Pi version and repins a Pi
profile pinned to the older official version. An unpinned Pi profile follows the
selection. An absent or deselected Pi package is not changed; the plan lists
a gap with the `pohunek plugin update pohunek.runtime.pi
runtime/packages/pi.tar.zst --catalog runtime/runtime-catalog.json --yes`
command. A disabled one is not changed either, and its gap names
`pohunek plugin enable pohunek.runtime.pi --digest <digest>` instead. After the update `pohunek plugin profile migrate <name> --yes` moves a Pi
profile. A selected Pi version newer than the release's is kept. Its stopped
sessions are adopted on their next resume or fork. A session
launched as the bare `pi` runtime follows the selected version.

A newer version that fails one of these checks does not break the session.
When the upgrade repinned its profile to that version, the session still
resumes from its frozen digest, as before the upgrade. The profile counts as
unchanged when only its pin moved to a strictly newer official version of the
same package. The preflight reports the session as `adoptable`.

A digest the registry already records under another trust source (for example
`explicit_digest`) is never the official newer pin, even when the release plan
lists it: the preflight and resume both judge it by that record, so a session
whose profile pins such a digest is not adopted and resume refuses
`agent_profile_changed`.

The relaunch keeps the frozen program and arguments. It probes the agent with
the newer version's version probe and records the newer digest as the session's
pin. A lower version never adopts a session, and neither does another digest of
the same version (the registry holds one digest per version). A live session is
never adopted: it keeps being observed through the digest it runs.

"Official" means recorded from a catalog that the host's single trust anchor
verified. The registry does not record which anchor root or key authorized a
package ([#1200](https://github.com/zajca/pohunek/issues/1200)).

The apply is judged once more. After it, the new release's preflight judges the
live sessions again, with the same decision as before the apply, because the
apply may select a package with another native contract. A session at risk, or
a preflight that cannot run, rolls the apply back.

When the release carries no usable runtime catalog, an installed package that
loads keeps serving its runtime: the step reports `catalog: unavailable` and the
package as `kept`, with nothing installed and nothing refused. A runtime with no
installed package that serves it still fails with
`release_runtime_catalog_unavailable`. A plan whose only change is the release
catalog's trust state (`catalog: record`) stops and backs up the daemon like any
other apply.

### The journal of an interrupted upgrade

An interrupted upgrade leaves its transaction journal, `service-install.json` in
the CLI's state directory. On macOS it holds the launchd default `PATH` of the
previous daemon job, in the optional `launchd_default_path` field, read from the
loaded job when the upgrade started, and the boot session UUID of that moment
(`kern.bootsessionuuid`, which only a reboot changes) in `launchd_boot_identity`.
Linux never writes these fields, and a journal written without them has none.

A job without a recorded `PATH` gets, in launchd's order: the domain's `PATH` (set
with `launchctl setenv`, readable whether or not the job is loaded); the loaded
job's default; for a job that is not loaded, the journaled default of the
transaction while the host has not rebooted since (the persisted configuration is not read then, so an unreadable one does not block it); and only when none of these is
available, the persisted launchd configuration. A rollback therefore judges with
the `PATH` the restored job gets where launchd can still say so. The
persisted-configuration case that is only a stand-in is reported as a `warning:`
line that names the journal, because it is the best evidence available and not
launchd's answer.

Before any step stops or replaces a loaded job (the swap, the runtime apply, the
migration stop, a rollback that stops the daemon) the CLI journals the default
the loaded job reports now, with the boot UUID
at that moment, so the gates after the stop and a rollback after a failed apply
judge the value that was observed, not one an earlier boot saw. A job whose
default cannot be read keeps the existing journal.

A reboot after the journal write makes a pending `launchctl config user path`
active, so a journal whose boot session UUID differs from the current one is not
used: the persisted configuration is, but only when `user.plist` was last
modified before the current boot (`kern.boottime`; for an absent file, its
directory's modification time). A later write is pending, or was applied by the
boot and changed again, and its value says nothing about what launchd applied:
equality with the journaled default or with any earlier value is never evidence.
A journal without a boot UUID, or a host whose boot UUID cannot be read, cannot
tell whether a reboot happened: the same time rule applies and the persisted
value must also equal the journaled default. When the rule cannot be shown to hold, the two
sides of a transaction differ:

| Persisted configuration after a reboot | Forward gate (upgrade, resume, check) | Rollback and its gate |
|---|---|---|
| Older than the boot (and equal to the journaled default when the journal has no usable boot marker) | its `PATH`, no warning | its `PATH`, no warning |
| Modified after the boot, one whose time cannot be read, an unreadable boot time, or (without a usable boot marker) a value that differs from the journaled default | refused with `service_manager_environment_unavailable`; the message names the fix (set the `PATH` with `launchctl setenv PATH <directories>` in the GUI session) | its `PATH` as a stand-in, with the `warning:` line, because refusing a rollback would strand the host |
| Present but unreadable (permissions, corrupt content) | refused with `service_manager_environment_unavailable`, naming the read error | launchd's standard `PATH` as a stand-in, with the `warning:` line |

The times compared are the later of a file's modification and status change time
(so a copy or rename that preserves the modification time still counts), and the
nearest existing directory's for a missing file. Known limit: both times are on
the calendar clock, so a clock step right after the boot (a VM resumed with a
stale clock, then corrected) moves `kern.boottime` but not older file times, and
a `launchctl config user path` written inside that window can be taken for a
value the boot applied. Setting the `PATH` with `launchctl setenv` avoids it.

### A job that is not loaded and has no journaled default (macOS)

When the early gate of an upgrade finds the daemon job not loaded, there is no
launchd default to journal, and the persisted configuration (`user.plist`) is the
best evidence of the `PATH` launchd applies. It is the active one only when it
was last modified before the host booted, by more than a margin (120 seconds,
`LAUNCHD_CONFIG_BOOT_MARGIN` in `settings.rs`, so that a clock step around the
boot cannot reorder the two times); an absent file is dated by its directory, and
launchd's standard `PATH` then counts as persisted.

| Persisted configuration | Forward gate (upgrade, check, setup) | Rollback and its gate |
|---|---|---|
| Older than the boot (or absent and its directory older) | its `PATH`, no warning | its `PATH`, no warning |
| Newer than, or within the margin of, the boot; or the boot time or the file time cannot be read | refused with `service_manager_environment_unavailable` before any effect: start the job so launchd loads it, set the `PATH` with `launchctl setenv PATH <directories>` in the GUI session, or reboot | its `PATH` as a stand-in, with the `warning:` line, because refusing a rollback would strand the host |
| Present but unreadable (permissions, corrupt content) | refused with `service_manager_environment_unavailable`, naming the read error: fix the file's permissions or content | launchd's standard `PATH` as a stand-in, with the `warning:` line: the value cannot be known, and refusing a rollback would strand the host |

An absent file whose directory was changed after the boot is treated as possibly
pending too, not as "no configuration": the file may have existed when launchd
booted and been removed since, which leaves the `PATH` launchd applied unknown.

Known limit: both times are on the calendar clock, so a clock step right after the
boot moves `kern.boottime` but not older file times, and a write inside the margin
is the only protection against it.

On Linux a job gets the environment below; the journal does not change it. The
manager's `Environment` is the base. The install replaces the unit file itself and
keeps its drop-ins (`~/.config/systemd/user/<unit>.d/*.conf`), so only what a
drop-in gives counts; a setting only the unit file holds is replaced with it. The
CLI reads the drop-in files as systemd does (byte order mark dropped, lines ended
by `\n`, `\r` or NUL, `[Service]` only; a run of backslashes at the end of a line
continues it only when its length is odd, a comment line ending in a backslash
swallows nothing, and an escaped backslash is one backslash), once per judgement,
and takes the value from the file and checks it against the manager's merged
property for a loaded unit; a mismatch is refused.

| Setting | `PATH` | Store variable the allowlist selects | Bootstrap key (`HOME`, `XDG_*`) |
|---|---|---|---|
| Drop-in `Environment=NAME=value` | the last drop-in's value replaces the recorded (or the manager's) `PATH`; a warning names the drop-in | replaces the manager's value | refused, naming the key and the drop-in |
| Drop-in `Environment=` with no value (reset) | refused (it removes the `PATH` and the bootstrap keys the unit file gives the job) | refused | refused |
| `EnvironmentFile=` assigning the name | refused (quoting is not re-parsed) | refused: move it to an `Environment=` line of a drop-in | refused |
| `PassEnvironment=NAME` | no effect on a recorded `PATH`; refused for an unrecorded one | refused unless a drop-in `Environment=` assigns it (the value would be the manager process's own) | no effect |
| `UnsetEnvironment=NAME` | refused | the variable is absent; `NAME=VALUE` removes it only when the final value equals `VALUE`; a name only the unit file unsets is ignored | refused |
| A line in a form not read (escape sequence, unclosed quote, specifier, bytes that are not UTF-8) that may touch the name | refused | refused (whether or not the name is set now) | refused |
| `Environment=` set only by the (replaced) unit file | ignored | ignored | n/a |
| `EnvironmentFile=` only the (replaced) unit file holds | judged from the manager's merged list, refused when it sets `PATH`: for a recorded `PATH` only when the unit has drop-ins, for an unrecorded one always | ignored | ignored |

An unreachable bus, unreadable unit properties, a masked or failed unit, or a
snapshot made stale by a change on disk next to drop-ins (run `systemctl --user
daemon-reload`) is refused for a recorded `PATH` as for an unrecorded one. A unit
with no unit file (not installed yet, or removed by a rollback or a config
migration) is judged from its drop-in files by the rules below, so a recovery that
removes the unit still completes.

Whatever the CLI cannot attribute or read is refused with the same code, and the
message names the file or directive to change:
- an `EnvironmentFile=` that sets `PATH`, uses quoting the check does not follow,
  is a wildcard, or is a required file that cannot be read as a regular file of at
  most 1 MiB;
- `PassEnvironment=PATH` or `UnsetEnvironment=PATH`, and a masked or broken unit;
- the drop-ins of a loaded unit that a removed unit could not resolve (below): a
  rollback or config migration removes the unit and resolves again from its
  drop-ins alone, so the first resolution refuses upfront, before anything is
  stopped, what the second could not read or resolves to a different `PATH`
  (the drop-in files on disk are the same in both). The manager expands a loaded
  unit's specifiers itself, but for a unit it has not loaded their values cannot
  be read, so no specifier (`%h`, `%t`, `%%`, ...) is expanded there and a
  drop-in `Environment=PATH=`, `ExecSearchPath=` or `EnvironmentFile=` that uses
  one is refused with its key and the specifier named; write the value out;
- a unit with no unit file (not installed yet, or removed by a rollback or a
  config migration) whose drop-in files in the manager's own `UnitPath`
  directories hold a `PATH`-affecting line the CLI does not read: an
  `Environment=` `PATH`, `ExecSearchPath=` (more than one word, a relative entry)
  or `EnvironmentFile=` with an escape sequence or specifier, `PassEnvironment=`
  or `UnsetEnvironment=` naming `PATH` (matched by the exact unquoted name, so
  `PYTHONPATH` is not `PATH`; a `PATH=` word, a specifier and an escape sequence count as naming it), or a file name held by directories of different
  names (`<unit>.d` and `service.d`, say). Of a file name held by same-named
  directories in several `UnitPath` entries the higher-priority one applies, and
  a link to `/dev/null` that wins disables the name. Plain `Environment=PATH=`,
  colon-separated `ExecSearchPath=` and `EnvironmentFile=` lines (checked like a
  loaded unit's) of those drop-ins are read in file-name order across the
  directories (an empty assignment resets; hidden and backup files such as
  `.name.conf` are skipped, a relative `EnvironmentFile=` is ignored as systemd
  does) and give the job's `PATH`, so a
  rollback or migration that removed the unit still completes. Every construct
  the reader follows is one a real-manager test pins (file-name order across
  directories, same-name priority and `/dev/null` masks, hidden files, `[service]`
  spelling, a byte order mark at the start and CR/CRLF/NUL line ends, comment lines,
  quoted and no-break-space `Environment=` values, resets, a spaced or relative
  `EnvironmentFile=`, `EnvironmentFile=` contents, normalized `ExecSearchPath=`
  entries, and the odd/even rule for a run of backslashes at the end of a line); anything else is refused with a named reason: a
  non-normalized absolute `EnvironmentFile=` path, an empty, relative or
  non-normalized or over-long `ExecSearchPath=` entry or one with quotes, a
  section header that is never closed (a form feed is not white space there), a
  byte order mark after the start of the file, a line with a Unicode
  noncharacter, escape sequences, specifiers, an unclosed quote. A drop-in a loaded
  unit's manager lists as a link to `/dev/null` is disabled, not refused;
- a snapshot made stale by a change on disk while any drop-in exists (run
  `systemctl --user daemon-reload`); a drop-in directory that cannot be read;
- an `ExecSearchPath=` that the unit file and a drop-in both feed (the directive
  appends, so the merged list cannot be split into the part the install keeps);
- a unit file that also sets the value, with a drop-in `Environment=` or
  `ExecSearchPath=` line that may set it in a form the CLI does not read (an
  escape sequence, a specifier systemd drops, bytes that are not UTF-8, a quote
  never closed), or with no recognizable drop-in at all.

A journal written by an older CLI, or one whose job was not loaded when the upgrade
started, has no journaled default; see "A job that is not loaded and has no journaled
default" for what is judged then. A journal with the fields is read by
older CLIs as an unknown field, so an older CLI refuses it; roll it back with the
CLI that wrote it or with this release.

## Move Claude and Codex sessions to the official package before upgrading

The upgrade installs and pins the official packages itself (see above), so no
manual step is needed before `pohunek service upgrade`. The steps below are the
manual path to the same state, for a host that cannot run the upgrade's runtime
step or that must move its sessions before the upgrade.

A session records the runtime source it was launched from: a built-in runtime
compiled into the daemon, or the digest of an installed runtime package.
Resume and fork relaunch exactly that source. A release that does not serve it
and has no installed package adopting it adopts the live worker but reports the
session as `would lose recovery` with `runtime_source_not_served`. To keep native
recovery across an upgrade to a release that does not serve the built-in Claude
or Codex runtime, install the official package with the release that is running,
before upgrading. The installed package adopts the built-in sessions, and
the preflight reports them as `adoptable`:

1. Install the official package from the running release's daemon archive
   (v0.34.2 or later ships it under `runtime/`), authorized by the archive's
   signed catalog:

   ```bash
   pohunek plugin install runtime/packages/claude.tar.zst --catalog runtime/runtime-catalog.json --yes
   ```

   Use `runtime/packages/codex.tar.zst` for Codex. A catalog past its
   `expires_at` is refused; install from a newer archive instead.
2. Pin every host profile with that base: `pohunek plugin profile list` shows
   the ones in `needs_migration`, and `pohunek plugin profile migrate <name>
   --yes` pins each to the selected package digest.
3. Start new sessions for the work. They are pinned to the package digest.
   Sessions launched from the built-in are adopted by the installed package,
   live or stopped, and keep resume and fork through it when the package honors
   their recovery contract. Finish the work in a session the package cannot
   honor before the upgrade, or accept losing its native recovery.

   A release that compiles none of Codex, Claude Code and Hermes in (every
   release after 0.34.x) serves `codex`, `claude` and `hermes` only from their
   packages. A live Codex, Claude or Hermes session launched from the built-in
   without the installed package is refused every interaction with it (input,
   attach, resize, rename, metadata), whether or not it could resume
   (`runtime_builtin_removed`; the preflight reports it at risk and its detail
   says so). Installing the package in step 1 keeps such a session usable after
   the upgrade, and resume and fork run through the package when it honors the
   recovery contract. No release
   up to 0.34.x ships the Hermes package, so step 1 has no Hermes package to
   install: finish or stop built-in Hermes sessions before the upgrade, or
   accept the loss (`--accept-runtime-loss`) and install the Hermes package
   from the new release's archive afterwards to finish their work in the live
   session. On a host that keeps no Codex or Claude Code, the hooks pohunek
   installed into `~/.codex` or `~/.claude` stay removable after the upgrade
   with `pohunek integration uninstall --agent codex` or
   `pohunek integration uninstall --agent claude`, which need no package.
   After the upgrade, install the hooks again only after the package:
   `pohunek integration install --agent claude` (or `codex`) is refused while
   no package serves the runtime.
4. Run `pohunek service check` with the new release's binaries: the sessions
   launched from the package are `adoptable`.

A later `service upgrade` installs the newer release's package. It repins the
profiles pinned to an older version of it, and stopped sessions move to the
newer version on their next resume or fork (see "Official runtime packages
during an upgrade"). Outside an upgrade, `pohunek plugin update <package>
<archive> --catalog <catalog> --yes` with a newer release's archive installs and
selects its version for new launches. A profile moves to it only through
`plugin profile migrate`. `plugin update` refuses a lower version than the
selected one (`plugin_update_downgrade`) and a version that cannot be ordered
(`plugin_update_version_unordered`); `plugin select` is the explicit
rollback. The release upgrade test
(`scripts/upgrade-test`, `crates/cli/tests/release_upgrade.rs`) runs the one-step
path for Claude and Codex from the previous release to the build under test.

## Hook stderr capture from 0.35

From 0.35 a failing `pre-create` or `post-create` hook's stderr tail appears in
the `session.new` warning, so the hook's stderr is a pipe the daemon drains
instead of `/dev/null` (`hooks.stderr_tail_bytes`, default 4096). A process a
hook leaves running in the background with its stderr inherited (`npm run dev &`,
also `nohup cmd &`, since `nohup` redirects only a terminal) keeps running while
the daemon does. Two seconds after the hook ended, the daemon hands that stderr
to a drain process (`/bin/cat` into `/dev/null`), so the process keeps running
across a later daemon restart on macOS or without a service manager. Only a
restart within those two seconds still gives it `SIGPIPE` on its next stderr
write. If the drain cannot start, the daemon log names the hook:

```text
WARN ... a background process still holds the hook's stderr and no drain process could take it; redirect its stderr or set hooks.stderr_tail_bytes = 0, or it gets SIGPIPE after a daemon restart event=post-create script=...
```

Then redirect that process's stderr in the hook (`>/dev/null 2>&1` or a log
file), or set `hooks.stderr_tail_bytes = 0` in `service.toml` to keep every
hook's stderr on `/dev/null`. Under systemd the daemon restart already ends
such processes.

## Claude profiles with a detection manifest override

A Claude package waits for the prompt box before it writes a session's initial
input (`input.ready = "screen_idle"`, evidence from an idle rule on a screen
region). A host profile over Claude whose `manifest` override has no such rule,
such as `rules = []` or only working and blocked rules, keeps loading when
upgrading from 0.34.2, and its sessions keep the bracketed-paste readiness of
that release: the initial input is written once bracketed paste is on, and
Claude may drop it. `pohunek doctor` lists each such profile as a warning
(`agent_profile_<name>`). `pohunek service check` and the upgrade's preflight
list it too, under `profile_warnings` with the code
`profile_without_screen_readiness`, judged as the new release loads it: the
planned profile text when the release plan pins the profile, else the profile on
disk, over the planned package when its pin names a digest the release installs. That warning never blocks the upgrade and needs no `--accept-runtime-loss`.
To get prompt-box readiness, add an idle rule on the `prompt_box_body` region to
the override, copying the package's `live_prompt_box` rule.

## Unknown keys in `service.toml`

`service.toml` is tolerant of keys this version does not know (#1401). A typo
or a setting written by a newer release is ignored, not fatal:

- The daemon logs one `service_config.unknown_key` warning per key at start,
  with the key path, line and file, never the value.
- `pohunek service status`, the `warnings` of `pohunek service check` and
  `pohunek service upgrade`, and the `service_config_unknown_keys` check of
  `pohunek doctor` list them with a "did you mean" hint where a known key is
  close. Fix a typo by renaming the key; a typo of a required key still fails
  the load, naming the key it was probably meant to be.
- An upgrade, a reinstall and a same-schema rollback keep the unknown keys of
  the tolerant tables and sections at their paths, and say so in a warning.
  A migration rollback restores the exact pre-upgrade bytes as before.
- An unknown key in `[namespace]` or `[environment]` is still refused, naming
  the key: an older binary must not ignore a setting that moves a root or
  narrows the environment. Remove the key, or install the release that
  understands it.

`schema_version` changes only for a breaking change: a removed or renamed key,
a changed meaning or unit, or validation that rejects a file that was valid
before. An additive optional key does not change it and is omitted from the
file while it has its default, so rolling back to the previous release after
an upgrade leaves a file the previous release reads, unless the operator set
the new key. A previous release that is still strict refuses a file with any
unknown key, so remove hand-added keys before a rollback to a release older
than #1401.

## Configuration migration from schemas 2, 3, 4, 5 and 6

When the installed `service.toml` was written by release 0.33.x (schema 2, no
`[input]` or `[waits]` table), by a release with schema 3 (no `[waits]`
table), by a release with schema 4 (no `input.write_stall_timeout_ms`), by a
release with schema 5 (no `[hooks]` table), or by a release with schema 6 (no
`[events]` table) and the target version differs, `pohunek service upgrade` turns into a
configuration migration and the `--json` reports carry the source schema,
`config_schema_migration: 2`, `3`, `4`, `5` or `6`. `service check` verifies a pending migration's
backup and reports the migration it would perform without writing anything,
so an archive updater sees it before any effect. A same-version refresh runs
no migration and reports `null`. The migration assigns the documented
defaults of the missing keys and tables explicitly: `[input]` 5000 ms and
150 ms for schema 2, `input.write_stall_timeout_ms` 3000 ms for schemas 2 to 4,
`[waits]` 300000 ms and 1800000 ms for schemas 2 and 3, and
`[hooks] stderr_tail_bytes` 4096 for schemas 2 to 5, and `[events]`
(`max_file_bytes` 33554432, `max_files` 8, `max_age_days` 14) for schemas 2 to
6. The first daemon start under the new schema removes an `events.jsonl` above
`max_file_bytes` without reading it, a 13 GB journal included, and logs one
WARN with the path and removed size; the history in that file is not kept.
A schema-6 file written by a build with tolerant unknown keys is still read in
its exact shape, so remove hand-added keys from it before the upgrade. Every
value the file
records, including tuned `[input]` and `[waits]` timings of a schema-3,
schema-4 or schema-5 file, is kept.

The ordering follows the schema boundary. The transaction first saves the
exact pre-upgrade bytes owner-private next to its record and journals their
SHA-256 digest; the daemon job is then stopped before `service.toml` is
rewritten, because a previous daemon's strict reader refuses a restart against
the new file and its restart policy would crash-loop it. The new daemon is
registered after the write, with the still-live session preflight judged
while the old daemon runs. A live session keeps its worker PTY through the
stopped daemon: the workers run in jobs of their own.

An interrupted migration resumes from its record after re-verifying the
digest: while the transaction has not rewritten `service.toml` yet, the file
itself must still be the exact pre-upgrade bytes; a stray file where the
backup will be written is harmless, the transaction rewrites it from the
verified bytes. A failing step rolls the migration back: the exact backup is
verified before any daemon is stopped, and the previous release's store reader
and live-worker adoption are checked whenever rollback must restart it. A
registered new daemon is then stopped, the store is checked again, the exact
pre-upgrade bytes are restored, and only then does the previous daemon restart
against its own file. An unreadable store refuses rollback even with
`--accept-runtime-loss`. A missing or altered backup refuses both the resume and the
rollback closed with `service_config_backup_invalid`: nothing is written, no
daemon restarts, and the record stays for a repaired retry. The recovery is
to restore both files to the bytes the record digests, or to run
`pohunek service uninstall`, which checks live sessions before removing the
installation.

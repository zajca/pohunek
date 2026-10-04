---
type: Runbook
id: runbook/debug-daemon
title: Debug daemon availability
description: Diagnose cases where the CLI cannot reach the local or remote Pohunek daemon.
source_kind: manual
intents: [debug, setup, help]
since: 0.3.3
---

# Debug Daemon Availability

Use this runbook when commands report that the daemon is unreachable or unhealthy.

1. Run `pohunek doctor --json` for local environment checks. On macOS this
   includes `runtime_dir_private`, `socket_path_length`, `worker_executable`,
   `launchd_domain`, `launchd_job`, `filesystem_access` (a Privacy & Security
   denial names the terminal app or the daemon executables to grant under Files
   and Folders; Full Disk Access is not the default remedy), `login_shell`,
   `terminal`, `desktop_notifications` and `keychain`; see
   [local setup](../guides/setup.md#doctor-checks-by-platform). `daemon.doctor`
   returns the same list minus the CLI-only `launchd_job` and current-directory
   probe, evaluated on the host that owns the runtime.
2. Run `pohunek health --json` to query the local daemon.
   If path resolution fails first, inspect `XDG_RUNTIME_DIR`. It must be an
   absolute nonempty path when present. Linux requires it; macOS uses
   `/private/tmp/pohunek-<effective-uid>` only when the variable is absent and
   ignores `TMPDIR` for this decision. An overlong encoded daemon or worker
   socket path is rejected before filesystem mutation.
   If Pohunek reports an unsafe runtime root, inspect its owner, type, exact
   private mode, and every application-owned component. Do not recursively
   delete it or follow a symlink, and do not use broad `chmod` to make a foreign
   entry pass validation.
   The daemon also refuses to start when `HOME` or a forwarded `XDG_*`
   variable is one a worker job definition would reject: every value must be
   an absolute normalized UTF-8 path (no `.` or `..` segments), and `HOME`, the
   workers' working directory, must exist. The fatal error names the variable;
   a relative `HOME` such as `.` fails this way even when every `XDG_*` root is
   absolute.
   It also refuses to start when `<data_dir>/metadata.jsonl` has a schema it
   cannot use: the fatal error names both schema versions. A store newer than
   the daemon needs the newer release (or the `metadata.jsonl.pre-schema-<old>`
   backup restored); see [update after
   release](update-after-release.md#upgrade-window-and-the-metadata-store).
3. If health cannot connect, run `pohunek service status --json`. An installed
   service reports its daemon job (`daemon.state`, `daemon.pid`) and
   `daemon_error` when the service manager cannot be queried. On macOS a
   loaded job without a running process reports `unknown` without a `pid`,
   because launchd does not say whether it has not started yet or has exited;
   check the launchd output files below. A `failed` or `stopped` daemon job is
   restarted by the service manager after its restart
   throttle; its logs are the daemon's JSON log under
   `~/.local/state/pohunek/logs/` and, on macOS, the launchd output files under
   `~/.local/state/pohunek/logs/launchd/`. If `installed` is false, install the
   service with `pohunek service install`. `pohunek daemon start --detach` runs
   the installed daemon by hand and fails with `service_not_installed` when
   `service.toml` is missing; `pohunek daemon start --dev-subprocess --detach` is
   for development only.
4. Run `pohunek health --json` again and inspect the reported socket, version,
   and status.
5. Run `pohunek host governance inspect local --json` to distinguish a healthy
   daemon with a never-enrolled host from unavailable governance state. A
   never-enrolled result has four explicit null fields (`enrollment`, `owner`,
   `owner_revision`, and `quarantine`) plus a stable host ID and safe
   approval-key reference. Do not edit owner-private host-state files or keys
   to change this result.
6. For remote hosts, run `pohunek host inspect <host> --json` and confirm the
   host daemon responds through the remote transport.
   If the remote daemon started before NetBird was ready, allow one retry
   interval for its NetBird-only listener to become available, then repeat the
   inspection and check its logs for `serving control protocol over overlay`.
   A daemon restart is not required for this startup ordering. If a listener
   supervisor exits or panics unexpectedly, the daemon logs
   `remote listener supervisor failed; shutting down daemon` with the overlay
   identifier and exits after controlled cleanup instead of remaining ready
   without that listener.
7. If a session was expected, run `pohunek session list --json` on the relevant
   host and inspect the specific session with `pohunek session inspect <target>`.
8. If the daemon restarted, do not infer session exit from the closed control or
   attach socket. Check the session's `runtime.state`, `worker_id`, and
   `worker_instance_id`, then use the
   [session runtime runbook](debug-session-runtime.md) for `reconnecting`,
   `lost`, `conflict`, or `incompatible`.
9. If `pohunek session new` or `session fork` fails with
   `migration_manifest_missing`, the daemon found legacy resume bindings that
   no migration manifest imported and refuses to write a first session record
   that would make them unimportable. Follow the migration section of the
   [session runtime runbook](debug-session-runtime.md): run
   `pohunek migration preflight` through the worker-aware installer against
   the legacy daemon, and the next daemon start imports the manifest.

For durable notification issues:

1. Run `pohunek notifications list --json` to inspect visible local records.
2. Run `pohunek notifications list --status deleted --json` when a record may
   have been logically deleted.
3. Run `pohunek notifications list --host <host> --json` to query a specific
   remote daemon, or `pohunek notifications list --all-hosts --json` to compare
   local plus reachable hosts.
4. Run `pohunek notifications policy get --json` and confirm the notification
   kind is enabled for the producer provider. Policy is enforced by the daemon
   for hooks, projectors, and any direct `notification.create` caller.
5. Run `pohunek notifications policy set --provider default --kind turn_completed --enabled --json`
   only when intentionally enabling noisy turn-completion records.
6. Run `pohunek notifications retention prune --dry-run --status archived --json`
   before applying retention cleanup. Use `--apply` only after reviewing the
   dry-run ids.
7. Run `pohunek notifications watch --json` in one terminal, then reproduce the
   event. A create should emit `notification_created`; read, acknowledge,
   archive, provider upgrade, or dedupe upgrade should emit
   `notification_updated`; delete should emit `notification_deleted`.
8. If a provider approval prompt does not appear, reinstall hooks with
   `pohunek integration install --agent <codex-or-claude>` and confirm the
   provider build supports the modern hook surface. Codex approval notifications
   require lifecycle `PermissionRequest` hooks; the legacy Codex `notify` key is
   not enough. Claude requires `Notification`, `Stop`, and `StopFailure` hooks.
   Reinstall preserves user hooks unless their command exactly matches a
   Pohunek-managed hook command.
   First inspect both agents with `pohunek integration status --json`, or select
   one with `pohunek --host <name> integration status --agent
   <codex-or-claude> --json`; daemon-backed status honors the effective host.
   For human output, remote recovery commands explicitly name the daemon host;
   run the local-only installer on that machine rather than adding `--host` to
   `integration install`.
   `current` verifies both executable assets with the installer-owned permission
   mode and effective-UID owner, exactly one managed registration under each
   expected provider event, and Codex feature/trust state. Asset type, UID, mode,
   and content come from one no-follow descriptor. The parent chain from the
   selected agent config root through the direct asset parent must contain only
   effective-UID-owned real directories with no group/world write access;
   ancestors above that explicit trust anchor are not inspected. Unsafe asset or
   parent ownership/permissions and duplicate managed registrations are
   `outdated`. A missing Claude `hooks/` child under a trusted config root is a
   reinstallable absence; installation creates it with exact owner-private mode
   `0700` regardless of inherited umask, removes that newly created directory if
   mode enforcement or safe opening fails, and leaves an existing real user
   directory's mode unchanged. Claude
   `settings.json`, Codex `hooks.json`, and Codex `config.toml` are each opened
   no-follow and require effective-UID ownership plus no group/world write bits;
   their metadata and bounded content come from the same descriptor. Codex
   managed trust uses a canonical single-handler group, so a sibling handler is
   drift even when the managed command itself is unchanged. The managed trust
   key set must also be exact. A trust record is installer-owned only when its
   trusted hash is that of a managed command for that event, so a stale managed
   record (a managed hash at an old position) is reinstallable drift and is
   removed during reinstall, while the records of a user's own hooks in the same
   events are never removed or flagged and follow their hooks: a trust key
   embeds the group and handler index, so when install, reinstall, or uninstall
   shifts a user hook's position, its record is re-keyed to the new position
   (its hash stays valid), and two claims on one key fail closed with
   `configuration/integration_trust_conflict` without writing anything (review
   the `[hooks.state]` tables by hand, then repeat the command). A scalar anywhere in the managed trust
   namespace requires configuration repair even when hook drift hides its
   current position. `CLAUDE_CONFIG_DIR` and `CODEX_HOME`
   must resolve to absolute UTF-8 paths, which keeps generated registrations
   independent of the daemon and provider working directories. Follow the typed
   recovery action: `reinstall` means the installer
   can repair every finding, while `repair_configuration` means provider files,
   symlinked, special, foreign-owned, or group/world-writable managed assets or
   parents, invalid registration roots, incompatible TOML table shapes, and
   scalar values anywhere in the installer-owned trust namespace must be
   inspected and fixed first. A missing `hooks` object and an exact owned
   command with drifted handler metadata remain safely reinstallable. Oversized
   and non-regular files are rejected by bounded, nonblocking inspection and
   reported with the applicable recovery.
   `pohunek integration doctor [--agent <codex-or-claude>] [--json]` is the
   read-only diagnosis (it follows `--host`, like status, and exits non-zero
   when any finding is an error). It turns each status warning into a stable
   finding (`asset_missing`, `asset_modified`, `asset_unsafe`,
   `registration_drift`, `provider_config_invalid`,
   `codex_hooks_feature_disabled`, `codex_trust_drift`, `config_root_invalid`)
   with a remediation, reports a present agent without hooks as
   `hooks_not_installed` and an absent optional agent as `agent_not_installed`
   (both informational; only the absence expected of a not-installed
   integration is muted, so an empty config with an unsafe, symlinked, or
   group-writable directory is still an error, exactly where a later install would
   refuse), and checks what status cannot: whether the daemon's
   runtime socket path, and the longest worker socket path, fit the platform
   limit (`hook_socket_path_invalid`), and an informational `python3` note. The
   daemon cannot know the agent's `PATH` or working directory, so the note never
   fails the doctor or changes its exit code: it reports the first executable
   `python3` behind an absolute entry of the daemon's own `PATH`
   (`hook_runtime_python_found`), or that none was found
   (`hook_runtime_missing`), or that it is the macOS stub with no Command Line
   Tools or Xcode behind it (`hook_runtime_macos_shim`, recognized through any
   alias of `/usr/bin/python3`). In every case the hooks run `python3` from the
   agent's own `PATH`, so verify it there (on macOS check `xcode-select -p` and
   install the Command Line Tools if they are missing). Relative and empty
   `PATH` entries are skipped, `python3` is never run, and the note applies only
   to an agent whose hooks are installed.
   `pohunek integration uninstall --agent <codex-or-claude> [--json]` (the agent
   is always named; there is no all-agents removal, so a failure can never leave
   one agent removed and another unreported) removes
   exactly what the installer owns, always on the local daemon host: the exact
   managed commands in `settings.json` or `hooks.json`, the Codex trust records
   in `config.toml` whose hash is that of a managed command (a user's own hook
   records stay), and each managed hook script that is a regular
   file still carrying its `POHUNEK_INTEGRATION_ID` marker line. The
   registration is edited first, so the agent never references a missing
   script. User hooks, other settings, `[features] hooks`, the `hooks/`
   directory, and the lock file are left as they are. A symlink, directory,
   FIFO, or unmarked file at a managed script path is preserved and listed
   under `preserved_paths`; the marker is read from the same inode that is then
   deleted (it is moved aside first and moved back untouched when unmarked), and
   a provider file whose content, mode, or inode changed since it was read is a
   collision. Removal runs under the same lock and rollback as
   install, reports the same `integration_install_in_progress`,
   `integration_destination_collision`, and `integration_recovery_required`
   errors, and is idempotent: with nothing installed (or no config directory)
   it reports `not_installed` and changes nothing.
   Status itself never repairs or rewrites provider configuration. Installation
   validates all existing config and hook parents before any mutation and fails
   unsafe parents with `configuration/integration_path_untrusted`. It performs
   replacements through already-opened directory descriptors with exclusive
   temporary files, explicit modes, and descriptor-relative rename, so a
   concurrent parent-name swap cannot redirect a managed write. Existing safe
   provider-file modes are preserved; new registration files use `0600`.
   An agent whose config directory does not exist is optional and simply not
   installed here: status reports `available: false`, `not_installed`, recovery
   `none`, and no warning. A config path that exists but is not a directory, or
   cannot be resolved or inspected, is a failure (`outdated`,
   `repair_configuration`) with a warning. A symlink at the config path, live or
   dangling, is such a failure (`config_root_invalid` in the doctor), never an
   absent agent, and install and uninstall refuse it.
   Status and doctor validate the config directory with the same read-only,
   descriptor-relative component walk the installer performs: a symlink at any
   component (live or dangling), a foreign owner, or a group/world-writable
   directory or ancestor is `outdated` with `repair_configuration`
   (`config_root_invalid`), for exactly the paths install and uninstall refuse.
   Replaced and removed originals are moved aside atomically into a private
   quarantine name bound to their inode and verified there (identity, mode, and
   content) against what was read, the new file is activated with a no-replace
   rename, and the originals are deleted only after every file is in place. A
   provider file that changed at any moment is a collision and is left intact,
   and so is a file the operation itself wrote that another writer changed
   before the operation finished: every written file is re-verified (inode,
   mode, complete content) before any script is removed and again before the
   operation completes and before any rollback, the other version is kept, the rest is rolled back
   without touching it, and the error names where the original stays quarantined;
   and so is a loaded input the operation leaves unchanged (for example an
   already-current Codex `config.toml`): it is verified again before any
   destructive step and right before the registration step;
   a failed step moves each original back untouched. If the final cleanup cannot
   delete a quarantined original, the install or removal still succeeds and
   lists it in `cleanup_incomplete`; the doctor keeps reporting
   `displaced_original_left_behind` with the paths until you review and delete
   them (or move one back if an install was interrupted).
   Every rollback or cleanup step that leaves data in quarantine (a removal
   refused because the entry changed, a restore that collided, a failed move) is
   reported with the true quarantine path: as `integration_recovery_required`
   while the operation is still rolling back, or as a `cleanup_incomplete` entry
   once it has committed.
   The doctor takes the installer lock without blocking and never creating the
   file, and holds it for its whole (short, read-only) inspection. While another
   install, uninstall, or doctor holds it, the doctor reads and scans nothing and
   reports only the informational `operation_in_progress` finding for that agent
   (still `ok`, with no status); run it again afterwards. An installer that
   arrives during an inspection fails fast with
   `integration_install_in_progress` ("another integration install, uninstall or
   doctor holds ..."), which is safe to repeat. A lock file that is a symlink, a
   directory, has the wrong mode or owner, or was replaced makes every install
   and uninstall fail, so it is the error finding `unsafe_installer_lock`:
   delete it when nothing is running (it is recreated with mode 0600) or repair
   its permissions. If the created `hooks/` directory cannot be removed or moved
   back after a failed install (the provider recreated `hooks/`), the install
   reports `integration_recovery_required` with the quarantine path that holds
   the directory and any files that appeared in it.
   The doctor recognizes the installer's own quarantine names and every
   platform staging, parking, and stale prefix (including `.pohunek-restore-*`).
   Its scan stops after a fixed number of entries per directory, and an
   incomplete or failed scan is itself an error finding
   (`quarantine_scan_incomplete`), never a clean result: list the directory by
   hand for names starting with `.pohunek-`, review them, and remove unrelated
   files so the scan can finish. It finds these entries through the same no-follow trusted walk, only
   in a config root and Claude `hooks/` directory that passed validation, so a
   symlinked root or `hooks/` is never scanned; each directory scan and the
   listing are bounded, and the finding says when the scan stopped early. A
   rollback removes only the exact file the failed step activated (its identity
   is taken from the descriptor that wrote it before the rename), so a file
   another process put in place afterwards is kept and reported with
   `integration_recovery_required`, which also names where the original is
   quarantined. Hints in these errors name the running operation, install or
   uninstall.
   Installation is one transaction over the ordered files (hook scripts first,
   provider registration last) under an exclusive lock file
   `.pohunek-integration.lock` (mode `0600`) inside the agent config directory,
   so installers for the same home or profile never interleave and installers
   for different profiles never contend. Failures are distinct and non-secret:
   `runtime/integration_install_in_progress` means another installer holds the
   lock (retry once it finishes); `runtime/integration_destination_collision`
   means a provider file changed between the installer's read and its write,
   and nothing was overwritten (rerun the install); `configuration/integration_path_untrusted`
   means an unsafe path shape (symlinked config root, directory, FIFO, wrong
   owner, or group/world-writable parent), and the foreign entry is left as it
   was. Every file already committed is restored from a snapshot when a later
   step fails, including a displaced managed symlink, and a `hooks/` directory the
   install created is removed again when the rollback leaves it empty. When that rollback itself
   cannot restore a file, the error is
   `runtime/integration_recovery_required` and names each unrestored path: fix
   those files by hand, then rerun `pohunek integration install`. Managed hook
   scripts are fully owned by the installer, so a modified or oversized script
   is reinstall-repairable drift, while provider files are merged and never
   clobbered. A script too large to snapshot is moved aside under a
   `.pohunek-integration-displaced-*` name while the new one is written, so a
   failed install puts back the exact original inode, and an interrupted
   rollback leaves that quarantined original on disk and reports
   `integration_recovery_required`. Uninstall never deletes a script it cannot
   read and verify as owned; it lists it under `preserved_paths`.
   On macOS the same checks apply. The installer never follows a symlink in the
   config root path, so a `CLAUDE_CONFIG_DIR` or `CODEX_HOME` that goes through
   a symlink (a dotfile-managed `~/.claude`, or a path under `/var`, which is a
   symlink to `/private/var`) fails with `integration_path_untrusted`: point the
   variable at the canonical directory (`realpath`). Hooks connect to the
   socket path the daemon injected in `POHUNEK_SOCKET_PATH`; that path is
   resolved and length-checked by the daemon (103 bytes on Darwin). Hooks stay
   silent by design, so when a macOS session reports nothing, first check that
   the daemon started with a short enough `XDG_RUNTIME_DIR` (an overlong worker
   socket fails with `worker_socket_path_invalid`) before suspecting the hook.
   Parse errors in provider TOML are reported by position and message only,
   never by quoting the offending line.
   Status itself never repairs or rewrites provider configuration. Installation
   validates all existing config and hook parents before any mutation and fails
   unsafe parents with `configuration/integration_path_untrusted`. It performs
   replacements through already-opened directory descriptors with exclusive
   temporary files, explicit modes, and descriptor-relative rename, so a
   concurrent parent-name swap cannot redirect a managed write. Existing safe
   provider-file modes are preserved; new registration files use `0600`.
9. If a notification is missing its session link, inspect the hook environment
   setup. Hook adapters silently drop an invalid `POHUNEK_SESSION_ID` and still
   create the notification without linkage.
10. If duplicate attention notifications appear, compare each record's
    `dedupe_key`, `source.provider`, `source.provider_event`, and `created_at`.
    Session attention dedupe uses `attention:<session_id>` and only applies
    inside the policy's `attention_dedupe_window_secs`.
11. If stale `agent_blocked`, `approval_required`, or `turn_completed` records
    stay `unread` after the agent already resumed, check that the daemon still
    observes the session returning to `working` activity. Session notifications
    auto-resolve to `acknowledged` when the projector sees the transition into
    `working`, keyed by `attention:<session_id>` and `turn:<session_id>`, so
    hook- and projector-produced records are cleared together. A record that
    never self-resolves usually means no `working` activity edge reached the
    projector (for example a session whose activity is not being reported).
12. If repeated `turn_completed` rows appear for one session, inspect their
    `dedupe_key`. Modern hooks send `turn:<session_id>` for `Stop`; a newer
    unread turn supersedes older unread turns for the same key, and a visible
    attention record supersedes the unread turn twin. Missing `turn:` keys mean
    the host likely needs `pohunek integration install` so managed hooks refresh.
13. If an expected `agent_blocked`, `approval_required`, or `turn_completed`
    notification does not (yet) show up in `pohunek notifications list --json`
    or `pohunek notifications watch --json`, it may simply be debounced: the
    daemon holds session creates pending in memory for `attention_debounce_secs`
    (5 seconds by default, see `pohunek notifications policy get --json`) before
    committing and emitting `notification_created`. Wait past the configured
    window and re-check; if the session resolved back to `working` inside the
    window, the pending record was dropped and will never appear — that is the
    intended debounce behavior, not a bug. Pending debounced entries are
    in-memory only and are not persisted, so a daemon restart while an entry is
    pending drops that transient signal; this is expected for a sub-10s window
    and is not a data loss bug worth chasing.

The durable store is under the daemon data directory in `notifications/`.
`notifications.jsonl` is append-only record history, while `policy.json` is the
current persisted notification policy. Both are host-local; `--all-hosts`
commands perform client-side fan-out rather than reading a shared store.

Do not treat installed config files as the daemon source of truth. A missing
`attach.conf` or prompt template does not mean the daemon is unhealthy; verify
daemon health first, then run `pohunek setup config` for the missing files.

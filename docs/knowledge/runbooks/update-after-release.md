---
type: Runbook
id: runbook/update-after-release
title: Update after release
description: Reconcile setup assets, host capabilities, projects, and launcher config after updating Pohunek.
source_kind: manual
intents: [update, setup, debug, help]
since: 0.3.3
---

# Update After Release

Use this runbook after replacing an installed Pohunek binary from a component
release archive or rebuilding it from source.

## Current public protocol v3 boundary

The current release supports only protocol `3..=3` and cannot communicate with
protocol-v2 peers. Before replacing any component, inventory every CLI,
web backend/SDK, custom client, and local or NetBird-reachable daemon that must
talk to another peer. Drain cross-host automation, upgrade that complete set in
one maintenance window, and then verify every host with `pohunek health --json`
and `pohunek host inspect <host> --json`. The response must advertise protocol
range `3..=3` for this release.

There is no v2 compatibility shim. Do not downgrade one peer independently: it
will be isolated from v3 peers, and protocol-v3 overlay state is not a v2
rollback mechanism. Restore the coordinated v3 component set instead. The
historical v2 release introduced range negotiation from integer-v1; that
history does not widen the current supported range.

1. Download the component archive for the binary being updated: CLI (`pohunek`),
   daemon (`pohunekd`, `pohunek-sessiond`, and the `pohunek` CLI that installs
   them as a native service), or web backend.
2. Run `pohunek doctor --json` to confirm the current binary can find required
   paths and state directories.
3. Run `pohunek health --json` to confirm the daemon responds with the expected
   version and protocol compatibility.
4. Run `pohunek host inspect local --json` to inspect local runtimes and
   capabilities.
5. Refresh launcher scripts with `pohunek setup scripts`.
6. Review config changes before applying `pohunek setup config --force`; default
   setup config should not overwrite existing files.
7. Reprint or refresh sway integration with `pohunek setup sway --print` or
   `pohunek setup sway`.
8. For important projects, verify `pohunek project show <id-or-label> --json`
   and resolved actions with `pohunek project actions <id-or-label> --json`.

For the Hermes M2 runtime, inspect the `hermes` entry after upgrade. It is
launchable only with `version: "0.20.0"` and `supported: true`; a missing
binary has no version-policy result, while a wrong or unparseable version is
reported as unsupported. The model-free compatibility check, `cargo xtask
hermes compatibility --pohunek-bin ABS`, needs the pinned Hermes executable on
`PATH` and an absolute, canonical built Pohunek executable at `ABS`; it validates
committed evidence. Refresh goldens with `cargo xtask hermes refresh-goldens
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
`pohunek service upgrade --from <archive-dir>`) rather than replacing only `pohunekd`. The upgrade copies the new
binaries into their own `<prefix>/libexec/pohunek/<version>/` directory,
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
offending entry. Make sure nothing runs from it, remove it, and rerun. The
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
2. Inspect that session and confirm the same `worker_id` and `runtime_id`.
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
changes nothing, and `pohunek service uninstall` removes it through the full
session-checked uninstall instead (`--stop-sessions` stops the live sessions).
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
terminal back to the command and continues it. One signal received while the
lock process waits for adopters ends that wait. If the lock process cannot tell whether an adopter still runs, it
removes its record, releases the lock, and fails. `pohunek service check [--prefix <dir>] [--json]`
runs, without changing anything, every check the install (while an install is
pending or nothing is installed) or upgrade of this version makes before its
first effect — `HOME` and the XDG roots, the prefix, every directory it writes,
a pending transaction it would refuse, the recorded installation, the prefix
owner, and, for an install that starts over, a daemon job registered without
`service.toml` (`service_daemon_job_present`) — and fails with the error and
code that command would fail with.
`packaging/install-daemon.sh` runs its whole legacy retirement and the final
`service install|upgrade` under `pohunek service lock -- <installer>`, and runs
`service check` before it touches the legacy install.

`pohunek service uninstall` refuses while sessions are live and lists them.
`--stop-sessions` stops every session through its worker first; the session
store, journals, and host identity are kept unless `--purge` is given. The
prefix ownership record is removed after the prefix's version directories and
CLI copy (it stays while a version is kept for a worker journal or job of this
installation). The transaction record (`service-install.json`) is cleared just before
`service.toml`, which is removed last, so if an uninstall fails partway or is
interrupted, rerunning `pohunek service uninstall` finishes the cleanup. A
pending install that reached its `registering` step is uninstalled through the
same live-session check even when its `service.toml` is already gone: the
record's version and prefix identify the installation.
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
[debug session runtime](debug-session-runtime.md).

When the assistant feature is available, its update intent should use bundle
version metadata and `changed_in` frontmatter to explain version-specific
changes before recommending edits.

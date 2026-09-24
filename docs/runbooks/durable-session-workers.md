# Durable Session Worker Operations

This runbook covers normal daemon restart, worker diagnosis, runtime loss, and
safe administrative actions for worker-backed sessions.

## Process Model

`pohunek service install` installs the daemon as the only login service, and
the daemon starts one native job per worker generation. `<ns>` is the
installation namespace (the `namespace` field of `pohunek service status
--json`): the first 12 hex digits of SHA-256 over the user ID and the canonical
state and runtime roots. `<generation>` is an 8-character lowercase base32
token the daemon mints and persists before it starts the job.

| Job | Linux (systemd user manager, 255 or newer) | macOS (launchd `gui/<uid>`) |
|---|---|---|
| Daemon | `pohunek-<ns>-daemon.service`, `Restart=on-failure` | agent `io.github.zajca.pohunek.<ns>.daemon` in `~/Library/LaunchAgents`, `KeepAlive={SuccessfulExit=false}` with `ThrottleInterval` |
| Worker generation | transient unit `pohunek-<ns>-worker-<session-id>-<generation>.service`, `Restart=no`, `KillMode=control-group`, `SendSIGHUP=yes` | job `io.github.zajca.pohunek.<ns>.worker.<session-id>.<generation>`, `RunAtLoad`, no `KeepAlive`, `AbandonProcessGroup=false`, `ExitTimeOut` from `service.toml` |
| Accounting | slice `pohunek-<ns>-sessions.slice` (`MemoryAccounting`, `TasksAccounting`) | open-file limit from `service.toml` on every job |

Worker jobs have no unit file or `LaunchAgents` entry. On Linux they exist only
in the running user manager. On macOS their definitions live in
`~/.local/state/pohunek/launchd/` (directory `0700`, files `0600`) and their
stdout/stderr in `~/.local/state/pohunek/logs/launchd/`; both are removed when
the generation is retired, and launchd never loads them again at login.

The daemon job has no stop-propagation dependency to workers. Restarting or
killing the daemon closes public and private controller connections but does
not stop a worker or signal its child. Worker jobs have no restart policy: once
a worker exits, restarting it cannot reconstruct the destroyed PTY.

Workers live for the login session. Closing a terminal or locking the screen is
safe. Logging out or rebooting ends every worker job; after the next login the
daemon reports those sessions `lost` with reason `runtime_lost` and never
restarts or resurrects them.

## Verify a Lossless Daemon Restart

Choose a non-critical live session and capture its identities:

```bash
pohunek session inspect s-42 --json
pohunek service status --json
```

Record `runtime.worker_id`, `runtime.runtime_id`, the session root `pid`, and
the session's `workers` entry (`generation` and `pid`). Restart only the control
plane. On Linux:

```bash
systemctl --user restart pohunek-<ns>-daemon.service
```

On macOS:

```bash
launchctl kickstart -k gui/$(id -u)/io.github.zajca.pohunek.<ns>.daemon
```

Then:

```bash
pohunek health --json
pohunek session inspect s-42 --json
pohunek service status --json
```

The health request succeeds only after startup reconciliation. The worker id,
runtime id, root child PID, worker generation, and worker PID must be
unchanged. An existing attach socket closes during restart; reconnecting attach
clients open a new public stream to the same runtime.

## Diagnose Runtime State

Start with the public logical record:

```bash
pohunek session list --json
pohunek session inspect s-42 --json
```

Interpret the runtime independently from the agent lifecycle:

| Runtime state | Meaning | Safe next action |
|---|---|---|
| `starting` | A worker is bootstrapping or initializing | Wait for creation to commit or return a typed failure |
| `live` | The daemon controls the current worker generation | Attach or continue normally |
| `reconnecting` | Reconciliation knows the worker but has not finished adoption; `runtime_supervision_unavailable` means the service manager could not be inspected | Wait; reconciliation retries on its own. Do not recover or restart the worker |
| `terminal` | The worker observed child exit | Inspect the terminal result; acknowledge or recover explicitly when eligible |
| `lost` | The PTY generation no longer exists; `runtime_lost` after its leftover processes were swept, `runtime_lost_cleanup_unconfirmed` when the sweep could not confirm that | Preserve the logical record; use explicit native recovery only when available. After `runtime_lost_cleanup_unconfirmed`, check `ps` for that session's processes first |
| `conflict` | More than one or mismatched runtime identity is present: `runtime_supervision_ambiguous` (job present, worker socket silent, journal not terminal) or `runtime_identity_mismatch` (job definition or process does not match the record) | Preserve evidence; do not kill a worker automatically. After diagnosis, `session rm` may remove only the logical record. |
| `incompatible` | A live worker has no compatible private protocol | Run a compatible daemon; leave the worker alive |

`pohunek service status --json` lists every worker job of the installation
with its `generation`, `state`, `pid`, and the executable and arguments the
backend proved. To inspect the native job itself without mutating it, on Linux:

```bash
systemctl --user status pohunek-<ns>-worker-s-42-<generation>.service
systemctl --user show \
  -p ActiveState -p SubState -p MainPID -p ControlGroup \
  pohunek-<ns>-worker-s-42-<generation>.service
```

On macOS, use only the exit status of `launchctl print`: `0` means loaded and
`113` means absent. The printed text is informational and changes between
macOS releases.

```bash
launchctl print gui/$(id -u)/io.github.zajca.pohunek.<ns>.worker.s-42.<generation>
```

The worker socket is under
`<runtime-root>/workers/<session-id>/control.sock`. A valid explicit
`XDG_RUNTIME_DIR` selects its `pohunek` child; Linux requires that variable,
while macOS without it uses `/private/tmp/pohunek-<effective-uid>`. The journal
is under
`${XDG_STATE_HOME:-$HOME/.local/state}/pohunek/workers/<session-id>/<worker-id>.json`.
Do not remove either path while its job is active. A failed connection does not
prove that unlinking the socket is safe.

Structured daemon and worker logs live under
`${XDG_STATE_HOME:-$HOME/.local/state}/pohunek/logs/`. Lifecycle records include
session, worker, runtime, phase, and outcome identifiers. They intentionally
exclude environment values, input, prompts, raw terminal bytes, data tokens,
controller tokens, and native reference values.

## Safe and Unsafe Actions

Safe control-plane restart: restart the daemon job as shown above.

Intentional session stop:

```bash
pohunek session stop s-42
```

Do not restart a worker job, and do not start one by hand. The old worker's
exit destroys its PTY, while a new process would be a different runtime
generation without a valid recovery transaction. Stopping a worker job
(`systemctl --user stop`, `launchctl bootout`) is a last-resort destructive
action: systemd's `KillMode=control-group` terminates the worker and its managed
descendants; launchd terminates the worker's process group, and descendants
that left that group are only reaped by the daemon's ownership-marker sweep
once it reconciles the lost generation.

Do not:

- unlink a worker socket because the daemon cannot connect;
- delete a journal or a launchd definition while its job is active;
- kill a worker to clear `conflict` or `incompatible`;
- interpret daemon disconnection as child exit;
- invoke `session.resume` for a live or reconnecting runtime;
- edit `worker_id` or `runtime_id` in metadata by hand.

After preserving diagnostic evidence, `pohunek session rm <id>` can remove a
`lost`, `conflict`, or `incompatible` logical record. It does not stop or signal
an unavailable runtime; an ambiguous worker remains an operator responsibility.

## Runtime Loss and Explicit Recovery

Worker crash, worker `SIGKILL`, logout, host reboot, user-manager shutdown, or
power loss destroys the live PTY generation. The daemon retains the logical
session and reports `runtime.state=lost`; it never starts provider-native resume
during reconciliation. When the worker's job ended while its journal still said
live and the journal's worker process is proven gone, reconciliation first
sends `SIGTERM`, then after the configured `[sweep] grace_ms` `SIGKILL`, to every
same-user process that carries exactly that generation's `POHUNEK_RUNTIME_ID`
ownership marker, checking each process's start identity before each signal.
It retires the ended job and its definition, then reports `runtime_lost`.
Uncertain evidence kills nothing.

If the immutable launch agent has a valid native recovery reference, the
operator may call `session.resume`. Recovery preserves the logical session id,
name, creation time, metadata, project, and worktree, but starts a new worker
generation (a new native job), runtime id, PTY, child PID, and provider process.
The old generation must be proven ended first; two generations of one session
are never live at once. Clients receive
`session_native_recovered` and must present the generation change visibly.
Shell sessions and uncaptured agent sessions cannot be reconstructed.

## Upgrade and Rollback

Upgrade with `pohunek service upgrade` (the daemon archive's
`packaging/install-daemon.sh` runs it). Each version lives in its own
`<prefix>/libexec/pohunek/<version>/` directory; worker jobs reference the
absolute versioned `pohunek-sessiond` and journal it, so an upgrade switches
`active_version` in `service.toml`, rewrites the daemon job to the new version,
and restarts only the daemon. Running workers keep their PID, PTY, and child.
Version directories still referenced by a non-final worker journal or a running
process are kept; `pohunek service status --json` shows which journals and
processes reference each version.

Worker-aware releases negotiate the current and immediately preceding private
worker protocol. An unsupported worker remains alive as `incompatible`. Do not
force a worker restart as a compatibility shortcut.

When an older compatible worker lacks a newer attach feature, the daemon falls
back to that worker protocol's bounded replay attach path. Snapshot-first
terminal restoration is an enhancement, never a condition for reaching a live
PTY after a daemon upgrade.

Worker-internal fixes apply only to worker generations started after the
upgrade. Already-running workers continue executing their own versioned binary. Replace a live session only through an explicit safe
operator workflow; never restart its worker merely to pick up an update.

The first worker-aware release is a separate boundary from the normal N/N-1
window. Follow
[the migration guide](../migrations/durable-session-workers.md) before replacing
a legacy daemon.

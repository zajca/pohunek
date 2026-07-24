# Durable Session Worker Operations

This runbook covers normal daemon restart, worker diagnosis, runtime loss, and
safe administrative actions for worker-backed sessions.

## Process Model

The installed user manager owns these sibling units:

- `pohunekd.service`: restartable public control plane and logical-session
  authority;
- `pohunek-session@<session-id>.service`: one PTY runtime owner per live
  session;
- `pohunek-sessions.slice`: resource accounting for worker units.

The daemon service has no stop-propagation dependency to workers. Restarting or
killing the daemon closes public and private controller connections but does
not stop a worker or signal its child. Worker units use `Restart=no`: once a
worker exits, restarting it cannot reconstruct the destroyed PTY.

## Verify a Lossless Daemon Restart

Choose a non-critical live session and capture its identities:

```bash
pohunek session inspect s-42 --json
systemctl --user show \
  -p ActiveState -p SubState -p MainPID \
  pohunek-session@s-42.service
```

Record `runtime.worker_id`, `runtime.runtime_id`, the session root `pid`, and
the worker `MainPID`. Restart only the control plane:

```bash
systemctl --user restart pohunekd.service
pohunek health --json
pohunek session inspect s-42 --json
```

The health request succeeds only after startup reconciliation. The worker id,
runtime id, root child PID, and worker `MainPID` must be unchanged. An existing
attach socket closes during restart; reconnecting attach clients open a new
public stream to the same runtime.

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
| `reconnecting` | Reconciliation knows the worker but has not finished adoption | Wait; do not recover or restart the worker |
| `terminal` | The worker observed child exit | Inspect the terminal result; acknowledge or recover explicitly when eligible |
| `lost` | The PTY generation no longer exists | Preserve the logical record; use explicit native recovery only when available |
| `conflict` | More than one or mismatched runtime identity is present | Preserve all evidence and resolve manually; do not kill automatically |
| `incompatible` | A live worker has no compatible private protocol | Run a compatible daemon; leave the worker alive |

Inspect the matching systemd unit without mutating it:

```bash
systemctl --user status pohunek-session@s-42.service
systemctl --user show \
  -p ActiveState -p SubState -p MainPID -p ControlGroup \
  pohunek-session@s-42.service
```

The worker socket is under
`$XDG_RUNTIME_DIR/pohunek/workers/<session-id>/control.sock`. The journal is
under
`${XDG_STATE_HOME:-$HOME/.local/state}/pohunek/workers/<session-id>/<worker-id>.json`.
Do not remove either path while a unit is active. A failed connection does not
prove that unlinking the socket is safe.

Structured daemon and worker logs live under
`${XDG_STATE_HOME:-$HOME/.local/state}/pohunek/logs/`. Lifecycle records include
session, worker, runtime, phase, and outcome identifiers. They intentionally
exclude environment values, input, prompts, raw terminal bytes, data tokens,
controller tokens, and native reference values.

## Safe and Unsafe Actions

Safe control-plane restart:

```bash
systemctl --user restart pohunekd.service
```

Intentional session stop:

```bash
pohunek session stop s-42
```

Do not use `systemctl --user restart pohunek-session@s-42.service`. The old
worker's exit destroys its PTY, while the new process would be a different
runtime generation without a valid recovery transaction. An administrative
`systemctl --user stop` is a last-resort destructive action because
`KillMode=control-group` terminates the worker and its managed descendants.

Do not:

- unlink a worker socket because the daemon cannot connect;
- delete a journal while its unit is active;
- kill a worker to clear `conflict` or `incompatible`;
- interpret daemon disconnection as child exit;
- invoke `session.resume` for a live or reconnecting runtime;
- edit `worker_id` or `runtime_id` in metadata by hand.

## Runtime Loss and Explicit Recovery

Worker crash, worker `SIGKILL`, host reboot, user-manager shutdown, or power
loss destroys the live PTY generation. The daemon retains the logical session
and reports `runtime.state=lost`; it never starts provider-native resume during
reconciliation.

If the immutable launch agent has a valid native recovery reference, the
operator may call `session.resume`. Recovery preserves the logical session id,
name, creation time, metadata, project, and worktree, but creates a new worker,
runtime id, PTY, child PID, and provider process. Clients receive
`session_native_recovered` and must present the generation change visibly.
Shell sessions and uncaptured agent sessions cannot be reconstructed.

## Upgrade and Rollback

Install the complete daemon archive so `pohunekd`, `pohunek-sessiond`, and all
three unit definitions remain version-coherent. Updating the worker binary on
disk does not change workers already mapped in memory. The installer reloads
unit definitions and restarts only `pohunekd.service`.

Worker-aware releases negotiate the current and immediately preceding private
worker protocol. An unsupported worker remains alive as `incompatible`. Do not
force a worker restart as a compatibility shortcut.

The first worker-aware release is a separate boundary from the normal N/N-1
window. Follow
[the migration guide](../migrations/durable-session-workers.md) before replacing
a legacy daemon.

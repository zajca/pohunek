---
type: Runbook
id: runbook/debug-session-runtime
title: Debug a durable session runtime
description: Diagnose worker reconnection, runtime loss, identity conflicts, and first worker-aware migration.
source_kind: manual
intents: [debug, setup, update, help]
since: 0.19.0
---

# Debug a Durable Session Runtime

Use this runbook when a session survives in the list but cannot attach, when a
daemon restart closed an attach terminal, or when an update reports a worker
problem.

Start with public, non-destructive inspection:

1. Run `pohunek health --json` and wait for the replacement daemon to become
   ready. Readiness follows worker discovery and reconciliation.
2. Run `pohunek session inspect <target> --json`.
3. Record `runtime.state`, `runtime.worker_id`, `runtime.runtime_id`,
   `runtime.last_connected_at`, and `runtime.loss_reason`.
4. On the session's host, inspect the unit without changing it:
   `systemctl --user status pohunek-session@<session-id>.service` and
   `systemctl --user show -p ActiveState -p SubState -p MainPID
   pohunek-session@<session-id>.service`.
5. Inspect daemon and worker structured logs under
   `~/.local/state/pohunek/logs/`. Do not copy prompt, input, or raw terminal
   content into reports. `pohunekd.jsonl` plus seven rotations retain at most
   256 MiB; `pohunek-session-<session-id>.jsonl` plus three rotations retain at
   most 16 MiB across all worker generations for that session.

Interpret runtime states as follows:

- `live`: the daemon has the current worker controller lease. Attach should use
  the existing runtime.
- `reconnecting`: a known worker is still being validated or adopted. Do not
  start native recovery or restart the worker.
- `terminal`: the worker observed child exit and the logical outcome is being
  retained or has been imported.
- `lost`: no live PTY generation remains. The logical record is intentionally
  retained; explicit native recovery is possible only with a valid launch
  recovery reference.
- `conflict`: multiple or mismatched identities claim the session. Do not stop,
  unlink, or kill either candidate automatically. Preserve the unit, journal,
  and socket evidence for diagnosis. Afterward, `pohunek session rm <id>` can
  remove only the quarantined logical record; it does not signal a worker.
- `incompatible`: the worker is alive but private protocol negotiation failed.
  Leave it alive and use a compatible daemon release.

Restarting `pohunekd.service` is safe for workers, but it closes current public
connections:

```bash
systemctl --user restart pohunekd.service
```

After health returns, the same `worker_id`, `runtime_id`, worker `MainPID`, and
agent child PID demonstrate reconnection. A new runtime id means explicit
recovery or a defect; it is not normal daemon restart behavior.

Do not run `systemctl --user restart pohunek-session@<id>.service`. Worker units
use `Restart=no` because once a worker exits its PTY cannot be recreated. An
administrative worker stop kills that unit's process group and therefore loses
the runtime generation. Use `pohunek session stop <session-id>` for an
intentional session stop.

For the first worker-aware installation, let all legacy sessions finish or stop
them explicitly. The archive installer lists live sessions that lack durable
`runtime` metadata and refuses replacement. Sessions with a runtime binding are
already worker-owned, are excluded from this one-time guard, and survive the
daemon restart. `packaging/install-daemon.sh --accept-runtime-loss` is
destructive consent: existing legacy PTYs cannot be transferred into workers.
Use it only after recording the affected ids and accepting that shell and
uncaptured agent sessions cannot be reconstructed.

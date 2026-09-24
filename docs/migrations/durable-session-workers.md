# Migration to Durable Session Workers

This guide covers the first upgrade from a legacy daemon-owned PTY release to a
worker-aware release.

## Why the Boundary Is Destructive

A legacy `pohunekd` owns every PTY master. Once that daemon exits, the new
daemon cannot recover those descriptors, reader state, child handle, terminal
tracker, or output buffer. Existing native agent metadata can start a different
provider process later, but it cannot preserve the same PTY, PID, shell, or
in-flight terminal state.

The first worker-aware installation therefore fails closed when the legacy
daemon exposes live sessions without durable runtime metadata. Sessions that
already report a `runtime` binding are worker-owned and are excluded from this
one-time guard. This limitation applies only to the boundary migration.
Sessions created by a worker-aware release subsequently survive daemon restart,
daemon crash, and daemon binary upgrade with the same worker, PTY, child PID,
and runtime id.

## Preferred Migration

1. While the legacy daemon is still running, inventory every session:

   ```bash
   pohunek session list --json
   pohunek session inspect <session-id> --json
   ```

2. Record which sessions are `starting` or `running` without a `runtime`
   binding. Note which agent sessions have a valid native resume reference.
   Shell sessions and uncaptured agents cannot be reconstructed after the
   boundary. Existing worker-bound sessions are not legacy migration targets.
3. Let live work finish, or stop each session intentionally through
   `pohunek session stop <session-id>`.
4. Repeat `pohunek session list --json` and require zero `starting` or `running`
   legacy sessions.
5. Back up the owner-private pohunek data and state directories according to
   the host's normal backup policy. Do not copy raw terminal content into an
   issue or shared log.
6. Unpack the complete daemon archive and run its installer:

   ```bash
   ./packaging/install-daemon.sh
   ```

7. Verify the installed service and daemon readiness:

   ```bash
   pohunek service status --json
   pohunek health --json
   ```

8. Create a disposable shell session and verify that restarting the daemon job
   (`systemctl --user restart pohunek-<ns>-daemon.service` on Linux) preserves
   its `worker_id`, `runtime_id`, root child PID, and the worker generation and
   PID reported by `pohunek service status --json`.

The installer runs `pohunek service install` (or `pohunek service upgrade` for
an existing service): it copies the binaries into
`<prefix>/libexec/pohunek/<version>/`, writes `service.toml`, installs the
daemon job and the sessions slice, and waits for daemon readiness. The daemon
then starts every worker as its own transient unit per worker generation.

An install that already runs worker-aware sessions under the older
`pohunek-session@<session-id>.service` template is retired by the same
installer: it refuses while any of those template workers is active, then
disables `pohunekd.service` and removes `pohunekd.service`,
`pohunek-session@.service`, and `pohunek-sessions.slice` from the user unit
directory before installing the service. Stop those sessions first; their PTYs
cannot move into the new per-generation jobs.

## Explicit Runtime-Loss Acceptance

If live legacy sessions cannot be drained and losing their PTYs is acceptable,
the installer requires an explicit destructive flag:

```bash
./packaging/install-daemon.sh --accept-runtime-loss
```

Before using it:

- capture the exact affected session ids;
- assume every live shell and agent without a launch-native reference is
  unrecoverable;
- understand that a recoverable provider conversation will still receive a new
  PTY, process, PID, and runtime generation;
- ensure no automation adds this flag by default.

The flag is informed consent, not a live handoff and not a recovery command.
The installer does not silently invoke native resume. After installation,
inspect retained logical records. Use explicit `session.resume` only for a
terminal or lost session whose immutable launch identity has valid recovery
metadata.

## Boundary Rollback

A legacy daemon cannot adopt worker-owned PTYs. Do not start a legacy daemon
beside live worker jobs: it could treat old resume metadata as independent
work and launch a duplicate process.

Before crossing back to a legacy release:

1. enumerate worker jobs with `pohunek service status --json`;
2. let every worker-backed session finish or stop it explicitly;
3. export only eligible logical sessions into the legacy recovery format using
   the supported release tooling;
4. verify that no worker job remains;
5. remove the service with `pohunek service uninstall` (durable metadata is
   kept without `--purge`);
6. start the legacy daemon;
7. recover eligible sessions explicitly.

If the release tooling cannot export the worker-aware logical records, rollback
is blocked. Do not edit tagged metadata records by hand.

## Post-migration Expectations

For every new managed session:

- `SessionInfo.runtime` reports the worker and runtime generation;
- one worker job per generation (`pohunek-<ns>-worker-<session-id>-<generation>.service`
  on Linux) is active while its PTY is live;
- restarting the daemon job closes existing client streams but leaves the
  worker job and child unchanged;
- the replacement daemon emits `session_runtime_reconnected`;
- worker or host loss leaves the logical record visible as `lost`;
- provider-native recovery is explicit and emits `session_native_recovered`.

See the
[durable worker operations runbook](../runbooks/durable-session-workers.md) for
diagnosis after migration.

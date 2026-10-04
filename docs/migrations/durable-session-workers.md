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
   its `worker_id`, `worker_instance_id`, root child PID, and the worker generation and
   PID reported by `pohunek service status --json`.

The installer runs `pohunek service install` (or `pohunek service upgrade` for
an existing service): it copies the binaries into
`<prefix>/libexec/pohunek/<version>/`, writes `service.toml`, installs the
daemon job and the sessions slice, and waits for daemon readiness. The daemon
then starts every worker as its own transient unit per worker generation.

An install that already runs worker-aware sessions under the older
`pohunek-session@<session-id>.service` template is retired by the same
installer. Because the already-deployed legacy binary cannot gain a
daemon-side barrier, the installer retires it in this order. It first asks
`systemctl --user is-active pohunekd.service` for the legacy daemon's state;
a query that answers nothing refuses before anything changes.

The new daemon converts the legacy session records (including their resume
bindings) only from the migration manifest that `pohunek migration preflight`
writes against the running legacy daemon, so every retirement takes a fresh
snapshot. A daemon reported `inactive` or `failed` is started with
`systemctl --user start pohunekd.service` for it (the legacy unit is
`Type=notify`, so the start returns once the daemon serves its socket). A
start that fails, or a daemon that is not `active` afterwards, refuses the run
without retiring anything, because retiring without the snapshot would leave
resume bindings unimported or a stale manifest from an earlier refused run in
place. Every later refusal, and an interruption, stops a daemon the run
started again, so a refused run leaves the legacy install stopped as it found
it. Every other state (`active`, `activating`, `deactivating`, `reloading`, or
anything unexpected) goes straight to the barrier. A daemon that is not
stopped but has no control socket node yet (or anymore) refuses unchanged,
since no preflight can reach it.

1. it moves the legacy daemon's control socket node aside
   (`XDG_RUNTIME_DIR/pohunek/daemon.sock`) with `rename(2)` to the sibling
   `XDG_RUNTIME_DIR/pohunek/retiring`, so new clients cannot open new
   connections (existing connections keep serving). The moved name is no
   longer than `daemon.sock`, so it fits the `sun_path` limit whenever the
   original does. A node already at that name, left by a run killed before
   it could clean up, refuses the run unchanged; when the daemon's socket
   itself is the one left there, the refusal prints the `mv` that restores it;
2. it runs `pohunek migration preflight --socket <moved-socket>` over that
   socket, which refuses while the legacy daemon owns live PTYs;
3. it lists `pohunek-session@` template jobs in every state except
   `inactive` and refuses when one survives — including jobs still in
   `activating`, which a state-filtered check would miss;
4. only then does it `systemctl --user disable --now pohunekd.service`, which
   closes the socket and stops new sessions for good. If that command fails,
   the run aborts with its status and keeps the legacy files: a daemon that
   may still run gets its socket node back at the original path, and one that
   `systemctl --user is-active` reports `inactive` or `failed` loses the stale
   moved node. The same rule settles the moved node when the run ends any
   other way while it is in place — `HUP`, `INT` (Ctrl-C), or `TERM`, or an
   unexpected shell error — so an interrupted run never leaves a running
   legacy daemon reachable only under the moved name; the run keeps its exit
   status, and a signal is re-raised after the cleanup. The node goes back
   with a hard link to the original name and an unlink of the moved one,
   which never replaces an existing name: the legacy unit restarts its daemon
   on failure, and a restarted daemon that already bound a new socket there
   keeps it, while the then-stale moved node is removed and reported. When
   the node cannot go back for another reason, the run prints the `mv` that
   restores it;
5. it re-runs the job inventory: a worker that survived the stop was started
   after the preflight and aborts the run;
6. it removes `pohunekd.service`, `pohunek-session@.service`, and
   `pohunek-sessions.slice` from the user unit directory before installing.

The whole installer run holds the service transaction lock
(`$XDG_STATE_HOME/pohunek/service-install.lock`): it re-executes itself under
`pohunek service lock -- …`, which refuses with
`service_transaction_in_progress`, before anything changes, while another
`pohunek service install|upgrade|uninstall` holds the lock. From then on no
such command can start until the run ends, and the final `pohunek service
install|upgrade` adopts the same lock with the holder token
`pohunek service lock` passes in `POHUNEK_SERVICE_LOCK_TOKEN` (it records
itself and that token in `$XDG_STATE_HOME/pohunek/service-install.lock.holder`
and keeps the lock itself until the run ends; a command that adopted the
lock holds `$XDG_STATE_HOME/pohunek/service-install.lock.adopted` shared, and
the lock is released only after every such command ended; a command that
runs a transaction also holds `service-install.lock.inherited` exclusively, so
adopted transactions run one at a time). A `pohunek service` command
started from elsewhere during the retirement is refused instead of racing it,
and no process the run leaves behind can hold or adopt the lock afterwards.

Before the first of the steps above, the installer runs
`pohunek service check --prefix <prefix> --json`, which makes every check the
final `pohunek service install|upgrade` makes before its first effect, in the
same Rust code, and changes nothing: `HOME` must be an existing directory and
it and every XDG root an absolute, normalized UTF-8 path the daemon's job
definition can carry; the prefix must be valid; every directory the command
writes must be trusted — the install prefix, `<prefix>/bin`,
`<prefix>/libexec`, `<prefix>/libexec/pohunek`, the user unit directory
(`$XDG_CONFIG_HOME/systemd/user`), and the `service.toml` directory
(`$XDG_CONFIG_HOME/pohunek`) reached through no symlink, owned by the invoking
user (or, for an ancestor, by the owner of `/`), and not writable by group or
others, and the application state and runtime roots (`$XDG_STATE_HOME/pohunek`,
`$XDG_RUNTIME_DIR/pohunek`) of mode `0700` exactly; a pending transaction the
command would refuse (`service_install_pending`) refuses; an upgrade's
recorded installation must describe this user and these XDG roots; the
prefix must not belong to another installation (`service_prefix_owned`); and
an install that starts over must find no daemon job registered without
`service.toml` (`service_daemon_job_present`), which the check asks the
service manager last. A
failed check prints the error document, with the code the final command would
fail with, and refuses the run with the legacy install untouched. The check
also confirms it ran under the run's lock (`"locked": true`); a
`POHUNEK_SERVICE_LOCK_TOKEN` that proves no live holder fails with
`service_inherited_lock_invalid` rather than running unlocked.

Every legacy file the installer removes lies under one validated install
prefix, resolved before anything changes:

- for a fresh install (or a pending install being finished) it is
  `POHUNEK_INSTALL_PREFIX`, or `$HOME/.local` when that is unset; for an
  upgrade it is the prefix recorded in `service.toml`, read from
  `pohunek service status --json`, and a `POHUNEK_INSTALL_PREFIX` that names
  a different path refuses the run;
- the prefix must be an absolute path without `.` or `..` components
  (repeated and trailing slashes are collapsed); anything else refuses;
- a legacy `pohunekd.service` must run `<prefix>/bin/pohunekd` for that same
  prefix, otherwise the run refuses and names the unit's `ExecStart=` path, so
  point `POHUNEK_INSTALL_PREFIX` at the legacy install's prefix;
- `<prefix>/bin/pohunekd` and `<prefix>/libexec/pohunek-sessiond` are removed
  only as regular files owned by the invoking user, reached through no
  symlinked `bin` or `libexec` directory — the files the legacy installer
  wrote. A symlink, another user's file, or anything else at those paths is
  left in place and named on stderr.

Any refusal above leaves the legacy files untouched, so the operator can
restart the legacy daemon and decide. Stop those sessions first; their PTYs
cannot move into the new per-generation jobs. Even with this sequence, a
client that already held a control connection before the barrier (a
long-lived client) can still start a session into the window until the daemon
stops; the post-stop inventory catches it.

## Explicit Runtime-Loss Acceptance

If live legacy sessions cannot be drained and losing their PTYs is acceptable,
the installer requires an explicit destructive flag:

```bash
./packaging/install-daemon.sh --accept-runtime-loss
```

The flag is forwarded to the preflight so it records consent in the migration
manifest. It covers only daemon-owned PTYs: a live template worker, found
before or after the daemon stop, always aborts the run, because removing the
unit files it runs from would orphan it. Before using it:

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

A worker-aware daemon that starts over legacy resume bindings with no logical
record and no migration manifest (the binary was replaced without this
installer's preflight) imports nothing and still starts. Each binding stays on
disk and is listed by `pohunek session runtime-inventory` as `orphaned` with
reason `migration_manifest_missing`, and `session.new` and `session.fork` are
refused with the error code `migration_manifest_missing` until a daemon start
imports a manifest. Reinstall the legacy release and rerun this installer so
the preflight snapshots them; once a logical record exists, a later manifest is
archived without being imported.

See the
[durable worker operations runbook](../runbooks/durable-session-workers.md) for
diagnosis after migration.

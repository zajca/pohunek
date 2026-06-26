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

1. Run `pohunek doctor --json` for local environment checks.
2. Run `pohunek health --json` to query the local daemon.
3. If health cannot connect, start the daemon with
   `pohunek daemon start --detach`.
4. Run `pohunek health --json` again and inspect the reported socket, version,
   and status.
5. For remote hosts, run `pohunek host inspect <host> --json` and confirm the
   host daemon responds through the remote transport.
6. If a session was expected, run `pohunek session list --json` on the relevant
   host and inspect the specific session with `pohunek session inspect <target>`.

Do not treat setup assets as the daemon source of truth. Launcher scripts may be
stale or missing while the daemon itself is healthy; verify daemon health first,
then move to [debug launcher](debug-launcher.md).

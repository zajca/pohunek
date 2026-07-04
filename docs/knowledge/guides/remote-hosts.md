---
type: Guide
id: guide/remote-hosts
title: Remote hosts
description: Use host discovery and host-qualified targets to inspect and operate Pohunek across NetBird peers.
source_kind: manual
intents: [setup, project, debug, help]
---

# Remote Hosts

Remote behavior is host-aware. The CLI uses `--host <host>` for commands that
target a host, and session targets can use `<host>/<session-id>`.

Use these commands for orientation:

- `pohunek host discover --json` to enumerate NetBird peers and probe daemons.
- `pohunek host list --json` to list known live peers.
- `pohunek host inspect <host> --json` to inspect one host's daemon
  capabilities.

Remote session creation should use a registered project or an explicit
repository path valid on the remote host. Non-local starts preserve the existing
confirmation model; non-interactive remote starts require `--yes`.

Durable notifications keep the same host-authoritative model. Each daemon owns
only its local notification store, and cross-host notification views are
client-side fan-out. `pohunek notifications list --all-hosts` queries the local
daemon plus reachable daemon peers discovered through `host.discover`, then
renders per-host successes and structured per-host errors. The matching watch
command with `--all-hosts` opens one subscription per reachable host and streams
notification create, update, and delete events as they arrive.

Policy and retention commands can also fan out with `--all-hosts`:
`pohunek notifications policy get --all-hosts`, policy set with `--all-hosts`,
and retention prune with `--all-hosts`. Single-record actions use the target
host: `host/id` overrides `--host`, while a bare id targets the selected
`--host` or local daemon.

The assistant design keeps the same boundary. A remote assistant must use a
knowledge bundle materialized on the remote host, version-matched to the remote
binary, and readable by the selected remote agent profile.

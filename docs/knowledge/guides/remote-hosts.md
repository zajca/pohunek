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

The assistant design keeps the same boundary. A remote assistant must use a
knowledge bundle materialized on the remote host, version-matched to the remote
binary, and readable by the selected remote agent profile.

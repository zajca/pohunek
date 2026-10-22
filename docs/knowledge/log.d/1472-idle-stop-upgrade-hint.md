## Unreleased (2026-10-22, idle-stop upgrade hint names the daemon version, #1472)

`pohunek session policy set --idle-stop-after` against a daemon that predates the idle-stop default fails with `method_not_found`; its upgrade hint now names the connected daemon's version, read from the `daemon.health` answer of the same connection. See `docs/cli.md`.

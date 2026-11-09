---
kind: removed
---
The team relay moved to its own repository, `pohunekd/pohunek-relay`. The `pohunek relay` command group (`login`, `status`, `logout`, `rotate`, `resume-rotation`) is gone and is an unknown-command usage error, the `pohunek-relayd` archive is no longer a release artifact, and the `@pohunek/protocol/relay` TypeScript subpath is removed. `pohunek host list`, `session list` and `doctor` are unchanged on a host with no relay configured, and host identity, host-local governance and the relay principal and team types stay in core. Credentials stored by `pohunek relay login` in the OS keyring are no longer read or removed by Pohunek; use the relay repository's tooling or delete the `pohunek-relay` keyring entries by hand.

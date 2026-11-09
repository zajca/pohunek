---
kind: removed
breaking: true
---
The team relay moved to its own repository, `pohunekd/pohunek-relay`. The `pohunek relay` command group (`login`, `status`, `logout`, `rotate`, `resume-rotation`) is gone and is an unknown-command usage error, the `pohunek-relayd` archive is no longer a release artifact, and the `@pohunek/protocol/relay` TypeScript subpath is removed. `pohunek host list`, `session list` and `doctor` are unchanged on a host with no relay configured, and host identity, host-local governance and the relay principal and team types stay in core. Credentials stored by `pohunek relay login` in the OS keyring are no longer read or removed by Pohunek: run `pohunek relay logout --origin <origin>` with the previous release before upgrading, or delete the `pohunek-relay` keyring entries by hand afterwards. `$XDG_DATA_HOME/pohunek/relay-locks/` is safe to delete; `pohunek service uninstall --purge` does not remove it.

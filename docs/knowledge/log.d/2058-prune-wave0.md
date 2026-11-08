## Unreleased (2026-11-08, prune wave 0, #2058, #2039, #2037, #2038, #2036)

- Workflow plugins, daemon idle stop, session fork and the debug commands `store rejected`, `runtime-inventory` and `session policy` are gone from the CLI, the protocol (`method_not_found` on every version) and the guides. `capabilities.fork` stays on session listings and is always `false`.
- Fork data in an older runtime descriptor, host profile or stored session record is accepted and ignored with a warning; see [runtime packages](guides/runtime-packages.md#fork-data-is-ignored).
- The Hermes operator plugin no longer offers `pohunek_session_fork`; `pohunek doctor` still reports metadata-store lines no loader accepts, and the [recovery runbook](runbooks/runtime-package-recovery.md) has the manual remedy.

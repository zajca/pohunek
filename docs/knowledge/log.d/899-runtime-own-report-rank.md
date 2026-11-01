## Unreleased (2026-11-01, a runtime's own report ranks as a hook provider, #899)

- A session-scoped `notification.create` whose provider is the id of the held session's own runtime and which the runtime's hook schema admits is a hook claim: the record carries the daemon-set `hook_admitted: true`, ranks as a provider hook and upgrades the projector's attention record. A create that is not admitted is stored unmarked, not refused. The notification store is format 2 and reads format 1. See [Sessions](concepts/sessions.md) and `docs/public-api.md`.

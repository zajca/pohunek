## Unreleased (2026-10-22, bounded Hermes provisioning, #1494)

`scripts/provision-hermes-compat` bounds its network phases: the locked-tag
`git fetch` retries up to three times with a stall bound (below 1 KiB/s for
60 s), the pinned CPython download has connect, stall and total timeouts and a
bounded retry, and each phase prints a progress line and fails with a message
naming it. The `Provision locked Hermes runtime` steps of both pinned Hermes
compatibility jobs carry a 15 minute step timeout.

## Unreleased (2026-10-30, store set-aside refusal text and setup guide, #1752)

- `store_rejected_line_live` names the refused line kind: a `resume` line whose package pin cannot be read, or a line that may be a session record. The `store rejected set-aside` help states the unreadable-pin `resume` rule. The error code is unchanged.
- The setup guide documents the optional `[inventory]` table of `service.toml`.
- The architecture concept and the durable-workers RFC name a same-user server in a logind session scope next to `at(1)` as a way for a marked process to run outside the worker's user-manager subtree.
- The daemon logs `inventory probe joined in-flight` at debug level when a `host.inspect` inventory waits for another inventory's version probes instead of starting its own.

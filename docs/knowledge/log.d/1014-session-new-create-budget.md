## Unreleased (2026-10-15, session new waits for the daemon's bounded create, #1014)

- Behaviour change in the Rust and TypeScript SDKs: a typed `session.new`
  without an explicit response deadline now waits up to the SDK's creation
  budget, 60 minutes plus 1 second of transport headroom
  (`SESSION_NEW_REQUEST_BUDGET_MS`), on a dedicated connection, instead of the
  5-second default. The budget is a constant derived from the daemon's own
  bounds: worker connection and initialization, up to 10 minutes each, the
  `pre-create` and `post-create` hooks, a host and a repository layer of 5
  minutes each, and, with initial input, the reader grace. Before, only a
  creation with initial input got it. A caller that wants a shorter wait still
  passes one: `--request-timeout-ms`, `with_request_timeout` or
  `requestTimeoutMs`.
- The daemon answers `session.new` only after the project's create hooks, so
  with the 5-second deadline a slow hook turned into `request_timeout` for a
  create the daemon then finished.
- A `session new` that still times out reports the unknown outcome in its
  recovery hint: look the session up by a caller `--meta` key with
  `session list --meta-eq` before retrying, since an empty list does not prove
  that nothing was created and a retry can create a second session.
- The daemon logs `hook started` and `hook finished` at INFO for every hook
  run, with the session id, event, layer, script, timeout, duration and
  outcome.

## Unreleased (2026-10-14, session capability refusal reasons, #912)

- `session list` and `session inspect` responses carry an optional
  `capability_refusals` object, `{resume?, fork?}`, whenever the daemon clears
  a frozen `true` capability. Each value is the error code `session.resume`
  or `session.fork` returns when the session's recovery resolution refuses
  it, computed by the same check that clears the flag. State-dependent or
  per-request refusals come only from the operation.
- The reason is an additive field on `SessionInfo`, which is not strict, so
  0.34 clients on protocol 4 ignore it, and the protocol 3 adapter strips it.
  The capability object stays strict and unchanged; moving the reason into
  `SessionCapabilities` is deferred to the next public protocol bump.
- Human `session list` prints `refused <id>: resume=<code> fork=<code>`, and
  `session inspect` has a `capability_refusals` row.

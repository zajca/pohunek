## Unreleased (2026-10-12, TypeScript turn waits, #858)

- The TypeScript SDK adds `client.sessionWaitTurn(params)` for
  `session.wait_turn`, on a dedicated connection whose deadline follows
  `timeout_ms` or, when it is omitted, `MAX_SESSION_WAIT_TURN_MS`; generic
  `call("session.wait_turn", ...)` takes the same path. `@pohunek/protocol`
  exports `MAX_SESSION_WAIT_TURN_MS`, `MAX_SESSION_WAIT_TURN_TARGETS`, and
  `MAX_SESSION_TURN_CURSOR_BYTES`. The testkit fixture daemon serves the method
  and adds `scenario.recordTurnEvent`. Documented in the `ts-sdk` guide,
  `concepts/sessions`, `sdk/ts/sdk/README.md`, and `docs/public-api.md`.

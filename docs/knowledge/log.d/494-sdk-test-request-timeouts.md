## Unreleased (2026-10-15, SDK test request timeouts, #494)

- The TS SDK relay suite (`sdk/ts/sdk/test/ws-transport.test.ts`) gives its
  clients a 2 s request timeout, `RELAY_REQUEST_TIMEOUT_MS`, instead of 20 ms.
  A round trip through the in-process relay exceeded 20 ms on a loaded macOS
  runner. Only the scenario that asserts the timeout passes a tiny value.
- The dedicated-routing scenarios in `request-response.test.ts` use a 1 s
  shared timeout, and daemon replies delayed 1.5 s. The dedicated deadlines
  are 4 s for the generic `session.wait` and `session.output` calls and 9 s
  for the input wait (its default 8 s plus 1 s headroom). The direct call the
  scenarios also make no longer has to finish in 20 ms.
- SDK behaviour is unchanged.

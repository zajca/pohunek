## Unreleased (2026-10-14, generic SDK calls route waits to dedicated connections, #878)

- `pohunek_client::Client::call` and the TypeScript SDK's `Client.call` send
  `session.wait`, and `session.output` with `wait_ms`, on a dedicated
  connection whose deadline follows the requested wait, as the typed session
  wait and output helpers do. A generic wait longer than the 5 s default
  request timeout no longer fails on the client side, and a wait no longer
  blocks the shared connection.

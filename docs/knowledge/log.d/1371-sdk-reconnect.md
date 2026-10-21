## Unreleased (2026-10-21, clients recover from a timed-out request, #1371)

- A TypeScript SDK `Client` is no longer unusable for good after one request
  timeout. The failed call still rejects (`requestTimeout`, and never resent,
  reads included), and the next call opens a fresh control connection, learns
  the daemon's protocol version again and sends only its own request; a late
  reply of the old connection is never delivered, concurrent callers share one
  reconnect, and the version probe of a fresh connection has its own
  `requestTimeoutMs`. The Rust `Client` does the same. Closed clients, consumed
  subscriptions and incompatible protocol versions stay refused.

## Unreleased (2026-11-26, external-client contract: subscription lag, heartbeat, local socket resolver, #1314)

- `subscribe` streams now send `subscription_lagged {channel, skipped}` before any
  later event of that channel when a slow subscriber lost events; the client
  re-seeds with `session.list` (session channel) or `notification.list`
  (notification channel). A lag is per channel, never per connection.
- `subscribe` accepts optional `{heartbeat_ms}`; the daemon then sends `heartbeat`
  events while the stream is quiet. `host.inspect` carries `subscribe_heartbeat
  {min_ms, max_ms}` on a daemon that serves it; an out-of-range value is
  `heartbeat_out_of_range`. The new `[subscriptions]` table of `service.toml` holds
  the channel capacities and the heartbeat bounds.
- `pohunek-client` exports `local_socket_path()` with typed `local_socket_*` errors
  and `Client::subscribe_with_options`; `pohunek-paths` checks that an existing
  runtime directory and socket are private. All changes are additive within the
  current protocol version.
- Source map: `concepts/` need no change; `assistant/source-map.md` lists the three
  new files (`crates/protocol/src/subscription.rs`,
  `crates/daemon/src/api/handler/subscribe.rs`, `crates/client/src/local_socket.rs`).

---
kind: added
---
A `subscribe` stream now tells a slow reader what it lost with a `subscription_lagged` event, and a subscriber can ask for periodic `heartbeat` events while the stream is quiet (`heartbeat_ms`, bounded by the new `[subscriptions]` table of `service.toml` and advertised in `host.inspect`). `pohunek-client` exports `local_socket_path()`, which checks that the runtime directory and socket are private, and `Client::subscribe_with_options`. ([#1314](https://github.com/zajca/pohunek/issues/1314), [#1772](https://github.com/zajca/pohunek/issues/1772), [#1315](https://github.com/zajca/pohunek/issues/1315))

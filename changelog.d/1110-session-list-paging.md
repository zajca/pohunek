---
kind: fixed
---
`session.list` accepts `limit` and `cursor`, and a session set that does not fit one control line is answered with the typed `session_list_too_large` error instead of a frame the client refuses. `pohunek session list` and the Rust SDK `Client::session_list` page through the whole set themselves; the TypeScript SDK does not page yet ([#2570](https://github.com/zajca/pohunek/issues/2570)). ([#1110](https://github.com/zajca/pohunek/issues/1110))

---
kind: changed
---
`cargo xtask docs check` gains a `numeric-facts` check: the public protocol window, the worker protocol window and the store schema version that the docs state must match `PROTOCOL_VERSION`, `MIN_PROTOCOL_VERSION`, `CURRENT_VERSION`, `PREVIOUS_VERSION` and `STORE_SCHEMA_VERSION`, and a wrong statement fails with its page and line. `docs/public-api.md` now names store schema 22. ([#2182](https://github.com/zajca/pohunek/issues/2182))

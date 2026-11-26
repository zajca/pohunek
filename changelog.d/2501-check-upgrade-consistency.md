---
kind: fixed
---
`pohunek service check` now refuses a pending `refresh-path` over a `service.toml` of an earlier schema with `service_refresh_config_schema`, the refusal `service upgrade` gives, instead of reporting a clean roll back. `pohunek service upgrade` and `pohunek setup` also take one decision about the listed assistant package, so a revoked listed version is refused with the same text by both. ([#2501](https://github.com/zajca/pohunek/issues/2501), [#2503](https://github.com/zajca/pohunek/issues/2503))

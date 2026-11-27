---
kind: fixed
---
`pohunek service check` now refuses a pending `refresh-path` over a `service.toml` that was edited after the refresh wrote it with `service_refresh_config_changed`, the refusal `service upgrade` gives, instead of reporting a clean roll back. The check previews the rollback's own read and classification of the file, so it cannot drift from the upgrade again. ([#2562](https://github.com/zajca/pohunek/issues/2562))

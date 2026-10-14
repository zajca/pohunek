## Unreleased (2026-10-12, service.toml turn-wait timings, #858)

- `service.toml` schema 4 adds `[waits]`: `turn_default_ms` (the timeout of a
  `session.wait_turn` call that names none, 300000 on fresh installs) and
  `turn_max_ms` (the ceiling a requested timeout may not exceed, 1800000).
  Both are whole milliseconds in `5000..=3600000`, and a default above the
  ceiling makes the daemon refuse startup. `pohunek service upgrade` migrates
  schema-2 and schema-3 files with the same backup and rollback transaction,
  assigning the defaults of the missing tables and keeping every recorded
  value; `config_schema_migration` reports `2` or `3`. Documented in
  `guides/setup`, the `update-after-release` runbook and `docs/public-api.md`.

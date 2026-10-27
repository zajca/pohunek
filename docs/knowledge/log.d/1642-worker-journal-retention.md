## Unreleased (2026-10-27, worker journal retention, #1642)

- `pohunek session rm` now deletes the session's worker journal directory (`<state>/workers/<session>/`) after the session record is deleted; a removal that proceeded past an unconfirmed process keeps it.
- The daemon collects, at start, the journal directories of sessions that no longer exist when their workers are gone and nothing in them changed within `[worker_journals] max_age_days` (optional table of `service.toml`, default 7 days, `1..=365`). Journals of sessions that have a record are never deleted. See [setup](guides/setup.md).

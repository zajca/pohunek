## Unreleased (2026-11-11, retention ended_at, #2162)

- `SessionInfo.ended_at` records when a session entered `stopped`, `done` or `failed` (store schema 19; the migration copies `updated_at` into it for a finished record). A terminal session ages for retention from it, a `lost` runtime from `last_connected_at`, and a record without it from `updated_at`. See [Sessions](concepts/sessions.md).

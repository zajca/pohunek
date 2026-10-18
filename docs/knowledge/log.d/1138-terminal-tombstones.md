## Unreleased (2026-10-18, terminal notification compaction, #1138)

Deleted daemon-projected `error` and `session_finished` notifications stay deleted
after compaction and daemon restart. Compaction preserves at most one invisible
deleted snapshot per terminal kind for each durable session's current runtime
generation, including the timestamp needed for older epoch-keyed records. A later
eligible compaction drops evidence when its session is removed or advances
generation. While session metadata is unreadable or contains rejected or torn
records, compaction keeps every terminal tombstone and warns once, but still
compacts action history and unrelated deleted records. Complete metadata restores
the per-generation bound. Public protocol and persisted record shapes are unchanged.

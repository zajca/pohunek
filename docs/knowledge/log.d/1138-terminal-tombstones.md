## Unreleased (2026-10-18, terminal notification compaction, #1138)

Deleted daemon-projected `error` and `session_finished` notifications stay deleted
after compaction and daemon restart. Compaction preserves at most one invisible
deleted snapshot per terminal kind for each durable session's current runtime
generation, including the timestamp needed for older epoch-keyed records. A later
eligible compaction drops evidence when its session is removed or advances
generation. Unreadable or rejected session metadata prevents destructive
compaction. Ordinary notification history and unrelated deleted records still
compact normally; public protocol and persisted record shapes are unchanged.

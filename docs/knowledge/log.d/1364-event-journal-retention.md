## Unreleased (2026-10-22, bounded event journal, #1364)

- The event journal `<data>/events/events.jsonl` is bounded. `service.toml`
  schema 7 adds the required `[events]` table (`max_file_bytes` 33554432,
  `max_files` 8, `max_age_days` 14): the journal rotates to `events.jsonl.N`
  before a write would cross `max_file_bytes`, keeps at most `max_files` files
  and deletes rotated files older than `max_age_days`; an `events.jsonl` last
  written longer ago than that is removed at start. `pohunek service
  upgrade` migrates schemas 2 to 6 and writes these values; a daemon without
  `service.toml` uses them.
- The first start removes an `events.jsonl` above `max_file_bytes` without
  reading it and logs one WARN with its path and size to `pohunekd.jsonl`. The
  history in that file is not kept; nothing in the daemon reads the journal.
- `agent_state` events that restate a session's last journaled activity,
  source, runtime and epoch (only `revision` changed) are no longer journaled.
- A dedicated writer thread owns the journal files, so event logging no longer
  runs filesystem writes on a tokio worker. When the writer cannot keep up, the
  drain waits and the broadcast lag is recorded as an `events_dropped` line.

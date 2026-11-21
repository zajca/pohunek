## Unreleased (2026-11-21, stale needs-you records, #2489)

- Open `agent_blocked` / `approval_required` records are acknowledged when the
  session is removed (`session_removed` in the notification projector) and once
  after startup reconciliation (`resolve_stale_attention`, called from
  `main.rs` after `reconcile_workers`) for every session that is absent,
  stopped, done, failed or on a lost runtime. A live session keeps its record
  whatever its stored activity. The clear signal for a notifier is the
  existing `notification_updated` event with status `acknowledged`.

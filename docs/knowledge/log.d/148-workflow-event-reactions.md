## Unreleased (2026-10-14, workflow event reactions, #148)

- Workflow plugins gain `[reactions.<name>]`: argv arrays that run once per
  matching public event. Events come from the protocol's event table minus the
  attach stream events. Reactions run without the session origin pair.
- One dispatcher unit per namespace runs the hidden `pohunek workflow
  __dispatch`. It reads the daemon's `subscribe` stream, never starts the
  daemon, and keeps one bounded queue per plugin. A full queue drops events and
  writes one `dropped` run record per overflow episode.
- `[reactions]` owner knobs: `queue_depth`, `reconnect_initial_ms`,
  `reconnect_max_ms`.

## Unreleased (2026-10-23, rejected forwarded agent operations, #1418)

- A forwarded `session.report_agent` or `session.release_agent` whose answer was
  lost or timed out stays pending until the daemon knows what the worker did with
  it. When the worker rejected it after the bound (a stale sequence, a process
  outside the PTY tree, a release that matches no claim, a journal write fault),
  the daemon now drops the operation as soon as it reads a snapshot taken after
  the worker handled the request that still shows the identity revision the
  request was sent at. Until then the operation held the journal, so a
  `session.report_agent` for the claim the worker really holds was answered
  `recorded: false`; it is now admitted once the rejection is read, which is
  within one periodic worker sync (100 ms) of the worker answering. A snapshot
  read earlier, or one that shows a newer revision, still leaves the operation
  pending. No protocol, store or worker change.

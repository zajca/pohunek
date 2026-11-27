## Unreleased (2026-11-27, worker protocol window 8..=9, #2536)

- The private worker protocol window changes: `PREVIOUS_VERSION` is now 8 (was 7)
  and `CURRENT_VERSION` is now 9 (was 8). Protocol 9 adds no wire change yet; it
  opens the window for the offset-based reattach of #2536. A worker that
  negotiated protocol 7 (started by a 0.44-era daemon and still running) is
  outside the window: the daemon does not adopt it, and the upgrade preflight
  reports its session as `would_not_be_adopted` with `worker_protocol_incompatible`,
  so the upgrade refuses without `--accept-runtime-loss`; the session must be
  relaunched.
- Protocol 8 is the floor, so the daemon always sends `RecordActiveIdentity` and
  `ReleaseActiveIdentity`, and the worker snapshot always carries the forwarded
  claim's label, origin and reported session, the active identity revision, the
  write acknowledgement's `submit_mark_ns` and the `prompt_accepted_sequence`.
  The `session.active_identity.not_durable` cause `worker_protocol_v7` is gone.

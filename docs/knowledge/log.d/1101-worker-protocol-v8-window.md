## Unreleased (2026-10-19, worker protocol window 7..=8, #1101)

- The private worker protocol window changes: `PREVIOUS_VERSION` is now 7 (was 6)
  and `CURRENT_VERSION` is now 8 (was 7). Protocol 8 adds no wire change yet; it opens the
  window for the forwarded active identity of #1101. A worker that negotiated
  protocol 6 (started by a 0.34.x daemon and still running) is outside the
  window: the daemon does not adopt it, and the upgrade preflight reports its
  session as `would_not_be_adopted` with `worker_protocol_incompatible`, so the
  upgrade refuses without `--accept-runtime-loss`.
- Protocol 7 is the floor, so `Initialize` always carries the base environment
  and the input stall bound, a stalled write is always `session_input_stalled`
  and a missing launch directory is always `session_cwd_missing`. The
  inherit-the-worker-environment launch mode of protocol 5 is gone.

## Unreleased (2026-10-14, a sweep's last pass can confirm the cleanup, #989)

- The daemon's lost-runtime sweep decides the outcome after its last allowed
  pass from a survey that sends no signal. A runtime whose processes forked
  during the earlier passes, so that the last pass ended the last of them, is
  `lost` with its cleanup confirmed instead of
  `runtime_lost_cleanup_unconfirmed`, and its removal is no longer refused.
  Anything still attributed to the runtime, or that could belong to it, keeps
  the cleanup unconfirmed.

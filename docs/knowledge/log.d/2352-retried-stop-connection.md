## Unreleased (2026-11-14, retried stop keeps its connection, #2352)

- A stop the worker refuses because its terminal commit is still pending is
  sent once more on the control connection the stop still holds, so the
  retried stop never waits for the connection again: a stop waits for the
  connection once and for the worker's answer at most twice, the bound the SDK
  stop budget covers. See [Sessions](concepts/sessions.md).

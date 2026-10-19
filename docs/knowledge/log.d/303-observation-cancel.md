## Unreleased (2026-10-19, observation stream cancellation, #303)

Documented why a cancelled worker observation frame read cannot desynchronize
its one-shot stream: competing wakes finish the stream instead of resuming a
partial frame. Runtime behavior is unchanged.

## Unreleased (2026-10-18, descendant agent claims, #1107)

Procwatch preserves a hook-reported agent PID and active native reference when
the live reporter descends from an observed same-runtime agent in its session,
including foreground reconciliation after a Working transition. Process death,
PID reuse, a sibling outside the agent tree, another runtime and foreign-session
ownership still release or replace the claim. See [session identity](concepts/sessions.md).

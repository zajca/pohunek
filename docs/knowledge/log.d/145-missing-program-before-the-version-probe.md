## Unreleased (2026-10-12, missing program before the version probe, #145)

- `session.new`, `session.resume` and `session.fork` resolve the runtime's
  program before any version probe, for every runtime but a bare host shell
  (whose configured shell command its launch resolves), and answer
  `runtime/agent_binary_missing` when it does not resolve to an executable,
  before any session, worker, worktree or recovery write. That refusal's
  message and recover hint name only the program's file name. Only an
  executable that exists but that the probe rejects (another release,
  unparseable output, a failing or timed-out probe, an unknown parser) answers
  `agent_runtime_unsupported`. A missing Hermes executable, and a missing
  executable of a package runtime that declares a probe (the official Codex
  and Claude packages), therefore answer `agent_binary_missing`. Documented in
  `concepts/agent-profiles`, `concepts/sessions`, the
  `debug-session-runtime` runbook and `docs/public-api.md`.

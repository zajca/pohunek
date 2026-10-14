## Unreleased (2026-10-12, resumable Pi forks, #546)

- A runtime with `[native_reference] strategy = "assigned"` may declare
  `assign_fork = true` (only next to `[fork] supported = true`, otherwise
  `descriptor_invalid`). `session.fork` then generates a fresh reference for the
  fork and appends the rendered `launch_args` after the `[fork]` args; the fork
  holds it with provenance `assigned` from creation, so it resumes into its own
  conversation after a stop or a daemon restart. The official Pi package
  declares it from package version 1.0.1 (`pi --fork <source> --session-id
  <fork>`, verified against Pi 1.0.2, which writes the fork's
  `<time>_<fork>.jsonl` at once); `plugin update` installs 1.0.1 beside 1.0.0. The flag is
  frozen into the session's native launch spec (store schema 6, a no-op
  migration from 5), and `PackageRuntimeInfo` gains `fork_launch_args`, shown by
  `plugin inspect` as the fork's `then adds:` line. Documented in
  `guides/pi-package`, `concepts/sessions`, `concepts/agent-profiles`,
  `docs/install.md`, and `docs/public-api.md`.

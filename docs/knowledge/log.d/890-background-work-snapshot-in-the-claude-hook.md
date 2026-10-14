## Unreleased (2026-10-12, background work snapshot in the Claude hook, #890)

- `POHUNEK_INTEGRATION_VERSION` is 14. The Claude `stop` hook forwards each
  background task's sanitized `label` (every type), `program` (`shell` only,
  the command word's basename) and `agent_type` (`subagent` only) in the
  `subagent_snapshot` claim. The command and its arguments are never forwarded;
  the label and program limits come from worker-injected environment variables.
  The `session` hook forwards `session_source` (`startup`, `resume`, `clear`,
  `compact`) in its identity report.
- Documented in the `claude-package` guide. Concept and CLI pages follow with
  the later slices of #890.

## Unreleased (2026-10-20, fork, stop and remove response budgets, #1381)

- `session.fork`, `session.stop`, `session.remove` and
  `session.remove_accepting_unconfirmed` no longer run under the clients' 5 s
  default response timeout. The Rust client, the CLI (including the attach
  menu) and the TypeScript SDK wait on a dedicated connection for 30 minutes
  (fork), 90 minutes (stop) and 110 minutes (remove: the stop plus the worktree
  hooks at their 5 minute timeout). An elapsed deadline is the transport error
  `session_fork_outcome_unknown`, `session_stop_outcome_unknown` or
  `session_remove_outcome_unknown` (a retried fork creates a second child; a
  repeated stop is harmless). The CLI prints a waiting notice on stderr after
  5 s, never with `--json`. An explicit request timeout still overrides the
  budgets. The TypeScript SDK also gives `session.resume` its budget and the
  `sessionResume/Fork/Stop/RemoveOutcomeUnknown` error kinds.
- The daemon bounds its wait for a worker's stop answer (four stop graces, the
  kill reap deadline, the worker's terminal-commit wait and a margin); a worker
  that stays silent ends the stop or removal with `worker_operation_failed`.

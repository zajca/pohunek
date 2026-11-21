## Unreleased (2026-11-21, daemon follows the manager PATH, #2488)

- A daemon that runs as its systemd user unit with an empty `search_path` and a
  unit that sets no `PATH` re-reads the user manager's `PATH` when it inspects the host, runs a doctor or
  starts or resumes a session, and uses it for executable lookup, the doctor's agent checks, hooks
  and the environment of new agents; see
  [environment resolution](guides/environment-resolution.md). Other variables
  keep their start value, and `refresh-path` still has nothing to refresh there.

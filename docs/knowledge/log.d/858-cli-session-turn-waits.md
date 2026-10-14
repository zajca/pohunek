## Unreleased (2026-10-12, CLI session turn waits, #858)

- `pohunek session wait <target>...` waits for turn events through
  `session.wait_turn` when no legacy flag is given, also with no flags at all:
  `--until` (comma list or repeated, default all of `turn_completed`,
  `agent_blocked`, `approval_required`, `exited`), `--after <cursor>`, and
  `--timeout <duration>` with a unit (`90s`, `5m`; default the daemon default).
  It takes up to 32 targets on one host; targets on different hosts, several
  targets in legacy mode, and legacy flags mixed with turn flags are usage
  errors before any request. `--json` prints the daemon result; the human form
  prints the event or reason, one `progress:` line per target, and a final
  `cursor:` line. Every reason exits 0. The legacy predicate wait is selected by
  any of its flags and is unchanged. Documented in `docs/cli.md`,
  `concepts/sessions`, the `debug-session-runtime` runbook, a manager-agent loop
  in the `agent-skill` guide (regenerated skill artifact), and a note in the
  `hermes-operator` guide that the plugin's wait tool is the short predicate
  wait (regenerated Hermes skill).

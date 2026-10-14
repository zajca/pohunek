## Unreleased (2026-10-10, fork lineage decides withheld environments, #795)

- The runtime sweep decides macOS 27 platform binaries by fork lineage instead
  of listing each as an unreadable candidate. The kernel serves a boot-wide
  creation number for every same-user process, withheld environment or not, and
  records the creation number of the creator. That creator number is rewritten
  to launchd's when the process re-executes after being reparented, so a
  launchd creator is ambiguous and never excludes a process. The worker journals
  its own number as the optional `worker_spawn_id` (decimal string, no schema
  bump), and the daemon gives it to every journal-backed sweep only when the
  journal's boot identity is the current boot's. A process the worker reached
  through its creators is reaped like a marked one; a process created before the
  worker, or under an older non-launchd creator, is ignored and not reported;
  every other hidden process (created after the worker by launchd, a re-executed
  orphan, one whose creator exited after the worker started) stays an
  unreadable candidate and the cleanup unconfirmed. After a lost runtime on
  macOS 27 the runtime's own processes are reaped without
  `--accept-unconfirmed-cleanup`; the flag may still be needed for the
  remaining candidates, which the refusal lists by pid and command name, and
  for a runtime whose worker journaled no creation number (started by the
  previous release). Linux keeps its start-time ordering. Documented in the
  `debug-session-runtime` runbook, `concepts/sessions`, `concepts/architecture`
  and `docs/cli.md`.

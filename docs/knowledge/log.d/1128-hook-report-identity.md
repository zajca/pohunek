## Unreleased (2026-10-21, hook report identity and retryable admission, #1128, #1150)

- Two sessions that fired the same hook event in the same millisecond lost one
  notification, because the managed Claude and Codex hooks keyed it by event
  and timestamp only. The key is now
  `hook:<agent>:<session id>:<event>:<event id>`, and an event without a payload
  id gets a random per-invocation id that the worker route, the daemon fallback
  and every retry share. The store's merge rule is unchanged; the fix ships in
  the scripts, which `pohunek service upgrade` reinstalls.
- A hook report refused only because the session's worker was unreachable is no
  longer lost. `notification.create`, `session.report_agent`,
  `session.release_agent` and `session.report_native_id` accept an optional
  `retryable_admission`; with it the daemon waits 250 ms instead of the worker
  connect deadline and answers `hook_schema_unavailable`, and the managed hooks
  retry for up to the hook timeout minus 2 s. Daemons announce it with
  `POHUNEK_HOOK_ADMISSION_RETRY` in a session's environment; reports of an
  older session carry no new field. The hooks' `daemon_refused` trace now
  fires for a daemon error (it checked the wrong key before), and
  `daemon_retry` and `daemon_retry_exhausted` are new.

## Unreleased (2026-10-15, scripted retirement waits for the process to end, #1156)

- The daemon tests' scripted supervisor ends a retired job's process the way
  a real retirement does: the call returns once the process no longer runs
  (gone, or an unreaped zombie of the test process), not once `kill -KILL`
  has been sent. `a_partial_retirement_keeps_the_remove_intent_for_reconciliation`
  failed about 1 in 10 runs because the restarted registry still saw the
  killed worker running and, correctly, kept the removal for its background
  re-check.
- Product behaviour is unchanged. A removal intent whose worker still runs at
  startup is `conflict` after `reconcile_workers()` and is finished by the
  background re-check once the worker is gone.

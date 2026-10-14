## Unreleased (2026-10-12, coalition exclusion in the runtime sweep, #821)

- The worker journals its kernel coalitions as the optional `worker_coalitions`
  (`<resource>:<jetsam>`, decimal; additive, no journal schema bump, so a worker
  of the previous release lacks it). On macOS the sweep uses them for a hidden
  process whose fork lineage is open (a launchd creator number, or a creator
  that exited): when the kernel puts the process in a resource and a jetsam
  coalition that both differ from the worker's, it cannot have descended from
  the worker and is ignored, not reported. A launchd job, a system helper such
  as `mdworker_shared` or `biomesyncd`, or another session's agent started after
  the worker therefore no longer leaves a removal or a lost-runtime cleanup
  unconfirmed. Equal, unreadable or missing coalitions prove nothing and keep
  the process a candidate; a lineage descendant whose coalitions differ is
  skipped, never selected. Nothing is signalled that was not before.

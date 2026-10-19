## Unreleased (2026-10-19, exit-callback process evidence, #1107)

The exit callback of an observed agent reconciles with the same cached
process evidence as the scan that cached it: descendant pids read
inconclusive keep preserving a hook-reported claim across an unrelated
agent's exit until a scan can read them, and a confirmed observed exit
invalidates the exact matching lineage identity, so a claim proven through a
surviving wrapper or a direct launch root releases or is replaced instead of
staying bound to a dead reporter. See [session identity](concepts/sessions.md).

Verified process generations are retained independently of ancestry uncertainty.
Confirmed death or PID reuse invalidates the old claim before uncertain ancestry
or foreground probes can preserve it, and ancestor exits recompute uncertainty.
Fresh hook reports and automatic observations bind only verified identities;
probe errors alone neither refute nor bind a generation.
Uncertainty carries its candidate generation, so an exact bound-process exit
can invalidate the old claim without changing a replacement's inconclusive
evidence. Confirmed exits invalidate snapshots collected before the callback.

Process evidence retains its acquisition order across scans, callback delays and
hook updates. An older snapshot cannot refute a newer claim, and a fresh callback
does not make its cached snapshot fresh. Repeated hooks for the same identity
keep the verified generation through uncertainty; only newer conclusive evidence
can invalidate it. An explicit release also fences snapshots collected before it.

Hook admission and conclusive liveness have separate sequence fences. Unknown
observations update the cache but never advance a bound claim's proof of life;
a queued exact exit still applies after an inconclusive scan or a same-identity
hook refresh. Only newer conclusive own-process evidence supersedes it, even
when an ancestor remains unreadable.

## Unreleased (2026-10-19, descendant agent claims, #1107)

Procwatch keeps a hook-reported agent PID and active native reference when the
reporter is not an observed agent itself, such as the Codex binary below its
`node` wrapper, or is an observed agent below the agent the foreground selects
or below an agent launch root. A scan that proves the reporter (it runs, keeps
the claim's start identity, is not owned by another session, and descends from
an observed same-runtime agent or an agent launch root without another runtime
in between) binds the claim to that process generation. A bound claim ends only
with the exit of that generation: failed collections, failed probes, a reporter
missing from a scan and the exit of other agents leave it in place, and a hook
refresh keeps the binding. A claim whose probe is inconclusive stays up to the
claim TTL. See [session identity](concepts/sessions.md).

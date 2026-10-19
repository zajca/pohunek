## Unreleased (2026-10-19, descendant agent claims, #1107)

Procwatch keeps a hook-reported agent PID and active native reference when the
reporter is not an observed agent itself, such as the Codex binary below its
`node` wrapper, and one scan proves it: the reporter runs, keeps the claim's
start identity, is not owned by another session, and descends from an observed
same-runtime agent (or from an agent launch root) without another runtime in
between. A scan that cannot prove it, an exit notification, a failed collection
and a scan collected before the claim was admitted decide as before. See
[session identity](concepts/sessions.md).

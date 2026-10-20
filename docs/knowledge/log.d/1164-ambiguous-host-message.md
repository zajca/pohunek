## Unreleased (2026-10-20, ambiguous host message, #1164)

`overlay_host_ambiguous` now lists the hosts a selector matched, for example
`host 'build' matches 2 hosts on overlay netbird: this machine (build.a.test),
peer build.b.test`, instead of the overlay list in debug formatting. The
`recover` text names the selector of each host: `<overlay>:<selector>` for a peer, `local` for this machine.
The error code is unchanged. See [sessions](concepts/sessions.md).

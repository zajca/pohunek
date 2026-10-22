## Unreleased (2026-10-22, session commands pin the host, #1391)

Every `session <action> <name>` command (`inspect`, `stop`, `fork`, `rm`,
`input`, `screen`, `detection`, `read`, `resume`, `resize`, `metadata`, `rename`,
`diff`) looks the remote peer up once, so the session-name lookup and the action
reach the same peer; a peer that cannot be pinned fails with
`host_identity_unpinnable`. See [remote hosts](guides/remote-hosts.md).

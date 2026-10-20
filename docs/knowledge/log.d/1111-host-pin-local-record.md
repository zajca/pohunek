## Unreleased (2026-10-20, host pin and local host record, #1111)

A remote `--host` is resolved once per operation that opens several connections
(`attach` with its re-attach, status line and menu actions, `session output`,
`session wait`, `assistant`) and pinned to the peer identity it selected; a peer
with no peer id and no FQDN fails with `host_identity_unpinnable`. `host list
--json` and `host discover --json` list this machine as a record with `local:
true`, and `pohunek_client::Client::connect` maps this machine's own overlay
name to the local daemon. See [remote hosts](guides/remote-hosts.md).

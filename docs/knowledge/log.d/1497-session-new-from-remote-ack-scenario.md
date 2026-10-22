## Unreleased (2026-10-22, session new from remote with prompt acknowledgement, #1497)

A CLI scenario now runs `pohunek session new --from-remote
--input-ack-timeout-ms` together against a fixture daemon: the JSON document
carries both `worktree_source` and `prompt_delivered`, the human output prints
both lines, the two capability checks share one `host.inspect`, and a host
lacking either capability is refused before a session is created.

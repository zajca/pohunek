## Unreleased (2026-10-14, agent process tree trust boundary, #1007)

- The architecture trust boundary, `docs/public-api.md`, the sessions concept,
  the runtime packages guide and the trust model state that the agent's PTY
  subtree runs as the owner and is not an identity boundary: under
  `NamedForProvider` (`identity-subagent-v1`) any process the launch process
  starts can file the launch process's report, and only `FrozenRootEntrypoint`
  (`identity-v1`) binds the root's report to the root's own kernel peer.
- Authenticated agent identity is tracked in #52 and isolated runtime profiles
  in #88; an assigned Claude launch id is #1036 and the Codex evaluation #1037.

## Unreleased (2026-11-26, shell-session agents are not attributed through the transcript store, #2531)

- `docs/public-api.md` and `docs/knowledge/guides/runtime-packages.md` (Muse
  and Cline transcript sections) state that only an agent outside any Pohunek
  session is attributed through a `[transcripts]` store. A foreground agent
  inside a shell session is not: the session reports its own active agent, and
  `rescan_external_agents` in `crates/daemon/src/session/mod.rs` skips every
  process a live session owns before it reaches the transcript match.

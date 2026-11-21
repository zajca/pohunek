## Unreleased (2026-11-21, one credential per account over a shared config home, #1562)

- [Agent profiles](concepts/agent-profiles.md) gains "Choosing a credential with
  a launch option": a launch option whose choices set a non-secret selector, over
  a wrapper program that reads the credential from an owner file. Pohunek stores
  only the selector; a resume re-reads whatever the wrapper reads, so a replaced
  credential file switches identity without `agent_profile_changed`.
- The `config_home_id` entry now says an equal value means the same config
  directory, not the same account; clients show `display_name` and the session's
  `agent_options` for the account.
- [Claude Code runtime package](guides/claude-package.md) gains "One Claude
  subscription per session": `claude setup-token` and `CLAUDE_CODE_OAUTH_TOKEN`,
  the credential precedence, bare mode ignoring the token, no Remote Control or
  claude.ai connectors, the token inherited by subprocesses and the scrub
  trade-off, and the rule to choose the account by whose work it is, never by
  quota. Automatic switching on limits, quota strategies, capacity routing and a
  token-swapping proxy are not supported.
- [Secrets](safety/secrets.md) states where such a token lives and that
  hand-written profile `[env]` entries are not refused for credential names.
- A core launch-time credential reference is tracked in
  [#2504](https://github.com/zajca/pohunek/issues/2504).

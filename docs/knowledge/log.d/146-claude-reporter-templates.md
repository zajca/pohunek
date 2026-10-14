## Unreleased (2026-10-12, Claude reporter templates, #146)

- The Claude reporter scripts are core-owned templates with the
  `@POHUNEK_AGENT_ID@` and `@POHUNEK_AGENT_NAME@` placeholders, rendered by
  the Claude handler from the runtime id `claude` and the name `Claude` like
  the Codex scripts. The installed bytes are unchanged (recorded in
  `crates/daemon/src/integration/reporter_golden/claude-*.sh`), so installed
  integrations stay current. Documented in the runtime packages guide.

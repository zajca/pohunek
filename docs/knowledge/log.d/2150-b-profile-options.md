## Unreleased (2026-11-14, profile launch options, #2150)

- Host agent profiles accept `[options.<id>]` tables: closed choices with a
  label, a default and per-choice `args`, `env` and `min_runtime`. The selected
  choices' args follow the profile args in option-id order and their env joins
  the profile env; choice env keys, values and args pass the load rules of
  [Agent profiles](concepts/agent-profiles.md#launch-options).
- `session.new` takes `agent_options` (behind
  `session_new_agent_options_supported`; the Rust client and the TypeScript SDK
  refuse with `session_new_agent_options_unsupported` and send nothing) and
  refuses an undeclared option or choice (`agent_option_unknown`,
  `agent_option_choice_unknown`, `agent_options_not_declared`) or an
  unavailable floored choice (`agent_option_choice_unavailable`).
  `pohunek session new --opt OPTION=CHOICE` sends it.
- Sessions record the resolved choices (`SessionInfo.agent_options`, store
  schema 20) and resume with them; the profile revision covers only the
  selected choices, and a removed recorded choice is `agent_option_choice_missing`
  even with `--accept-profile-change`.
- `host.inspect` reports `AgentRuntime.options` with each choice's availability,
  and `pohunek plugin profile show <name> [--json]` prints them.

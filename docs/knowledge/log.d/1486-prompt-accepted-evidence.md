## Unreleased (2026-10-23, prompt-submit delivery evidence, #1486)

- A Claude session now reports each submitted prompt to its worker from a
  `UserPromptSubmit` hook (state hook action `prompt-submit`). The report is
  delivery evidence for a waited `session.input`, never activity: it moves no
  activity revision, emits no `agent_state` event and opens or closes no turn.
  The hook drains the prompt without decoding it, prints nothing, exits 0 on
  every path and makes one worker attempt with no daemon fallback. The worker
  applies it only from the session's launch process (a nested `claude -p`, other
  providers and a shell-hosted Claude are refused) and only with a sequence
  ahead of the last applied one; the sequence stays in memory.
- `session.input` gains the optional `wait.allow_prompt_accepted` and the
  result field `prompt_accepted`. An opted-in `working` wait is answered by an
  acceptance that started after the worker wrote Enter, for the same runtime and
  before the deadline, when the agent was not `working` at the boundary; the
  result then carries `prompt_accepted: true` and no activity, source or
  revision. An acceptance never answers `idle`. `session new
  --input-ack-timeout-ms` counts it as the acknowledgement. The CLI always opts
  in and prints `prompt_accepted=true source=prompt-submit-hook`. A daemon that
  predates the field answers on activity alone; a protocol 3 connection drops
  the option. Without any evidence the wait ends in `session_input_timeout`
  whose message names both kinds of evidence.
- Worker protocol 8 adds `WriteAck.submit_mark_ns` and
  `InspectSnapshot.prompt_accepted_sequence`; a version-7 connection sees
  neither. Hook schema `identity-subagent-v1` gains the `prompt_accepted`
  action for Claude. The integration version is 15 for every managed hook asset:
  reinstall the Claude and Codex hooks (`pohunek integration install`, or
  `pohunek service upgrade`); until then `integration.status` reports them
  `outdated` and waits get no acceptance.

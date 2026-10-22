## Unreleased (2026-10-22, waited input on delayed-submit agents, #953)

`session input --until` no longer fails with `session_input_wait_unsupported`
for Codex, Claude Code, and Hermes. The daemon types the text, waits out the
submit delay, re-checks the agent, and only then writes Enter. A success reports
`submitted: true`; `--until working` confirms the agent started on the input,
and an `idle` target counts only after `working` followed the submit. A blocked
agent, an elapsed deadline, or a shutdown before Enter fails with the new
`session_input_not_submitted`; the typed text may remain in the input box.

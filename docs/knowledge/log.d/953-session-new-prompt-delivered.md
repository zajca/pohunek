## Unreleased (2026-10-22, session new prompt delivery, #953)

`pohunek session new --input-ack-timeout-ms <1..8000>` (with `--input` or
`--input-stdin`) reports whether the agent started working on the initial
input: `--json` gains a top-level `prompt_delivered`, the human output prints
`prompt delivered: yes/no`, and the session is kept either way. The daemon
advertises `session_new_input_ack_supported` in `host.inspect`; a host without
it is refused before the session is created. The Rust and TypeScript clients
extend the `session.new` response budget by the window.

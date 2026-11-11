---
kind: fixed
---
`pohunek session input --until working` no longer confirms an input on process-sourced activity. On a Claude, Codex or Pi session the startup banner or the composer repainting the typed text made the call answer `submitted: true` while the text still sat unsent on the prompt line. Only program status (OSC 7501), hook report, title or screen evidence after the write, or a prompt acceptance, confirms the input; otherwise the call fails with `session_input_timeout`, and `session new --input-ack-timeout-ms` reports `prompt_delivered: false`. Sessions whose manifest declares no program status source keep the process signal.

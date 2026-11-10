---
kind: removed
breaking: true
---
Claude Code below 2.1.309 is no longer supported. The Claude runtime package is version 2.0.0 and accepts `[2.1.309, 3.0.0)`: a launch or resume with an older `claude` is refused with `agent_runtime_unsupported`, and `pohunek service upgrade` reports a stopped session whose installed Claude is older as unable to recover until a supported release is installed or `--accept-runtime-loss` is given. Every supported Claude reports OSC 7501 program-status records, so the Claude detection manifest no longer declares `authority_from` (its records are authoritative for every release) and drops the title, status-line and completed-turn rules that served older releases; the prompt box and every dialog rule stay.

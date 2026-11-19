---
kind: fixed
---
A first prompt of `session.new` that is a single bare word is typed into the agent after readiness instead of being appended to the launch arguments of a `prompt_arg` runtime, where the agent CLI could dispatch it as a subcommand (for example codex `logout`). A bare word is non-empty and holds only ASCII letters, digits, `-`, `_`, `.` and `:`; any other prompt keeps its delivery. ([#2433](https://github.com/zajca/pohunek/issues/2433))

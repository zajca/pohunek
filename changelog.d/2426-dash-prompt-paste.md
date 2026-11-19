---
kind: fixed
---
A first prompt of `session.new` that starts with `-` is typed into the agent after readiness instead of being appended to the launch arguments of a `prompt_arg` runtime, where the agent could read it as an option. ([#2426](https://github.com/zajca/pohunek/issues/2426))

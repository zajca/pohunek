---
kind: changed
---
`pohunek session` no longer fails on a runtime inventory status, retention reason or hold, wait or turn-wait reason, detection region or read source that a newer daemon adds: the value reads as `unknown` (an unknown detection region is dropped from the supported list). `session wait` exits non-zero with `session_wait_reason_unknown` when it cannot tell whether the condition matched.

---
kind: fixed
---
`pohunek attach` now exits 0 with `[pohunek] session <id> <state>; attach ended` when the session ends while attached (its shell exits, or it is stopped or removed), instead of reporting `runtime/session_not_running` or retrying a dead session. ([#1184](https://github.com/zajca/pohunek/issues/1184))

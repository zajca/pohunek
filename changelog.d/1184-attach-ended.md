---
kind: fixed
---
`pohunek attach` now ends with exit 0 instead of a `runtime/session_not_running` error when the session ends after a reconnect probe saw it running: the refused re-attach is confirmed with one inspect, and the client prints `[pohunek] session <id> <state>; attach ended`. ([#1184](https://github.com/zajca/pohunek/issues/1184))

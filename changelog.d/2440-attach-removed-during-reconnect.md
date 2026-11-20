---
kind: fixed
---
`pohunek attach` now ends at once with exit 0 when the session is removed while the attach waits to reconnect: as soon as the reconnect probe sees the session gone, the client prints `[pohunek] session <id> removed; attach ended` instead of retrying until `attach reconnect window expired`. A daemon that is unreachable, that is shutting down (`daemon_shutting_down`), or a session that is `starting`, is still retried until the window ends, so an attach keeps waiting across a daemon restart. ([#2440](https://github.com/zajca/pohunek/issues/2440))

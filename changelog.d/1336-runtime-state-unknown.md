---
kind: changed
---
`pohunek session list` and `session inspect` no longer fail on a runtime state that a newer daemon adds: it reads as `unknown`, the session counts as possibly live (`service uninstall` refuses while it exists), and `session stop` and `session rm` refuse it locally with `session_runtime_state_unknown`.

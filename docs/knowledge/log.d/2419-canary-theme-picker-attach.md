## Unreleased (2026-11-19, canary theme picker through attach, #2419)

- The Claude canary `login_screen` scenario answers the first-run theme
  picker through `pohunek attach` (one Enter on standard input, then
  detach) once the session activity is `blocked`, instead of plain
  `session input`, which the daemon refuses while a dialog blocks the
  session. The capture no longer depends on beating the activity
  transition. No daemon, protocol or CLI change.

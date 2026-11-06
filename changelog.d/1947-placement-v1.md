---
kind: added
---
Runtime descriptors can declare how a session stays out of a shared host daemon with a `[placement]` table (runtime feature `placement-v1`), and the session worker reports a process that left the terminal's session as a daemon: `session inspect` shows a `placement` row (`placement_escape` with pid and command name) and `pohunek doctor` a `placement_escape` check. The check is report-only and kills nothing. The Claude Code, Codex, Pi and Hermes packages declare their placement (1.3.0, 1.6.0, 1.2.0 and 1.1.0).

---
kind: changed
---
The Claude Code package guide now documents `CLAUDE_CODE_FORCE_SYNC_OUTPUT = "1"` in a profile `[env]`, which lets clients that draw between PTY reads, such as the Android companion, avoid torn frames. The variable is undocumented by Anthropic and may change.

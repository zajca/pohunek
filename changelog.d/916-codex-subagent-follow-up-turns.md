---
kind: fixed
---
A Codex subagent that finished a turn and got a follow-up task (or was resumed) is now recorded again when that turn ends: its row takes a new revision and finish time, so `session` subagent views and events show the new turn. Before, only the child's first turn changed its row. The row is `running` only between the child's start and its next stop, because Codex fires no hook at the start of every such turn (a `multi_agent_v2` follow-up fires none).

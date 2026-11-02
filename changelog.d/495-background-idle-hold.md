---
kind: fixed
---
Idle stop no longer stops a session that has opted in while its agent still has live background work: a background subagent, workflow, shell, monitor or MCP task listed in the session's `background` snapshot holds the stop, and the idle time counts a full limit from the moment that work ends. The reported `activity` is unchanged (`idle` once the turn ends), teammates and provider-internal tasks do not hold, and Codex sessions behave as before.

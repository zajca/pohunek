---
kind: fixed
---
A session name given to `session.new`, resume or `session.rename` is now refused with `bad_request` when it holds a separator or format character (a bidirectional override such as U+202E, a line separator, a zero-width character), the same rule a background-task label follows; before, only control characters were refused. A name persisted earlier still loads, lists and serves unchanged and a rename replaces it. A `SubagentStart` whose `agent_type` holds such a character is refused by the worker and creates no subagent record, and the daemon drops a journaled agent type that fails the rule on import while keeping the subagent.

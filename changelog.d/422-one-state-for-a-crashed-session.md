---
kind: changed
---
`pohunek session list` and `session inspect` show one state for a session whose runtime was lost, `crashed`, whether or not the loss is proven, with the readable cause (the memory killer and peak, the signal or exit code, the loss of the user service manager, or a host restart) and whether `session resume` can bring it back. The notification for a lost runtime carries the same cause: "Session crashed" for a proven loss, "Session runtime lost" for an unproven one, once per loss, so a runtime that answers again and is lost again is reported again. The TypeScript SDK gains `sessionCondition()` with the same rule. `--json` output is unchanged.

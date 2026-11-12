---
kind: fixed
---
`project candidates --sources history` no longer charges a session's auxiliary directory (a Claude conversation's `subagents/`) against the history directory bound, so a history with many such sessions no longer reports `entries` and drops repositories in later project directories.

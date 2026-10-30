---
kind: added
---
`pohunek store rejected list` shows the `metadata.jsonl` lines no loader accepts (kind, line number, reason, whether they may pin a package) and `pohunek store rejected set-aside <content-hash>` moves one into the owner-private `metadata.rejected.jsonl` with an audit record; a line that may own a live runtime, or a `resume` line whose pin cannot be read, needs `--accept-pin-loss`. The methods `store.rejected_list` and `store.rejected_set_aside` are local-only and new in protocol 4. `pohunek doctor` reports rejected lines and names the command, and the offline `plugin ensure` path judges a kept rejected line like the daemon gates: it never reads a digest from it, reports the plan as uncertain and fails closed where a pin may be lost.

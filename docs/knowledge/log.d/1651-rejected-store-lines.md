## Unreleased (2026-10-30, rejected metadata-store lines, #1651 #1652)

- `pohunek store rejected list` and `pohunek store rejected set-aside <content-hash>` (RPC `store.rejected_list`, `store.rejected_set_aside`, local-only) list the `metadata.jsonl` lines no loader accepts and move them into the owner-private `metadata.rejected.jsonl` with an audit record. A line that may belong to a live runtime needs `--accept-pin-loss`. New error codes: `store_rejected_line_not_found`, `store_rejected_line_live`, `store_quarantine_failed`, `store_read_failed`.
- `pohunek doctor` reports `metadata_store_rejected_lines` with the count and the command, never a line's text.
- The offline runtime step (`ensure`) no longer drops the pin of a kept rejected line: it judges every installed version while a line that may pin a package cannot be read, and its refusals name `pohunek store rejected list`.

---
kind: changed
---
`session.input` to a Pi session that reads `blocked` is refused with `session_input_blocked`, and the error now carries a hint to answer the dialog in an attached terminal (`pohunek attach`). A runtime descriptor can declare this with `[input] refuse_while_blocked` (Pi package 1.0.5; Claude and Codex keep delivering text). The store schema is now 12: a daemon of the previous release refuses the newer store, so roll back with the `metadata.jsonl.pre-schema-11` backup.

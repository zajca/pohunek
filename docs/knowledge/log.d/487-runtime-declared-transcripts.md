## Unreleased (2026-10-12, runtime-declared transcripts, #487)

- A runtime descriptor may declare `[transcripts]` (`parser`, a compiled parser
  id, and `dir`, a plain relative path below `[config_home]`); the external
  observer takes each transcript root and its parser from it instead of naming
  Claude and Codex. The built-in and official `codex` and `claude` descriptors
  declare `jsonl-head-v1` with `sessions` and `projects`, so the observed roots
  are unchanged; a runtime without the table (Pi) contributes no root, and a
  tree that equals or nests with another runtime's is attributed to neither
  (warning `external.transcript_roots.ambiguous`, code
  `transcripts_ambiguous_owner`). An
  unknown parser, an unsafe `dir` or a table without `[config_home]` is
  `descriptor_invalid`. Documented in the `runtime-packages` guide, the
  `sessions` and `agent-profiles` concepts and the public API.

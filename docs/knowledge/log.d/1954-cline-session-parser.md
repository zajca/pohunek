## Unreleased (2026-11-25, Cline session manifests as transcripts, #1954)

- The runtime feature `transcripts-cline-session-v1` adds the transcript parser
  `cline-session-v1`: it reads `<id>/<id>.json` below `[transcripts] dir`, takes the
  id from `session_id` (equal to the directory name, no `__`) and the working
  directory from an absolute `cwd`, within 64 KiB, and never opens
  `*.messages.json`. The walk lists the highest directory names first (the
  `dated_depth` of the tree is 1). File watchers pass a path on when any compiled
  parser reads its shape (`TranscriptParser::any_reads`) and the owning root decides
  with `TranscriptParser::reads`, so `.json` manifests reach a Cline root without
  `.json` files entering a `jsonl-head-v1` root. See
  [runtime packages](guides/runtime-packages.md#cline-session-manifests).

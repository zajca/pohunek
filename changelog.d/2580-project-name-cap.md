---
kind: changed
---
Custom project names (`project add --name`, `project rename`) are capped at
`MAX_PROJECT_NAME_BYTES` (128 UTF-8 bytes, `crates/protocol/src/limits.rs`, shared
by the daemon's `validate_project_name` and the CLI's pre-dial check). An over-cap
name is refused with the typed `bad_request` error naming the cap and the given
length — never truncated or echoed. The cap keeps one project record far below
the 1 MiB `MAX_CONTROL_LINE_BYTES`, so a single maximal name can no longer make
the whole `project.list` walk fail even at page limit 1; before this change the
same record was a framing error. Compatibility: custom project names longer than
128 bytes were accepted before and are refused now — rename them shorter.
([#2580](https://github.com/zajca/pohunek/issues/2580))

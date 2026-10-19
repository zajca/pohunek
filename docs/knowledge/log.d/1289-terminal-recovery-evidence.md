## Unreleased (2026-10-19, failed-exec terminal recovery evidence, #1289)

A failed `pohunek service lock` exec retains the original spawn error and the
`service_io_failed` error code, with the terminal owner PGID, initial ownership,
foreground PGIDs before/after recovery, and the result or numeric errno of
recovery operations. A skipped operation is identified rather than presented
as success. The existing ownership checks, single handback attempt, and command
exit behavior are unchanged; this records evidence for the intermittent macOS
recovery failure rather than claiming it fixed. See `docs/cli.md`.

## Unreleased (2026-10-15, hook stderr survives a daemon restart, #1046)

- A process a lifecycle hook leaves running with the hook's stderr inherited
  keeps a readable stderr across a daemon restart. Two seconds after the hook
  ended, the daemon hands the stderr pipe to a drain process, `/bin/cat` run by
  absolute path with the pipe as stdin and `/dev/null` as stdout and stderr, in
  its own process group, with a cleared environment and no other inherited
  descriptor. The daemon closes its read end, reaps the drain when the last
  writer is gone, and logs the hand-off at DEBUG with the hook's event and
  script and the drain's PID.
- A restart while the hook runs, or within those two seconds, still gives such
  a process `SIGPIPE` on its next stderr write.
- If the drain cannot start, the daemon keeps discarding the stderr itself and
  logs a WARN naming the hook.

## Unreleased (2026-10-15, echo held by an attach or resize output pause, #1133)

- An attach handshake and a resize suspend the session terminal's output
  (`tcflow` `TCOOFF`) while they take the terminal snapshot boundary. On
  Linux the line discipline keeps the echo of input it accepts during that
  pause in its pending echo buffer, and resuming output does not emit it. The
  echo then waited until the child itself read or wrote the terminal, which a
  child that does not read its input never does. The worker now writes zero
  bytes to the terminal after every resume, including the last-resort resume
  of an abandoned pause, which emits the held echo. A failed write logs
  `worker.pty.held_echo_release.failed`.
- This was the cause of the flaky
  `worker_environment::a_stalled_attach_write_keeps_its_stream_while_control_input_fails_with_input_stalled`:
  its observer attached while the stalled attach write was being echoed.

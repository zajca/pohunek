## Unreleased (2026-10-15, attach output while the stream's own input is pending, #1021)

- An attached terminal froze while its own input waited for the program to
  read it, for example a paste larger than the terminal's input queue into a
  program that does not read. The worker waited for the PTY write inside the
  attach stream's loop and forwarded no output meanwhile. A paste larger than
  the socket buffers made it permanent: the daemon's attach bridge and the
  CLI attach client both waited on their own input write inside the loop that
  forwards output, so each side blocked writing to the other.
- The worker, the daemon bridge and the CLI attach client now keep their
  input write pending across loop iterations and forward output meanwhile.
  Each reads further input only once its pending write completed, which
  keeps the input in order and bounded. An attach write runs in a task of its
  own, so it keeps the PTY execution lock until every byte is accepted even
  if its stream ends first, and a control input waiting behind it keeps its
  stall bound. The CLI sends the detach request for a detach shortcut once
  the input typed before it is written, showing output meanwhile.

## Unreleased (2026-10-15, service lock terminal handoff before exec, #554)

- `pohunek service lock` run in the foreground of a terminal makes its child
  take the terminal for the child's process group before it executes the
  command, as the child of a job-control shell does, and makes the same
  handoff from the lock process after the spawn. A command that reads the
  terminal at once is no longer stopped by `SIGTTIN` before the handoff. The
  lock process took that stop for a job stop, gave the terminal back to its
  own group and stopped itself. A caller without job control, such as a
  script run in a terminal, never continued it, so the lock hung. This was
  the CI timeout of
  `service_lock_terminal::ctrl_z_stops_the_job_and_sigcont_resumes_it_with_the_terminal`.
- When the command cannot be executed, the lock process takes the terminal
  back from the child's group, which has no member left.
- The `test-util` build holds the lock process between the spawn and its own
  handoff while `POHUNEK_TEST_SERVICE_LOCK_HANDOFF_HOLD` names a FIFO.
  Release builds do not contain that hold.

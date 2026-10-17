## Unreleased (2026-10-17, row launchd guard leaves Spotlight application workers out, #1246)

- `scripts/release-workflow/run-rows` leaves the Spotlight metadata workers of
  the shared and the application kind (`com.apple.mdworker.shared.<UUID>`,
  `com.apple.mdworker.application.<UUID>`) out of the launchd comparison, in
  both directions. A macOS row no longer fails when Spotlight starts or retires
  such a worker during the row; any other added or removed job still fails it.
- The program of an added launchd job is looked up in the process's, the login
  session's (`gui/<uid>`) and the user's (`user/<uid>`) domain, and the job log
  names every added job with its label and program path, or "not described",
  never another field of `launchctl print`.

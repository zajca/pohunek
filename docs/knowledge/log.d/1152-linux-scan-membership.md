## Unreleased (2026-10-15, release layout scan membership on Linux, #1152)

- The Linux release-layout scan (`pohunek_processes` in
  `crates/cli/tests/support/release_layout.rs`) decides that a process
  belongs to the scenario's scope by its executable or working directory
  below the scope, or by a value of its environment below it. The
  environment alone missed a child that `spawn` had just returned:
  `/proc/<pid>/environ` reads empty between the point where `execve` closes
  the spawner's close-on-exec pipe and the point where the new image's
  environment is set up. A probe saw it empty in 553 of 3000 spawns. The miss
  showed as a local full-gate flake of
  `guards::a_running_pohunek_process_outside_the_layout_is_refused`.
- That scenario starts its impostor with an empty environment, the state a
  scan can observe during `execve`. It fails deterministically without the
  executable and working-directory membership.
- Product behaviour is unchanged.

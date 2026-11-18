## Unreleased (2026-11-17, plugin kit commands, #1613)

- The `pohunek-plugin-kit` binary covers all six plugin kit operations over
  one package directory: `lint`, `build`, `verify`, `detect-check`, `smoke`
  and `prove`. `smoke --release-dir <dir> [--upstream <path>]` installs the
  built package on an isolated daemon through a throwaway index, starts it,
  waits for detection past `unknown`, requires a non-empty screen, then stops
  and removes the session; `--upstream` puts a locked install in front of the
  isolated daemon's `PATH`. `cargo xtask plugin build` delegates to the kit
  logic, keeping its exact contract including exit codes.

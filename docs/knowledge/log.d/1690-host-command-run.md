## Unreleased (2026-10-28, host command run CLI, #1690)

- `pohunek host command run <host> <package> <command> [--timeout-ms <MS>] [--max-output-bytes <N>] [--json]` calls `command.run` on a named host. It prints the program's stdout verbatim (or the full reply with `--json`) and exits 0; each `command_*` error exits with its own status (40 to 51), and the CLI offers no argument, environment, working-directory or input option. See [Runtime Packages](guides/runtime-packages.md#calling-a-command).

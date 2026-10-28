---
kind: added
---
`pohunek host command run <host> <package> <command>` runs a command an installed package declared, on the local daemon or an overlay peer, with `--timeout-ms`, `--max-output-bytes` and `--json`. It prints the program's stdout verbatim and exits 0; each `command_*` error exits with a status of its own (40 to 51, listed in `docs/cli.md`), and `command_unsupported` (40) reports a daemon without the method. The command takes no argument, environment variable, working directory or input.

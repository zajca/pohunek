## Unreleased (2026-10-30, package command follow-ups, #1708)

- `pohunek host command run` exits 52 for a `command_*` error code it does not know, so a script can tell a newer daemon's refusal from an unreachable host (exit 1). Every known command error keeps its own status in 40 to 51.
- A descriptor with several faults reports the schema or runtime API fault before a missing `requires` entry, as it did before packages could export commands.
- Unicode format characters in a command's arguments are accepted by the descriptor checks; a client that shows the arguments for consent escapes them.

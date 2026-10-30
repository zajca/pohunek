---
kind: added
---
`pohunek service refresh-path [--dry-run] [--accept-runtime-loss] [--json]` re-records the executable search path from your login shell, as install would record it now, prints the directories it adds and removes, and restarts the daemon with the new path. Use it after installing an agent prefix later or after a Nix profile update moved the profile link. The path in `service.toml` and the daemon job change together in one journaled transaction or not at all; only the daemon restarts, worker jobs keep running, and live sessions the new daemon would not adopt refuse the run unless `--accept-runtime-loss` is given. A login shell that fails, hangs or prints junk never replaces the recorded path.

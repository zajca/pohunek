---
kind: added
---
`pohunek service refresh-path [--dry-run] [--json]` re-records the executable search path in `service.toml` from your login shell, as install would record it now, and prints the directories it adds and removes. Use it after installing an agent prefix later or after a Nix profile update moved the profile link; `service check` and `service upgrade` judge with the new path at once, without stopping or restarting anything. A login shell that fails, hangs or prints junk never replaces the recorded path.

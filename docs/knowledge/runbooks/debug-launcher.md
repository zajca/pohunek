---
type: Runbook
id: runbook/debug-launcher
title: Debug launcher behavior
description: Diagnose launcher problems by separating scripts, config, daemon health, and project action resolution.
source_kind: manual
intents: [debug, setup, help]
since: 0.3.3
---

# Debug Launcher Behavior

Use this runbook when a launcher keybinding or menu does not start the expected
session.

1. Verify daemon health with `pohunek health --json`.
2. Reinstall or verify launcher scripts with `pohunek setup scripts`.
3. Check launcher config setup with `pohunek setup config`. Do not use `--force`
   unless the user wants existing files overwritten.
4. Print or update sway integration with `pohunek setup sway --print` or
   `pohunek setup sway`.
5. If the launcher targets a project action, run
   `pohunek project actions <id-or-label> --json` and
   `pohunek project action <id-or-label> <action> --json`.
6. If a session starts but does not appear where expected, use
   `pohunek session list --json` and `pohunek session inspect <target> --json`.

Keep launcher diagnosis layered: first daemon health, then installed assets, then
project/action resolution, then session state.

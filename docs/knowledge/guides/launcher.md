---
type: Guide
id: guide/launcher
title: Launcher setup
description: Install and verify the local launcher scripts, config, and sway keybindings.
source_kind: manual
intents: [setup, debug, help]
---

# Launcher Setup

The launcher integration is local filesystem setup. It writes scripts, default
configuration, and an optional sway drop-in.

Use the split setup commands when diagnosing or applying changes:

1. `pohunek setup scripts` materializes launcher scripts into the data directory
   bin path.
2. `pohunek setup config` writes default launcher configuration and prompt
   templates without overwriting existing files unless `--force` is used.
3. `pohunek setup sway` writes the sway drop-in, or `pohunek setup sway --print`
   prints the snippet for manual review.

After setup, verify daemon health and project/action resolution before blaming
the launcher UI. The launcher ultimately depends on the same daemon, project,
session, and action surfaces described in [sessions](../concepts/sessions.md)
and [projects](../concepts/projects.md).

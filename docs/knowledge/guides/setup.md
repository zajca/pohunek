---
type: Guide
id: guide/setup
title: Local setup
description: Configure a local Pohunek host enough to run daemon-backed sessions and launcher integration.
source_kind: manual
intents: [setup, help]
---

# Local Setup

Start with structured inspection:

1. Run `pohunek doctor --json` to check local binaries, socket paths, and
   writable state directories.
2. Run `pohunek daemon start --detach` if the daemon is not running.
3. Run `pohunek health --json` or `pohunek status --json` to confirm the daemon
   responds.
4. Run `pohunek host inspect local --json` to inspect local capabilities and
   available runtimes.

Setup assets are installed through `pohunek setup`. The subcommands split the
work into launcher scripts, config templates, and sway integration:

- `pohunek setup scripts`
- `pohunek setup config`
- `pohunek setup sway`

Do not overwrite existing user config unless the user asks for that behavior and
the command supports it. For launcher details, see [launcher](launcher.md). For
profile and secret boundaries, see [agent profiles](../concepts/agent-profiles.md)
and [secrets](../safety/secrets.md).

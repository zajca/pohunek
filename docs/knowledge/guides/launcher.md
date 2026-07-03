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

`launcher.conf` includes attach banner keys. When `banner=true`, the attach
client reserves the top terminal row for a status banner (session state,
activity, and the Ctrl-\ kill shortcut) and composites the agent screen below
it. Unlike the previous scroll-region overlay, the client now parses the agent
byte stream into its own screen model and re-renders the terminal itself, so the
banner works even under full-screen TUI agents such as Codex and Claude Code:
their cursor modes, scroll margins, and repaint timing stay inside the parsed
grid and never fight the banner. Because the client composites the whole screen
while the banner is on, the terminal's native scrollback is unavailable during
attach; leave `banner=false` (the default) to keep plain passthrough with native
scrollback. `banner_interval_seconds` caps the refresh cadence: `0` uses the
built-in ~60fps frame cadence, and a positive value throttles repaints to a
coarser refresh. The rofi/sway switcher only opens marked attach terminals; it
does not create a separate banner window.

Attach terminals automatically retry after an unexpected daemon stream close.
`attach_reconnect_seconds` controls the retry window, and
`attach_reconnect_interval_seconds` controls the poll interval. This only helps
sessions that the restarted daemon can resume from native agent metadata; live
PTYs and plain shell processes still do not survive a daemon restart. Set
`attach_reconnect_seconds=0` to disable the retry behavior.

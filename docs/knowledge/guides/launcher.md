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

`launcher.conf` includes attach banner keys, but the in-terminal overlay is
currently disabled at runtime. The previous scroll-region overlay interfered
with full-screen TUI agents such as Codex and Claude Code because they own the
same cursor modes, scroll margins, and repaint timing. While the overlay is
disabled, `banner=true` and `banner_interval_seconds` do not reserve a terminal
row, draw a banner frame, resize the attached PTY, or enable the Ctrl-\ banner
kill shortcut. The rofi/sway switcher only opens marked attach terminals; it
does not create a separate banner window.

Attach terminals automatically retry after an unexpected daemon stream close.
`attach_reconnect_seconds` controls the retry window, and
`attach_reconnect_interval_seconds` controls the poll interval. This only helps
sessions that the restarted daemon can resume from native agent metadata; live
PTYs and plain shell processes still do not survive a daemon restart. Set
`attach_reconnect_seconds=0` to disable the retry behavior.

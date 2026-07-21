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
   templates (`issue.tmpl`, `pr.tmpl`, `review.tmpl`) without overwriting
   existing files unless `--force` is used. `review.tmpl` is GUI-only
   (Track D.6, see [GUI setup](gui.md#review)): the shell launcher scripts
   render `issue.tmpl`/`pr.tmpl` themselves, but `pohunek-gui` reads and
   renders `review.tmpl` directly to build a review-dispatch session's
   prompt.
3. `pohunek setup sway` writes the sway drop-in, or `pohunek setup sway --print`
   prints the snippet for manual review.

After setup, verify daemon health and project/action resolution before blaming
the launcher UI. The launcher ultimately depends on the same daemon, project,
session, and action surfaces described in [sessions](../concepts/sessions.md)
and [projects](../concepts/projects.md).

`launcher.conf` includes attach banner keys. When `banner=true`, the attach
client reserves the top terminal row for a status banner and Ctrl-\ opens the
session menu. The menu owns kill confirmation (`k` then `y`), detach (`d`), new
session in the same worktree (`n`), fork (`f`), and rename (`r`). The client
composites the agent screen below the banner. Unlike the previous scroll-region
overlay, the client now parses the agent byte stream into its own screen model
and re-renders the terminal itself, so the banner works even under full-screen
TUI agents such as Codex and Claude Code: their cursor modes, scroll margins,
and repaint timing stay inside the parsed grid and never fight the banner.
Because the client composites the whole screen while the banner is on, the
terminal's native scrollback is unavailable during attach; mouse reporting still
works because physical coordinates are translated to the agent grid below the
banner. Leave `banner=false` (the default) to keep plain passthrough with native
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

## Work-item Links

`pohunek-launch-issue` and `pohunek-launch-pr` render the action's prompt with
`pohunek prompt render` (the same shared `crates/prompt` renderer the GUI
uses), then build the session-link metadata with a sibling client-side
subcommand, `pohunek prompt link --provider <linear_issue|github_pr>
--item-id <id> --url <url>`, reading the same provider JSON from stdin. It
derives `link.branch` from the provider JSON and prints the five canonical
`link.provider`/`link.kind`/`link.id`/`link.url`/`link.branch` lines. Neither
subcommand talks to the daemon.

`scripts/lib.sh`'s `pohunek_link_meta` helper wraps that call, and
`pohunek_run_session_new` forwards each line as a repeated `session new --meta
key=value` flag, so the link is written atomically in the same `session.new`
call that starts the agent — never as a separate post-launch step. Because
both surfaces build the metadata from the one shared implementation, a link
written by a launch script is byte-identical to one written by the GUI for the
same work item; see [GUI setup](gui.md) for the GUI side of this convention.

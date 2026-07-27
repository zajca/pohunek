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

Attach uses raw terminal passthrough by default, preserving the terminal's
native scrollback. Ctrl-\ temporarily freezes the visible agent screen and opens
a session menu together with a one-row status banner. The menu owns kill
confirmation (`k` then `y`), detach (`d`), new session in the same worktree
(`n`), fork (`f`), and rename (`r`). Agent output received while the menu is
open is buffered; closing the menu restores the frozen screen, replays that raw
output, and resumes passthrough without losing terminal modes or scroll margins.
The rofi/sway switcher only opens marked attach terminals; it does not create a
separate banner window. There are no banner settings in `launcher.conf`.

Attach terminals automatically retry after an unexpected daemon stream close.
`attach_reconnect_seconds` controls the retry window, and
`attach_reconnect_interval_seconds` controls the minimum delay between attempts.
`attach_reconnect_max_attempts` caps consecutive attempts within that window,
including failures where inspect still reports a running session. The
replacement daemon reconciles with the existing per-session worker, so Codex,
Claude, and plain shell sessions retain the same PTY, child PID, and runtime id.
A typed worker-stream failure is surfaced once and is not retried. A lost worker
cannot be reconstructed by retrying attach; inspect `runtime.state` and use
explicit native recovery only when supported. Set
`attach_reconnect_seconds=0` to disable retry behavior.

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

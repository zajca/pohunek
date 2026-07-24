---
type: Runbook
id: runbook/update-after-release
title: Update after release
description: Reconcile setup assets, host capabilities, projects, and launcher config after updating Pohunek.
source_kind: manual
intents: [update, setup, debug, help]
since: 0.3.3
---

# Update After Release

Use this runbook after replacing an installed Pohunek binary from a component
release archive or rebuilding it from source.

1. Download the component archive for the binary being updated: CLI (`pohunek`),
   daemon (`pohunekd` plus `pohunek-sessiond` and its systemd units), or GUI
   (`pohunek-gui`).
2. Run `pohunek doctor --json` to confirm the current binary can find required
   paths and state directories.
3. Run `pohunek health --json` to confirm the daemon responds with the expected
   version and protocol compatibility.
4. Run `pohunek host inspect local --json` to inspect local runtimes and
   capabilities.
5. Refresh launcher scripts with `pohunek setup scripts`.
6. Review config changes before applying `pohunek setup config --force`; default
   setup config should not overwrite existing files.
7. Reprint or refresh sway integration with `pohunek setup sway --print` or
   `pohunek setup sway`.
8. For important projects, verify `pohunek project show <id-or-label> --json`
   and resolved actions with `pohunek project actions <id-or-label> --json`.

For a daemon archive upgrade, run its installer rather than replacing only
`pohunekd`. The installer reloads unit definitions and restarts only
`pohunekd.service`; it does not restart existing
`pohunek-session@*.service` workers, so their mapped binary, PTY, and child PID
remain unchanged. After health returns:

1. Compare `systemctl --user show -p MainPID pohunek-session@<id>.service`
   before and after the daemon update for an important live session.
2. Inspect that session and confirm the same `worker_id` and `runtime_id`.
3. Treat `runtime.state=incompatible`, `conflict`, or `lost` as a diagnostic
   state. Do not restart or kill the worker merely to make the status disappear.

The first worker-aware release is a destructive compatibility boundary because
a legacy daemon cannot transfer an already-open PTY. Let all legacy sessions
finish before installing. The installer refuses visible live legacy sessions by
default; `--accept-runtime-loss` is informed consent to lose those existing
PTYs, not a recovery command. See
[debug session runtime](debug-session-runtime.md).

When the assistant feature is available, its update intent should use bundle
version metadata and `changed_in` frontmatter to explain version-specific
changes before recommending edits.

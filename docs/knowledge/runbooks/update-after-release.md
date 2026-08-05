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

## One-time public protocol v2 boundary

The v2 range-negotiation release cannot communicate with the former integer-v1
request envelope. Before replacing any component, inventory every CLI, GUI, web
backend/SDK, custom client, and local or NetBird-reachable daemon that must talk
to another peer. Drain cross-host automation, upgrade that complete set in one
maintenance window, and then verify every host with `pohunek health --json` and
`pohunek host inspect <host> --json`. The response must advertise protocol range
`2..=2` for this release.

There is no compatibility shim for the old request envelope or fixed
`codex`/`claude` notification-policy fields. Do not downgrade one peer to v1:
it will be isolated from v2 peers, and policy/state written by v2 is not a v1
rollback mechanism. Restore the coordinated v2 component set instead. Once the
boundary is crossed, later peers select the highest overlapping version, so M2,
M3, and additive provider work do not require another lockstep transition.

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

For the Hermes M2 runtime, inspect the `hermes` entry after upgrade. It is
launchable only with `version: "0.20.0"` and `supported: true`; a missing
binary has no version-policy result, while a wrong or unparseable version is
reported as unsupported. The model-free `cargo xtask hermes compatibility`
check needs the pinned executable and validates committed evidence. Do not run
`refresh-goldens` casually: it performs real provider turns in an isolated home
and can incur provider cost. It requires an operator-selected provider
environment-variable name, never reads the real Hermes home or `state.db`, and
must leave no pending golden records before release.

Do not downgrade a host from M2 to M1 after it has persisted a Hermes session.
M1 can preserve unknown provider values neutrally on the wire, but it cannot
operate the M2 Hermes runtime or safely rewrite its persisted launch identity.
Recover by upgrading forward to the matching M2-or-newer component set.

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
4. If concurrent reconciliation or lifecycle work returns
   `runtime/session_runtime_commit_stale`, refresh the session with
   `pohunek session inspect <target> --json`. The losing operation was not
   published; retry only from the runtime identity, decimal generation, and
   state now reported as authoritative. This code is not a post-rename
   durability warning: the daemon internally logs and applies a commit whose
   rename succeeded but parent-directory sync remained uncertain.

The first worker-aware release is a destructive compatibility boundary because
a legacy daemon cannot transfer an already-open PTY. Let all legacy sessions
finish before installing. The installer refuses visible live legacy sessions by
default; `--accept-runtime-loss` is informed consent to lose those existing
PTYs, not a recovery command. See
[debug session runtime](debug-session-runtime.md).

When the assistant feature is available, its update intent should use bundle
version metadata and `changed_in` frontmatter to explain version-specific
changes before recommending edits.

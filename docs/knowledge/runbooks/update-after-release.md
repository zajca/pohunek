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
reported as unsupported. The model-free compatibility check, `cargo xtask
hermes compatibility --pohunek-bin ABS`, needs the pinned Hermes executable on
`PATH` and an absolute, canonical built Pohunek executable at `ABS`; it validates
committed evidence. Refresh goldens with `cargo xtask hermes refresh-goldens
--hermes-bin ABS`, where `ABS` is the absolute path to the pinned Hermes
executable. The harness runs that real Hermes process in a real PTY but replaces
the model API with a repository-owned, deterministic IPv4-loopback mock.
It requires no provider credentials and incurs no provider cost.
Credential-source suppression normally produces no Copilot startup probe. If
the pinned background exchange still starts, the mock admits at most its
three-attempt budget of `CONNECT api.github.com:443` requests plus a
three-attempt `CONNECT api.githubcopilot.com:443` fallback budget. Fast process
shutdown may shorten
those probes or interleave them with scenario traffic. The mock validates the
two exact request lines and matching `Host` headers, returns HTTP 403 before TLS
begins, and therefore receives no authorization header or token. An over-budget
attempt, any other `CONNECT`, extra header, or absolute-form external request
fails closed. Each of the six model-bearing classic scenarios must then make
this exact localhost sequence: five ordered detection GETs to `/api/v1/models`,
`/api/tags`, `/v1/props`, `/props`, and `/version`, each receiving a
deterministic HTTP 404; then exactly one `POST /v1/chat/completions`. Discovery
is not cached across those processes. The isolated config statically pins
`pohunek-compat-v1`, `context_length: 64000`, and `discover_models: false`, so
Hermes does not request `/v1/models` and the mock does not permit that path.
The isolated home is preseeded with fresh `models_dev_cache.json` and
`cache/model_catalog.json` files, and remote model-catalog refreshes are
disabled. Its isolated `auth.json` suppresses every Copilot credential source,
including the `gh auth token` fallback. A repository-owned noncredential value
is selected before that subprocess; its pinned three-attempt token exchange is
the locally denied probe described above. An unreachable isolated D-Bus address
also prevents child processes from opening the operator's desktop keyring.
Harness-owned HTTP(S) proxy variables point at the loopback mock and
exempt only localhost. The exact denied Copilot probe is the only admitted
non-local proxy authority and never opens a tunnel. This is a fail-closed
application-level defense, not OS-level network containment. Exact response
evidence is the pinned streaming response frame's
ordered rounded header, exact content, and rounded footer render events across
prompt-toolkit redraws.
The `prompt-ready` and `exit` classic scenarios issue no model API requests.
The mock validates this application-level sequence, the POST model and last user
prompt, and the terminal tool where required. The refresh uses an isolated home without reading the
real Hermes home or `state.db`. Review every refreshed fixture and leave no
pending golden records before release.
The refresh also sets `HERMES_SKIP_NODE_BOOTSTRAP=1` and gives only the TUI
process an empty isolated `PATH`. A missing Node/npm runtime is recorded as the
recognized local `unsupported` state; the harness must never install TUI
dependencies or contact a package registry. Classic terminal-tool captures keep
the normal executable path for their exact repository-owned commands.

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

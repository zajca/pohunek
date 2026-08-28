---
type: Runbook
id: runbook/debug-daemon
title: Debug daemon availability
description: Diagnose cases where the CLI cannot reach the local or remote Pohunek daemon.
source_kind: manual
intents: [debug, setup, help]
since: 0.3.3
---

# Debug Daemon Availability

Use this runbook when commands report that the daemon is unreachable or unhealthy.

1. Run `pohunek doctor --json` for local environment checks.
2. Run `pohunek health --json` to query the local daemon.
3. If health cannot connect, start the daemon with
   `pohunek daemon start --detach`.
4. Run `pohunek health --json` again and inspect the reported socket, version,
   and status.
5. For remote hosts, run `pohunek host inspect <host> --json` and confirm the
   host daemon responds through the remote transport.
   If the remote daemon started before NetBird was ready, allow one retry
   interval for its NetBird-only listener to become available, then repeat the
   inspection and check its logs for `serving control protocol over NetBird`.
   A daemon restart is not required for this startup ordering.
6. If a session was expected, run `pohunek session list --json` on the relevant
   host and inspect the specific session with `pohunek session inspect <target>`.
7. If the daemon restarted, do not infer session exit from the closed control or
   attach socket. Check the session's `runtime.state`, `worker_id`, and
   `runtime_id`, then use the
   [session runtime runbook](debug-session-runtime.md) for `reconnecting`,
   `lost`, `conflict`, or `incompatible`.

For durable notification issues:

1. Run `pohunek notifications list --json` to inspect visible local records.
2. Run `pohunek notifications list --status deleted --json` when a record may
   have been logically deleted.
3. Run `pohunek notifications list --host <host> --json` to query a specific
   remote daemon, or `pohunek notifications list --all-hosts --json` to compare
   local plus reachable hosts.
4. Run `pohunek notifications policy get --json` and confirm the notification
   kind is enabled for the producer provider. Policy is enforced by the daemon
   for hooks, projectors, and any direct `notification.create` caller.
5. Run `pohunek notifications policy set --provider default --kind turn_completed --enabled --json`
   only when intentionally enabling noisy turn-completion records.
6. Run `pohunek notifications retention prune --dry-run --status archived --json`
   before applying retention cleanup. Use `--apply` only after reviewing the
   dry-run ids.
7. Run `pohunek notifications watch --json` in one terminal, then reproduce the
   event. A create should emit `notification_created`; read, acknowledge,
   archive, provider upgrade, or dedupe upgrade should emit
   `notification_updated`; delete should emit `notification_deleted`.
8. If a provider approval prompt does not appear, reinstall hooks with
   `pohunek integration install --agent <codex-or-claude>` and confirm the
   provider build supports the modern hook surface. Codex approval notifications
   require lifecycle `PermissionRequest` hooks; the legacy Codex `notify` key is
   not enough. Claude requires `Notification`, `Stop`, and `StopFailure` hooks.
   Reinstall preserves user hooks unless their command exactly matches a
   Pohunek-managed hook command.
   First inspect both agents with `pohunek integration status --json`, or select
   one with `pohunek --host <name> integration status --agent
   <codex-or-claude> --json`; daemon-backed status honors the effective host.
   For human output, remote recovery commands explicitly name the daemon host;
   run the local-only installer on that machine rather than adding `--host` to
   `integration install`.
   `current` verifies both executable assets with the installer-owned permission
   mode and effective-UID owner, exactly one managed registration under each
   expected provider event, and Codex feature/trust state. Asset type, UID, mode,
   and content come from one no-follow descriptor. The parent chain from the
   selected agent config root through the direct asset parent must contain only
   effective-UID-owned real directories with no group/world write access;
   ancestors above that explicit trust anchor are not inspected. Unsafe asset or
   parent ownership/permissions and duplicate managed registrations are
   `outdated`. A missing Claude `hooks/` child under a trusted config root is a
   reinstallable absence; installation creates it as owner-private mode `0700`
   and leaves an existing real user directory's mode unchanged. Claude
   `settings.json`, Codex `hooks.json`, and Codex `config.toml` are each opened
   no-follow and require effective-UID ownership plus no group/world write bits;
   their metadata and bounded content come from the same descriptor. Codex
   managed trust uses a canonical single-handler group, so a sibling handler is
   drift even when the managed command itself is unchanged. Follow the typed
   recovery action: `reinstall` means the installer
   can repair every finding, while `repair_configuration` means provider files,
   symlinked, special, foreign-owned, or group/world-writable managed assets or
   parents, invalid registration roots, incompatible TOML table shapes, and
   installer-owned scalar trust entries must be inspected and fixed first. A
   missing `hooks` object and an exact owned command with drifted handler metadata
   remain safely reinstallable. Oversized and non-regular files are rejected by
   bounded, nonblocking inspection and reported with the applicable recovery.
   Status itself never repairs or rewrites provider configuration.
9. If a notification is missing its session link, inspect the hook environment
   setup. Hook adapters silently drop an invalid `POHUNEK_SESSION_ID` and still
   create the notification without linkage.
10. If duplicate attention notifications appear, compare each record's
    `dedupe_key`, `source.provider`, `source.provider_event`, and `created_at`.
    Session attention dedupe uses `attention:<session_id>` and only applies
    inside the policy's `attention_dedupe_window_secs`.
11. If stale `agent_blocked`, `approval_required`, or `turn_completed` records
    stay `unread` after the agent already resumed, check that the daemon still
    observes the session returning to `working` activity. Session notifications
    auto-resolve to `acknowledged` when the projector sees the transition into
    `working`, keyed by `attention:<session_id>` and `turn:<session_id>`, so
    hook- and projector-produced records are cleared together. A record that
    never self-resolves usually means no `working` activity edge reached the
    projector (for example a session whose activity is not being reported).
12. If repeated `turn_completed` rows appear for one session, inspect their
    `dedupe_key`. Modern hooks send `turn:<session_id>` for `Stop`; a newer
    unread turn supersedes older unread turns for the same key, and a visible
    attention record supersedes the unread turn twin. Missing `turn:` keys mean
    the host likely needs `pohunek integration install` so managed hooks refresh.
13. If an expected `agent_blocked`, `approval_required`, or `turn_completed`
    notification does not (yet) show up in `pohunek notifications list --json`
    or `pohunek notifications watch --json`, it may simply be debounced: the
    daemon holds session creates pending in memory for `attention_debounce_secs`
    (5 seconds by default, see `pohunek notifications policy get --json`) before
    committing and emitting `notification_created`. Wait past the configured
    window and re-check; if the session resolved back to `working` inside the
    window, the pending record was dropped and will never appear — that is the
    intended debounce behavior, not a bug. Pending debounced entries are
    in-memory only and are not persisted, so a daemon restart while an entry is
    pending drops that transient signal; this is expected for a sub-10s window
    and is not a data loss bug worth chasing.

The durable store is under the daemon data directory in `notifications/`.
`notifications.jsonl` is append-only record history, while `policy.json` is the
current persisted notification policy. Both are host-local; `--all-hosts`
commands perform client-side fan-out rather than reading a shared store.

Do not treat setup assets as the daemon source of truth. Launcher scripts may be
stale or missing while the daemon itself is healthy; verify daemon health first,
then move to [debug launcher](debug-launcher.md).

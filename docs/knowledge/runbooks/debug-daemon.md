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
6. If a session was expected, run `pohunek session list --json` on the relevant
   host and inspect the specific session with `pohunek session inspect <target>`.

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
9. If a notification is missing its session link, inspect the hook environment
   setup. Hook adapters silently drop an invalid `POHUNEK_SESSION_ID` and still
   create the notification without linkage.
10. If duplicate attention notifications appear, compare each record's
    `dedupe_key`, `source.provider`, `source.provider_event`, and `created_at`.
    Session attention dedupe uses `attention:<session_id>` and only applies
    inside the policy's `attention_dedupe_window_secs`.

The durable store is under the daemon data directory in `notifications/`.
`notifications.jsonl` is append-only record history, while `policy.json` is the
current persisted notification policy. Both are host-local; `--all-hosts`
commands perform client-side fan-out rather than reading a shared store.

Do not treat setup assets as the daemon source of truth. Launcher scripts may be
stale or missing while the daemon itself is healthy; verify daemon health first,
then move to [debug launcher](debug-launcher.md).

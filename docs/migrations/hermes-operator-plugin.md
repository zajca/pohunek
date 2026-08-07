# Hermes Operator Plugin Migration and Rollback

This migration applies after the fleet has crossed the one-time M1 public
protocol transition. M3 does not change public protocol version `2`; it adds a
local, profile-owned Hermes plugin lifecycle to the CLI.

## Upgrade order

1. If any local or NetBird peer still uses the pre-M1 exact-version protocol,
   drain cross-host automation and complete the coordinated M1 migration first.
   There is intentionally no legacy envelope or notification-policy shim.
2. On each host, install the matching `pohunek-sessiond`, `pohunekd`, and local
   clients. Restart/reconcile the daemon and leave live workers alone; the
   daemon handles its current/previous private worker compatibility window.
3. Upgrade remaining clients and verify each relevant host pair negotiates a
   public protocol range. M2 and M3 need no new fleet-wide boundary once this is
   true.
4. Select one Hermes profile or custom absolute home and install the plugin with
   explicit `--access-mode` and `--allow-host` values. Start with `manage` and
   `local` for a canary.
5. Run `pohunek integration doctor --agent hermes` for the same explicit target,
   then validate a managed Hermes session's native identity, screen, output,
   wait, and native resume. Hermes native fork remains unsupported.
6. Expand to other profiles/hosts only after the canary succeeds. Remote
   execution remains direct over NetBird, never SSH; `full` and `*` remain
   explicit opt-ins.

## Rollback boundaries

Plugin disablement or uninstallation is safe and does not remove logical
sessions, Hermes `state.db`, user configuration, unrelated plugins, or existing
non-Hermes workers. Use the Hermes lifecycle command with the exact target;
never manually delete a profile tree.

```bash
pohunek integration uninstall --agent hermes --hermes-profile work --json
```

If checksum inspection reports changed managed files, keep the evidence and use
`--confirm-modified` only after confirming that removal is intended. Reinstall
or update only through the CLI so the external owner-private policy and embedded
absolute policy path remain coherent.

Binary downgrade is not a rollback strategy after a host has persisted Hermes
agent enum values or provider-keyed notification policy. Older binaries do not
provide an old-shape compatibility shim. Restore service by upgrading forward to
a compatible release, then use the plugin lifecycle commands to change only the
selected profile. A live worker is never restarted merely to force a downgrade.

## Data and security boundaries

The migration never reads or migrates Hermes `state.db`, `.env` files,
credentials, keys, certificates, prompts, tool payloads, or terminal output.
The policy is a delegated-tool guardrail, not a same-user sandbox. Its external
owner-private location is intentional: policy tampering is a doctor finding,
not an asset-checksum bypass or a reason to restore an old binary.

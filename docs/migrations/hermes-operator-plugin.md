# Hermes Operator Plugin Migration and Rollback

This is a historical M1-to-M3 migration record. At that time, M3 did not change
the then-current public protocol v2; it added a local, profile-owned Hermes
plugin lifecycle to the CLI. Current releases use protocol v3 and follow the
[current update runbook](../knowledge/runbooks/update-after-release.md).

## Upgrade order

1. If any local or NetBird peer still uses the pre-M1 exact-version protocol,
   drain cross-host automation and complete the coordinated M1 migration first.
   There is intentionally no legacy envelope or notification-policy shim.
2. On each host, install the matching `pohunek-sessiond`, `pohunekd`, and local
   clients. Restart/reconcile the daemon and leave live workers alone; the
   daemon handles its current/previous private worker compatibility window.
3. Historically, upgrade remaining clients and verify each relevant host pair
   negotiated a public protocol range. M2 and M3 required no additional
   fleet-wide boundary after the M1 transition.
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

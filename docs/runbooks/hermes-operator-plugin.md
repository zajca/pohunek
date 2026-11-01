# Hermes Operator Plugin Rollout and Recovery

Use this runbook to install or update the Pohunek operator plugin for one
selected Hermes Agent `0.20.0` profile. It is a single-operator integration:
the selected profile, the local Pohunek state directory, and the NetBird mesh
remain under the operator's control.

## Preconditions

- Use a matching Pohunek CLI, daemon, and `pohunek-sessiond` release on the
  host. Install the daemon archive as a set so the worker remains beside the
  daemon.
- Install the official Hermes runtime package before the plugin. The daemon
  serves `hermes` sessions only from this package, and the plugin's
  operator payload comes only from the installed, enabled and selected
  `pohunek.runtime.hermes` package; without it `integration install` and
  `update` fail with `hermes_package_not_installed` and change nothing. From an
  extracted Linux daemon archive:

  ```bash
  pohunek plugin install runtime/packages/hermes.tar.zst \
    --catalog runtime/runtime-catalog.json
  ```
- The Hermes executable must be the pinned supported release. Do not allow an
  installer or smoke test to download Hermes, Node packages, plugins, or model
  providers.
- Select one target explicitly. Use `--hermes-profile default`, a named
  `--hermes-profile`, or one absolute `--hermes-home`; never combine profile and
  home and never point at a broad or shared directory.
- Do not inspect, copy, repair, or delete Hermes `state.db`, credentials,
  `.env` files, keys, or certificates. The plugin does not need them.

## Historical M3 rollout context

M1's exact-version-to-range-negotiation transition historically established
protocol v2. M2 and M3 were ordinary rollouts that did not bump that then-current
public protocol. Current releases use protocol v4 and require the coordinated
v4 upgrade described in the [update runbook](../knowledge/runbooks/update-after-release.md).
The [durable-worker migration](../migrations/durable-session-workers.md) covers
the separate legacy daemon-owned-PTY boundary only.

For each host after that one-time transition:

1. Inventory local and NetBird-reachable daemons, CLI and web clients, and
   Hermes profiles that will operate sessions.
2. Install the matching daemon archive so `pohunekd` and
   `pohunek-sessiond` are updated together. Restart/reconcile the daemon and
   verify existing workers before touching a Hermes profile.
3. Upgrade remaining ordinary clients, then use `pohunek health --json` and
   `pohunek host inspect <host> --json` to confirm negotiated protocol ranges.
4. Install the plugin into one canary profile with a least-privilege policy:

   ```bash
   pohunek integration install --agent hermes --hermes-profile canary \
     --access-mode manage --allow-host local --json
   pohunek integration doctor --agent hermes --hermes-profile canary --json
   ```

5. Confirm the official Hermes package is installed (`pohunek plugin list`
   shows `pohunek.runtime.hermes` enabled and selected; see Preconditions),
   then launch one managed Hermes canary. Verify its reported native identity,
   continuation identity replacement after `pre_llm_call`, rendered screen,
   incremental output, bounded wait, and native resume. Hermes fork must return
   typed unsupported data rather than creating a child.
6. Add a named remote host only after verifying direct NetBird reachability.
   Use `full` only where stop/remove delegation is intended. A wildcard
   allowlist requires the explicit installer confirmation; it never triggers
   host discovery or scanning.
7. Repeat the install and doctor for each intentionally selected profile. The
   installer never modifies every Hermes profile automatically.

## Policy and operational safety

The policy is owner-private Pohunek state outside the immutable plugin checksum
set. It records the fixed CLI path, protocol range, access mode, and exact host
allowlist; a rendered plugin asset contains its absolute selected policy path.
It is not a sandbox against a same-user Hermes process that can use a shell or
write files. Treat terminal and repository content as untrusted, keep remote and
destructive delegation explicit, and rely on the daemon for normal session and
worktree preconditions.

The plugin rejects the origin session for exactly `session.stop`,
`session.resume`, `session.remove`, `session.fork`, `session.resize`,
`session.set_metadata`, `session.rename`, and `session.input` before it launches
the CLI. The daemon repeats that authoritative denial and also denies `session.remove_accepting_unconfirmed`, which the plugin never offers. Only
`session.report_agent`, `session.release_agent`, and
`session.report_native_id` may target the origin, and only for lifecycle
reporting.

Hooks use a short local socket deadline that bounds the agent's turn, not the
outcome of a report. Identity goes to the session worker only, with no public
native-ID fallback. A report whose request was fully written to the worker is
applied whether or not its reply arrives in time, so a deadline that passes
after that point is recorded as `unanswered` and is not counted as a failure.
A worker that closes the connection without a reply is a rejected report and
is counted, like a refusal or a failure to connect or write. Hooks never start
a subprocess, use a network, or access a Hermes database. Counted failures are
swallowed; process/screen detection remains available. Do not treat
`on_session_end` as a process exit.

## Doctor-led recovery

Always diagnose the same explicit target before repairing it:

```bash
pohunek integration status --agent hermes --hermes-profile canary --json
pohunek integration doctor --agent hermes --hermes-profile canary --json
```

Doctor returns fixed, payload-free checks for the pinned executable/version,
target safety, managed ownership and checksums, enablement, policy permissions
and schema, CLI compatibility, tool/skill/hook registration, host syntax,
access mode, and stale stage/backup state. Follow its recovery hint; do not
edit plugin files, Hermes YAML, or the policy by hand.

| Finding | Safe recovery |
| --- | --- |
| Version, CLI compatibility, tool, skill, or hook registration | Install matching binaries, then run `integration update` for the same target. |
| Ownership, checksum, policy permission, stale stage, or stale backup | Preserve the finding, review the selected target, then run `integration update`; use `--confirm-modified` only after verifying intentional changes. |
| Plugin not enabled | Run `integration update`; it uses the supported Hermes plugin command rather than editing general YAML. |
| `asset_source` fails with `hermes_package_not_installed` | Install the official Hermes package from its signed catalog, then run `integration update`. An install whose `provenance` is `embedded` came from an older CLI's compiled copy; it is never migrated automatically, `integration uninstall` removes it without the package, and `integration update` after the package install rewrites it with `source=package`. |
| Host or access-mode denial | Change the explicit policy through `integration update`, not via a tool or a file edit. |
| Output gap or runtime change | Discard the stale cursor; read a fresh screen/newest tail and continue only with returned runtime coordinates. |
| Wait timeout | Treat it as bounded no change. Reissue a short wait or let a human attach; do not claim a terminal outcome. |

## Update, removal, and incident containment

Update is atomic and preserves unrelated profile state:

```bash
pohunek integration update --agent hermes --hermes-profile canary --json
pohunek integration update --agent hermes --hermes-profile canary \
  --confirm-modified --json
```

If delegation must stop immediately, disable or remove the managed plugin
through its lifecycle command, not by deleting a profile directory:

```bash
pohunek integration uninstall --agent hermes --hermes-profile canary --json
pohunek integration uninstall --agent hermes --hermes-profile canary \
  --confirm-modified --json
```

Uninstall validates the ownership marker and checksums, disables through Hermes,
removes only manifest-listed assets and the matching external policy, and leaves
sessions, `state.db`, user configuration, and unrelated plugins untouched. A
modified managed file requires the explicit confirmation. Existing non-Hermes
workers and non-plugin session observation continue normally.

For a release-artifact smoke, run the script bundled under
`packaging/smoke-hermes-plugin-release` after extracting a daemon archive.
Supply the extracted archive directory, the Hermes runtime package archive, a
signed catalog that authorizes it, the trust anchor of that catalog's signer,
and an absolute, preinstalled pinned Hermes executable:

```bash
packaging/smoke-hermes-plugin-release /abs/pohunek-daemon-X.Y.Z-TARGET \
  /abs/hermes.tar.zst /abs/runtime-catalog.json \
  /abs/runtime-catalog-anchor.json /abs/pinned/hermes
```

A Linux or Apple Silicon release daemon archive carries all three inputs:
`runtime/packages/hermes.tar.zst`, `runtime/runtime-catalog.json`, and the
`runtime-catalog-anchor.json` that trusts the catalog's signer. CI and the
release workflows run this plugin smoke only on archives without the signed
release catalog, so that combination is not exercised there. For a development
archive, build the package from a checkout with `scripts/build-hermes-package`
and write a catalog and anchor for it with `cargo xtask hermes throwaway-catalog
--package <archive> --output-dir <dir>`.
The script copies the binaries into a private prefix with the anchor beside the
daemon, starts that daemon with its own temporary home/profile/state, installs
the package with `pohunek plugin install --catalog`, and fails hard if an
executable is missing, the daemon does not start or stop cleanly,
install/status/doctor/uninstall fails, or an installed-state result does not
report the package and its digest as the plugin's provenance. It never
downloads runtime dependencies or contacts a model/provider. The release
workflow performs this check for every daemon target before publication; repeat
it manually when validating a downloaded archive or local canary.

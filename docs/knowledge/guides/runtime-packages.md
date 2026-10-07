---
type: Guide
id: guide/runtime-packages
title: Runtime packages
description: Install, inspect, update, select, disable, and remove runtime packages with pohunek plugin, including the trust model, the consent step, and what disable and uninstall do to sessions.
source_kind: manual
intents: [setup, update, debug, help]
---

# Runtime packages

A runtime package adds an agent runtime (a program the daemon launches inside a
session) to the host. `pohunek plugin` manages the packages installed on the
machine the daemon runs on. See the
[runtime package archive](../concepts/runtime-package-archive.md) for the file
format and the [runtime catalog](../concepts/runtime-catalog.md) for official
trust. The repository's own package, [Pi](pi-package.md), is installed this way.
The [Codex package](codex-package.md) is the second official package source; it
serves the reserved `codex` id only through a signed catalog, and no release
catalog or signing key exists yet.
The [Claude Code package](claude-package.md) is the third; it serves the
reserved `claude` id the same way, and the built-in Claude runtime stays in the
daemon until a production catalog key exists.

```bash
pohunek plugin list
pohunek plugin install ./acme-pi-1.0.0.tar.zst --sha256 sha256:<64 hex>
pohunek plugin install ./acme-pi-1.0.0.tar.zst --sha256 sha256:<64 hex> --yes
pohunek plugin inspect acme.pi
pohunek plugin update acme.pi ./acme-pi-1.1.0.tar.zst --sha256 sha256:<64 hex> --yes
pohunek plugin select acme.pi --version 1.0.0
pohunek plugin disable acme.pi --version 1.1.0
pohunek plugin uninstall acme.pi --version 1.1.0 --yes
pohunek plugin doctor
```

Every subcommand accepts `--json`: one envelope on stdout, digests in full.
Tables abbreviate a digest to its first 12 hex characters; that prefix is a
valid `--digest` selector.

## Config home

A runtime that keeps its settings, hook registration or conversations in a
directory of its own declares it in an optional `[config_home]` table, separate
from `[integration]` because the home concerns every runtime:

```toml
[config_home]
env = "ACME_HOME"          # variable the agent reads to relocate its home
default = ".config/acme"   # directory below the user's home used otherwise
```

`env` is an upper-case variable name outside the reserved `POHUNEK_` namespace;
`default` is a relative path of plain components (no `..`, no absolute path, no
glob or shell character, bounded in length and depth). Both keys are required
and unknown keys are refused (`descriptor_invalid`) before anything is recorded.
The daemon resolves the home the way a launch does (the variable from the
environment the launched agent sees, a host profile's `[env]` overriding the
daemon's base environment, else the default below that environment's `HOME`) and
never expands a `~`. A runtime that names a daemon-run integration handler
needs the table, or `integration install` answers `agent_config_home_undeclared`
for it. A runtime without an integration (Pi) may omit it; Pi keeps declaring
its conversation root under `[native_reference.existence]`.

## Hook schemas and integration handlers

A package whose agent reports lifecycle hooks names its compiled integration
handler and the hook schema its reports follow in the descriptor:

```toml
[integration]
handler = "codex-hook-v1"
hook_schema = "identity-subagent-v1"
```

Core owns the schemas: `identity-v1` (identity, release, notification) and
`identity-subagent-v1` (the same plus subagent start and stop). A package only
names one; it cannot carry schema contents, add actions, or relax validation.
`plugin install` and `plugin install --yes` refuse a descriptor whose schema id
or handler id core does not provide, or whose handler does not drive the schema
(`descriptor_invalid`), before anything is recorded. A package without
`[integration]` (Pi) has no schema, and its sessions accept no hook report.
A package whose reference is `assigned` and that also declares an integration can
report its conversation: a validated report replaces the assigned reference and
labels it `reported` (a `/clear` or in-session resume is followed), while a
package without an integration keeps its `assigned` reference.

The handler is compiled core code selected by the descriptor's
`integration.handler`: `pohunek integration install`, `status`, `doctor`, and
`uninstall` resolve the runtime definition and run the handler it names, never a
branch on the runtime id. A handler owns exactly one active asset set, the paths
and modes it may write, conflict detection, rollback, and the commands it may
run; a package cannot ship shell installers, JSON patch programs, or filesystem
targets. Re-installing is an update transaction: the handler stages the new
assets against the active ones without changing them, checks that the hook
schema admits everything the new set reports, derived from the staged
registration and the embedded scripts' action tables rather than from a
declaration (`integration_update_incompatible` otherwise), activates atomically, and
restores the exact prior tree if any step fails, so the old set stays active
until activation succeeds. The check covers the hook schema of every package
version that a live, lost or resumable session or a host profile pin still
references for that handler, not only the schema of the selected version, and an
update of any handler is refused while such a version cannot be read.
The reporter scripts a handler installs are core-owned templates: the handler
renders them from the runtime's id and display name before staging, a value
outside a conservative character set is refused with a typed error, and the
rendered bytes are what is installed and what drift is compared against. A
package never supplies script bytes.
`hermes-hook-v1` is registered but its lifecycle runs
in the CLI; the daemon answers `agent_not_installable` for it.
`pohunek integration --agent` takes the runtime id of a package runtime that
names a daemon-run handler, and the recovery commands in status and doctor name
that id.

## Local only

Package commands act on the daemon on this machine through its local control
socket. A remote `--host` is rejected before anything runs (code
`plugin_local_only`), and the daemon refuses package methods on remote overlay
connections too. Installing a package extends what the owner's account may
launch on that machine, so it is never a remote operation.

## Trust model

- `--sha256 <digest>` authorizes a third-party archive by the digest the owner
  supplies (`sha256:<64 hex>` or the bare hex `sha256sum` prints). The archive
  carries no signature; the owner vouches for these exact bytes. This trust can
  never authorize an official runtime id such as `codex`, `claude`, or `hermes`.
- `--catalog <file>` authorizes an official package through the signed catalog,
  verified against the trust anchor file shipped beside the daemon (see the
  [runtime catalog](../concepts/runtime-catalog.md#trust-anchor-file)). A host
  without an anchor refuses it (`official_trust_unavailable`); a host whose
  anchor exists but cannot be trusted refuses it with
  `official_trust_anchor_invalid`, and `pohunek doctor` reports the
  `catalog_trust_anchor` check as `warn` or `fail` respectively. Only a package
  whose catalog entry binds its package id, runtime id and digest may serve
  `codex`, `claude` or `hermes`; a catalog that authorizes a different id,
  package or digest for the archive is refused (`package_untrusted`). Installs
  trusted by `--sha256` are unaffected.
- `link <dir>` copies a developer directory. The copy is installed disabled and
  unselected, and is recorded with the `link` origin.

## Consent

`install`, `link`, `update`, and `uninstall` change what the host may launch, so
they need `--yes`.

- Without `--yes`, the CLI asks the daemon to validate the package in a dry run
  (nothing is installed), prints what the package declares (package id,
  version, digest, trust origin, runtime id, display name, the program and fixed
  arguments the daemon will launch as the owner, the arguments it appends at
  launch to pass the session reference, the resume and fork argument templates
  (`{reference}` marks the reference slot), whether the first prompt is
  appended to the arguments, the version probe and the integration handler; the
  hook schema is part of the same descriptor), and fails with code `consent_required`. Nothing changed.
- With `--yes`, the CLI repeats the dry run and then performs the change.
- Under `--json` without `--yes` only the error envelope is printed; its
  message names the package, so omit `--json` to see the full review.

## Declaring a version probe

A descriptor names the supported release range with `version_probe` in
`[runtime]` (full reference in `docs/public-api.md`). Pick the parser by what
`<program> <args>` prints on its first line:

- `semver-v1` when it prints exactly `MAJOR.MINOR.PATCH` (Pi).
- `semver-line-v1` with a `line` template when the release sits inside fixed
  text: `line = "codex-cli {version}"` for `codex-cli 0.160.0`,
  `line = "{version} (Claude Code)"` for `2.1.289 (Claude Code)`,
  `line = "Hermes Agent v{version} {annotation}"` for
  `Hermes Agent v0.20.0 (2026.8.3)`.

The template is literal text, not a regular expression, and an invalid or
over-broad one (no literal next to `{version}`, an unknown placeholder, a
repeated `{version}`) fails package validation. A pre-release banner such as
`codex-cli 0.161.0-rc.1` never matches, so the runtime reports unsupported.
Verify the real banner of the runtime before declaring the template.

## Selecting a package version

`<package>` is a package id. Several versions of one id can be installed side by
side; narrow with `--digest <full digest or hex prefix of at least 12
characters>` and `--version <v>`.

- `inspect` with no narrowing shows the selected version, or errors listing the
  candidates when none is selected.
- `select`, `enable`, `disable`, and `uninstall` never guess: an ambiguous
  selection is the error `plugin_selector_ambiguous`, listing the candidates.
- A new version installed with `install` becomes selected only when it is the
  first version of its id. `update` installs enabled and selected and keeps the
  old version installed; `select` rolls back or forward. Profiles keep the
  digest they pinned, so an update never changes a pinned profile.
- `update` requires the id to be installed and the archive to declare the same
  package id.

### Incompatible updates stay installed but unselected

Sessions launched from an older version keep reporting through that version's
integration handler and hook schema until they end, so a version whose
integration differs cannot become the selected one while something still
references the old version. The integration of a version is the pair of its
`[integration]` handler id and hook schema id, or none; versions of one package
are compatible only when the pairs are equal (adding or dropping an integration
is a difference too), and a retained version whose descriptor or root cannot be
read counts as incompatible.

- A version is *held back* while a live, lost or resumable session or a host
  profile pin (the references that block `uninstall`) uses another version of
  the same package with a different integration. `plugin select` then fails with
  `package_integration_incompatible` and changes nothing. `plugin install` and
  `plugin update` still install the archive (enabled), report it as not
  selected, and name the digest it waits on; `update` exits with the select
  error because it promises a selected version. `plugin list` shows
  `selection blocked`, `plugin inspect` shows the `Selection:` line and the
  hook schema, and `plugin doctor` reports a `selection blocked by <digest>`
  finding.
- The selected version keeps serving fresh launches and nothing falls back to
  another package. A pinned profile or session still resolves from exactly its
  digest.
- The state is derived at each call from the registry and the sessions, never
  stored, so a daemon restart recomputes it. It clears when the last reference
  to the old version is gone (stop and remove the sessions, unpin or rebind the
  profiles, or uninstall the old version); the package is then selectable, but
  nothing selects it for you: run `plugin select` (or `plugin update` again).
- `package.bind_profile` (the wire method behind `plugin profile migrate`)
  refuses to pin a version whose integration differs from the selected version
  of its package or from another referenced version (the pin being replaced does not count), with the same error, because the pin would be a retained
  reference next to an incompatible selected version.
- A launch that resolved the previously selected version just before a select
  committed is refused with `runtime_package_changed`; retry it.

## Official aliases

An official package serves `codex`, `claude` or `hermes` in place of the
built-in runtime as soon as it is installed, enabled and selected;
`plugin disable` or `plugin uninstall` returns the alias to the built-in.
`plugin list` shows the serving package: the selected, enabled `official`
package whose runtime is the alias; without one the built-in serves it.
A session launched from the built-in runtime keeps its binding but cannot be
resumed or forked while the package serves the alias: the request fails with
`runtime_served_by_package`. Disable or uninstall the official package to resume
it from the built-in, or start a new session on the package. A host profile on
that base can be pinned with `plugin profile migrate` once the package serves
the base (`package_profile_base_builtin` before). A migrated schema-1 binding
stays unpinned, so it is refused the same way.

## Disable

`disable` blocks fresh launches of the package, including launches through a
profile that pins the digest and relay-approved profiles. It does not stop live
sessions, and an existing session pinned to the package can still be resumed.
`enable` allows fresh launches again.

## Uninstall

The daemon refuses to uninstall a digest a live, lost, or resumable session or a
host profile pins (`package_referenced`). A package whose root fails
verification is removed only with `--remove-modified`; a root that verifies is
refused on that path (`package_root_intact`), so uninstall it without the flag.

## Doctor

`doctor [<package>]` verifies every installed package root and reports faults,
package roots on disk without a registry record, and pins of digests that are
not installed. It exits with status 1 when any finding exists and 0 otherwise.

## Profile migration

A host agent profile (`<config dir>/agents/<name>.toml`) keeps `base =
"<runtime id>"`. When an installed package serves that runtime, the profile
must also carry an explicit pin: `package = "<package id>"` and `digest =
"sha256:<64 hex>"`. A profile whose base is served by no installed package
carries neither. A profile moves to another digest only through
`pohunek plugin profile migrate`, never through `plugin update` or `plugin
select`.

`pohunek plugin profile list [--json]` shows every `agents/*.toml` with its
base, pinned package, and digest (abbreviated in the table, whole in JSON) and
one state:

- `builtin`: no installed package serves the base; nothing to do.
- `pinned`: the pinned digest is installed and serves the base.
- `needs_migration`: an installed package serves the base but the pin is
  missing, incomplete, or names a package that does not serve the base.
- `pin_not_installed`: the pinned digest is not installed.
- `unreadable`: the file is a symlink or special file, is not owned by you, is
  group- or world-writable, is not valid TOML, or has an invalid `base`,
  `package`, or `digest`.

`pohunek plugin profile migrate <name> [--digest <d>] [--yes] [--json]` pins
the profile through the daemon's `package.bind_profile` method; the command
never writes the file. The daemon applies the pin under the same exclusive
package lifecycle authority as install, select and uninstall, so a concurrent
uninstall of the target either runs first (the bind then fails with
`package_profile_target_invalid`) or is refused with `package_referenced`.
The target is the installed package with the given digest (the full
`sha256:<64 hex>` or a unique prefix of at least 12 hex characters), which must
serve the profile's base and load without a fault. Without `--digest` it is the
one selected, enabled package serving the base; none or several is an error,
never a guess. The command first asks the daemon for a dry run, prints the
profile, base, package version, digest, and current pin, and changes nothing
until repeated with `--yes` (`consent_required` otherwise); the consented call
names the digest the preview showed. A base served by a built-in is refused
(`package_profile_base_builtin`), as are a missing profile
(`package_profile_not_found`), one the daemon cannot accept as a profile file
(`package_profile_unusable`: symlink, hard link, wrong owner, group- or
world-writable, too large, not valid TOML) and one edited while the bind ran
(`package_profile_changed`). A profile that already pins the target is reported
as `unchanged` with exit status 0 and is not written.

Only the `package` and `digest` keys are rewritten: existing ones are updated in
place, missing ones are inserted directly after `base`, and every other byte of
the file (comments, `[env]` values, ordering) is kept. The daemon reads the
profile with the same loader that serves launches and the retention scan, writes
the new text to a temporary file in the agents directory (mode narrowed to at
most `0600`), and renames it over `<name>.toml`: the name resolves to either the
old or the new complete file at every instant, with no interval in which the
profile is missing. Immediately before the rename the file is compared with what
was read and a changed profile is left as found. A temporary left by an
interrupted run never ends in `.toml`, is never loaded, and is removed by the
next bind. Profiles can hold secret `[env]` values, so nothing prints file
content or values; a parse failure is reported as a line number and message
only.

The commands work on this machine only (`--host` is rejected) and need the
agents directory to be owned by you and not group- or world-writable. A
migration changes the profile's revision on the daemon side, so a relay approval
of the previous revision goes stale.

## Shell completion

Dynamic completion (`pohunek completions <shell> --dynamic`) completes package
ids and `--digest` values from the local daemon within a short deadline, and
`plugin profile migrate` completes profile names from the agents directory
(names only, no daemon). Completion never starts the daemon and stays silent
when the daemon is unreachable. Archive and catalog arguments complete as files,
`link` as a directory.

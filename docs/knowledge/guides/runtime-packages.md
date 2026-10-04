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
trust.

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
- `--catalog <file>` authorizes an official package through the signed catalog.
  A host without a catalog trust anchor refuses it (`official_trust_unavailable`).
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
  appended to the arguments, the version probe and the integration handler), and fails with code `consent_required`. Nothing changed.
- With `--yes`, the CLI repeats the dry run and then performs the change.
- Under `--json` without `--yes` only the error envelope is printed; its
  message names the package, so omit `--json` to see the full review.

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
the profile. The target is the installed package with the given digest (the full
`sha256:<64 hex>` or a unique prefix of at least 12 hex characters), which must
serve the profile's base. Without `--digest` it is the one selected, enabled
package serving the base; none or several is an error, never a guess, and a
faulted package is refused. The command prints the profile, base, package
version, digest, and current pin and changes nothing until repeated with
`--yes` (`consent_required` otherwise). It refuses a profile whose base no
installed package serves (`profile_base_builtin`) and reports `unchanged` with
exit status 0 when the profile already pins the target.

Only the `package` and `digest` keys are rewritten: existing ones are updated in
place, missing ones are inserted directly after `base`, and every other byte of
the file (comments, `[env]` values, ordering) is kept. The write is atomic: the
new file is created exclusively in the same directory, synchronized, and renamed
over the original, which is displaced aside first so a profile edited since it
was read is detected instead of overwritten, and is removed only after the
rewritten file re-reads and verifies. The new file keeps the original mode
narrowed to at most `0600`. Profiles can hold secret `[env]` values, so no
command prints file content or values; a parse failure is reported as a line
number and message only.

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

---
type: Runbook
id: runbook/runtime-package-recovery
title: Recover a runtime package
description: Diagnose and recover runtime packages that fail verification, official installs refused by the catalog trust anchor or signer, and profiles or sessions whose pinned package was disabled, removed, or damaged, starting from pohunek plugin doctor.
source_kind: manual
intents: [debug, update, help]
since: 0.34.2
---

# Recover a Runtime Package

Use this runbook when a package command (`pohunek plugin <command>`), a session
launch, or a resume fails because of a runtime package. Every package command
acts on the daemon of the machine you run it on and refuses a remote `--host`
(`plugin_local_only`), so run these steps on the daemon's host.

Error codes are stable and their messages carry no path or package content: the
`package.*` codes are defined in `crates/protocol/src/package.rs`, the launch
codes `runtime_not_installed` and `runtime_incompatible` in
`crates/protocol/src/error.rs`, and the CLI-only codes in
`crates/cli/src/commands/plugin.rs`. The precise cause is only in the daemon's
JSON log under `~/.local/state/pohunek/logs/` (see the
[debug daemon](debug-daemon.md) runbook). The commands are explained in the
[runtime packages guide](../guides/runtime-packages.md).

## 1. Start with plugin doctor

1. Run `pohunek plugin doctor`. It verifies every installed package root
   against its manifest and exits with status 1 when any finding exists.
   `pohunek plugin doctor <package> --json` restricts the report to one package
   id and prints every digest in full. The `PINNED` column says whether a
   session or host profile pins the digest; a pinned digest cannot be
   uninstalled (`package_referenced`).
2. Match each finding (implemented in
   `crates/daemon/src/session/package_lifecycle/` and
   `crates/package/src/registry.rs`):
   - `root missing`: the package files are gone but the record remains.
     Reinstall the same archive with the same trust (`--sha256 <digest>` or
     `--catalog <file>`); the daemon extracts it again and reports
     `root_restored`. A `--catalog` reinstall restores the root only while the
     daemon still runs the release whose catalog authorized the package and no
     newer catalog has been accepted; otherwise it is refused with
     `package_incompatible` or `package_untrusted`. In that case release the
     lost digest first as step 4 describes, then remove its record with
     `pohunek plugin uninstall <package> --digest <digest> --yes` (without
     `--remove-modified`, which refuses a missing root), then install the current
     release's package with `pohunek plugin install
     runtime/packages/<runtime>.tar.zst --catalog runtime/runtime-catalog.json
     --yes` (use `pohunek plugin update <package>` with the same archive and
     catalog when another version of the package is still installed) and
     re-pin each profile with `pohunek plugin profile migrate <name> --yes`. An uninstall without `--remove-modified` also clears the
     record when nothing pins it.
   - `root modified` or `root unsafe`: content, a mode, an owner or a link
     count changed after install. Enable, select and uninstall fail with
     `package_root_invalid`, launches with `runtime_incompatible`, and
     reinstalling the same archive is refused too, because the daemon never
     overwrites a present root. Go to step 3.
   - `root unreadable`: the daemon could not read the root, which says nothing
     about its content, so enable, select and uninstall answer
     `package_registry_failed`. Restore your ownership and read access below
     `~/.local/state/pohunek/plugins/` and run the doctor again.
   - `descriptor missing`, `descriptor invalid`, or `identity mismatch`: the
     root verifies but this daemon cannot load its `runtime.toml` as the
     recorded package. Install a version this daemon accepts with
     `pohunek plugin update <package> <archive> --sha256 <digest>` (or
     `--catalog <file>`), or uninstall the package.
   - `runtime not claimable` or `runtime conflict`: the package claims a
     runtime id it may not serve, or another package serves the same id.
     Disable one of the conflicting packages with
     `pohunek plugin disable <package> --digest <digest>`.
   - `unregistered root`: package files without a record, for example after an
     interrupted uninstall. Installing the same archive again adopts the root
     after verifying it.
   - `pinned but not installed`: a session or host profile pins a digest that
     is not installed; see sections 4 and 5.
   - `selection blocked by <digest>`: a session or profile still uses an
     incompatible version of the same package; see "Incompatible updates stay
     installed but unselected" in the runtime packages guide.
3. Remove a modified root with
   `pohunek plugin uninstall <package> --digest <digest> --remove-modified --yes`.
   A root that still verifies or is missing is refused with
   `package_root_intact`; uninstall it without the flag. Then reinstall the package from an archive whose digest
   you verified.
4. If that removal is refused with `package_referenced`, a session or a host
   profile pins the damaged digest. There is no in-place repair: sessions
   pinned to it fail to launch or resume with `runtime_incompatible`, and
   nothing falls back to another package. While anything pins the damaged
   digest, `plugin update` cannot select another version
   (`package_integration_incompatible`) and `plugin profile migrate` refuses
   it, so release the digest first: remove each pinning session with
   `pohunek session rm <id>`, and delete the `package` and `digest` lines from
   each pinning profile. Then repeat step 3, install a working version of the
   package, and re-pin each profile with
   `pohunek plugin profile migrate <name> --yes`.

## 2. An install fails verification

The install is refused before anything is recorded; the registry is unchanged.

- `package_archive_invalid`: the file is not a canonical package archive,
  breaks a size limit, or its SHA-256 differs from the `--sha256` digest.
  Recompute the digest of the file you have (`sha256sum`), compare it with the
  digest its publisher announced, and download the archive again on a
  mismatch. Never adjust `--sha256` to match a file you cannot vouch for.
- `plugin_source_invalid`: the archive or catalog path does not exist or is not
  a file; the CLI refuses it before contacting the daemon.
- `package_source_unreadable`: the daemon cannot open the resolved path as a
  regular file, for example a file its user cannot read.
- `package_descriptor_invalid`: the archive is intact but its runtime
  descriptor is not a valid runtime definition for this daemon, for example an
  integration handler or hook schema this core does not provide.
- `package_runtime_not_claimable`: a `--sha256` or `link` install claims
  `shell`, `codex`, `claude` or `hermes`. `shell` is never claimable; the other
  three are served only by an official package installed with `--catalog`.
- `package_identity_installed`: the same package id and version are already
  installed from a different archive. Compare the digests with
  `pohunek plugin list`; do not replace an installed version with other bytes
  under the same version.

## 3. An official install is refused

Official packages are installed from the daemon archive of the release the
daemon runs, with
`pohunek plugin install runtime/packages/<runtime>.tar.zst --catalog runtime/runtime-catalog.json`.
The checks are in `crates/daemon/src/session/package_lifecycle/trust.rs` and
`crates/daemon/src/catalog_anchor.rs`.

1. Run `pohunek doctor --json` and read the `catalog_trust_anchor` check.
   - `warn` and install error `official_trust_unavailable`: no
     `runtime-catalog-anchor.json` sits beside the running `pohunekd`. A
     source build carries none. Install the daemon from a release daemon
     archive with `./packaging/install-daemon.sh`, which copies the anchor into
     `<prefix>/libexec/pohunek/<version>/`. A version directory is immutable,
     so a version already installed without an anchor (a source build of the
     same version) cannot be republished with one: the install fails with
     `service_version_conflict`. Remove it first with
     `pohunek service uninstall`, which deletes every version directory no
     live session worker still uses and refuses while sessions are live unless
     `--stop-sessions` is given, then run the release installer.
   - `fail` and install error `official_trust_anchor_invalid`: the anchor
     exists but cannot be trusted. The file must be a regular file with one
     link, no symbolic link, not group or world writable, owned by you or root,
     and every directory above it must be owned by you or root and not group or
     world writable; on macOS no ACL entry may grant another principal write
     access. Fix the file or its directories (for example `chmod go-w`), or
     reinstall the release. The other causes are malformed content or a key id
     that does not match its key; reinstall the release in that case.
2. The daemon reads the anchor once at startup. Restart it after any change:
   `systemctl --user restart pohunek-<ns>-daemon.service` on Linux, or
   `launchctl kickstart -k gui/<uid>/io.github.zajca.pohunek.<ns>.daemon` on
   macOS.
3. `package_untrusted` means the catalog does not authorize this archive. The
   code covers every catalog refusal: a signature that does not verify against
   the anchor, a signer outside its validity window or revoked, an expired
   catalog, a catalog older than the newest one this host has accepted, and a
   catalog with no entry binding this package id, runtime id, version and
   digest. A file whose digest the catalog does not list at all, such as a
   damaged download of a listed archive, is refused with this code before it
   is decompressed. Signature, signer validity, expiry and high-water refusals
   are logged as an `a package catalog was refused` warning; a catalog without an
   entry binding this package id, runtime id, version and digest is refused
   without a log line. Check that the catalog and the package archive come from the same
   extracted release archive. Do not pair an archive with the catalog of
   another release, and never edit the catalog: its signature covers its
   canonical bytes.
4. `package_incompatible` means the entry does not support this daemon: every
   official entry pins exactly the core version of its release
   (`core = "=<version>"`), and current releases list only the Linux x86_64
   glibc and musl targets. Install the catalog and packages from the daemon archive of
   the version the daemon runs (`pohunek health` reports it); the Apple Silicon
   archive currently carries no package.

## 4. The selected package was disabled or removed

A fresh launch rechecks that its package is enabled and that its root verifies
(`verify_launchable` and `resolve_pinned` in
`crates/daemon/src/agent/host/handle.rs`). Nothing falls back to another
package, another version, a built-in or `shell`. The daemon compiles none of
Codex, Claude Code and Hermes in, so while no official package serves `codex`,
`claude` or `hermes` nothing does: a bare launch fails with
`agent_profile_not_found`, an unpinned `base = "codex"`, `base = "claude"` or
`base = "hermes"` profile with `runtime_not_installed`, and both hints name
`pohunek plugin install runtime/packages/<runtime>.tar.zst --catalog runtime/runtime-catalog.json`
(`codex`, `claude` or `hermes`) from the current release's archive.

- A fresh launch of a disabled package, or of a package that is no longer
  installed, fails with `runtime_not_installed`. Run `pohunek plugin list`;
  enable the package with `pohunek plugin enable <package>`, or install it
  again. A session already pinned to a disabled package can still be resumed.
- A resume of a session whose pinned digest is no longer installed fails with
  `runtime_not_installed`, and one whose pinned root fails verification fails
  with `runtime_incompatible`. Reinstall exactly the pinned archive (the
  digest is shown by `pohunek plugin doctor` as `pinned but not installed`);
  a newer version does not stand in for it. A `--catalog` reinstall of an
  official archive succeeds only while the daemon still runs the release whose
  catalog authorized it; after a daemon upgrade, release the lost digest and
  install the current release's package as section 1 step 4 describes.
- `runtime_served_by_package`: the session was launched from a built-in
  runtime, and an official package serves that id now. There is no built-in
  to return to: start a new session on the package.
- `runtime_builtin_removed`: the session was launched from the built-in Codex,
  Claude Code or Hermes runtime of an earlier release, which this daemon does
  not compile in, and no package serves its runtime id. Resume, fork and every
  interaction (input, attach, resize, rename, metadata) are refused; only stop
  and remove work. Install the official package from the release archive (the
  hint names the command): a live session can then be used again to finish its
  work, but it is never resumed or forked (a Hermes fork answers
  `agent_fork_unsupported`), so continue in a new session with
  `pohunek session new --agent <runtime>` (`codex`, `claude` or `hermes`).
  Install the Codex or Claude Code package on the earlier release before
  upgrading to keep live sessions usable; no earlier release archive carries
  the Hermes package, so finish or stop built-in Hermes sessions first.

## 5. A host profile pin fails

Run `pohunek plugin profile list` (implemented in
`crates/cli/src/commands/plugin_profile.rs`) and act on the profile's state:

- `needs_migration`: an installed package declares the profile's base, but the
  profile pins nothing or another package. While that package serves the
  base, launches fail with `invalid_profile`, and the message names the
  command to run:
  `pohunek plugin profile migrate <name>` previews the pin and changes nothing
  until it is repeated with `--yes`.
- `pin_not_installed`: the pinned digest is not installed. Reinstall that
  exact archive, or pin the profile to an installed version with
  `pohunek plugin profile migrate <name> --digest <digest> --yes`.
- `pinned` while launches fail with `runtime_not_installed`: the pinned package
  is disabled. Enable it, or migrate the profile to another digest.
- `unreadable`: fix the file's ownership, mode, or TOML; the state list in the
  runtime packages guide names every cause.

A profile pin on `codex`, `claude` or `hermes` has no built-in to fall back
to: a disabled or uninstalled package fails its launches with
`runtime_not_installed` until the package is enabled or installed again;
`pohunek plugin profile migrate <name>` refuses the built-in base `shell`
(`package_profile_base_builtin`) and a base no package serves
(`package_profile_target_invalid`). A migration changes the profile revision, so
a relay approval of the previous revision goes stale and must be approved
again.

## 6. Verify the recovery

1. `pohunek plugin doctor` prints `No problems found` and exits with status 0.
2. `pohunek plugin profile list` shows no profile in `needs_migration`,
   `pin_not_installed` or `unreadable`.
3. `pohunek host inspect local --json` lists the runtime as installed and
   supported; a profile whose pin cannot resolve is reported unavailable there.

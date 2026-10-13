---
type: Concept
id: concept/runtime-package-archive
title: Runtime package archive
description: The canonical deterministic tar.zst format of a runtime package, its strict reader, the size limits, safe extraction into a verified package root, the registry store, how to build and verify one, and the signed release baseline.
source_kind: manual
intents: [debug, help, project]
---

# Runtime package archive

A runtime package is shipped as one canonical `tar.zst` archive. Its identity is
the package digest: `sha256:` plus the lowercase hex SHA-256 of the archive
bytes. Implemented by the `pohunek-package` crate (`crates/package`). The crate
builds the archive, reads it into memory, extracts it into an owner-private
package root, re-verifies that root, and keeps the registry of installed
packages. The runtime manifest inside the package, signatures, and the daemon
wiring are separate layers.

## Canonical form

The same file set always produces the same bytes, whatever the input order, host
clock, user, or filesystem metadata.

- One zstd frame with a content checksum, written by the pure-Rust `ruzstd`
  encoder pinned to an exact version (0.9.1). A `ruzstd` upgrade can change the
  bytes and therefore every package digest, so it is a deliberate format
  decision. An encoder-drift test in `crates/package/tests/archive.rs` pins the
  digests of a large generated sample and of the released v0.34.2 Claude
  package's content rebuilt by the current encoder; the second is the input
  shape whose encoding changed between `ruzstd` 0.9.0 and 0.9.1.
- Inside, a POSIX USTAR stream of regular files only, sorted by path in
  ascending byte order. Per file: one header block, the data, zero padding to a
  512-byte block. The stream ends with exactly two zero blocks and nothing
  else.
- Fixed metadata: owner id 0, empty owner and group names, modification time at
  the epoch, no `prefix`, no link name, device numbers zero. The mode is `0644`,
  or `0755` for an executable file. The reader rebuilds the canonical header for
  every entry and requires the actual header to match byte for byte, so any
  other metadata is rejected.
- Directories are implied by file paths and never stored.

## Format generations

The reader accepts only frames the pinned encoder writes, so a `ruzstd` change
that alters the output is a cut-over: archives of one generation are refused by
the binaries of the other, with `NonCanonicalCompression`. pohunek 0.35 moved
from `ruzstd` 0.9.0 to 0.9.1 this way.

- Binaries of 0.35 and later refuse archives built before 0.35, including the
  archives published with 0.34.x; such an archive must be rebuilt with current
  tooling (`cargo xtask package build`). Third-party archives are affected
  alike.
- Binaries of 0.34.x refuse archives built by 0.35 and later, for example after
  a downgrade or through `--host` against an older daemon.
- Packages already installed keep working across an upgrade: the registry
  stores verified extracted roots with a per-file manifest, never the archive
  bytes, and re-verifies a root against that manifest without re-encoding.
  Sessions pinned to a package digest, `resume`, and the upgrade preflight do
  not read archive bytes.

## Paths

A path is relative, uses `/` separators, and each segment consists of ASCII
letters, digits, `.`, `_`, and `-`. It is at most 100 bytes (the USTAR name
field). Absolute paths, empty segments, `.` and `..` segments, non-UTF-8 names,
control characters, backslashes, spaces, and non-ASCII characters are rejected.
ASCII-only names keep extraction identical on case-insensitive and normalizing
filesystems. Two paths that differ only by letter case collide, and a path may
not be both a file and a directory prefix of another entry.

## Rejected input

The strict reader fails closed on: symlinks, hard links, devices, fifos,
directories, PAX and GNU extension headers, unknown type flags, duplicate or
unsorted entries, non-zero padding, data after the tar end marker, data after
the zstd frame, a missing or wrong zstd content checksum, zstd windows above the
limit, and every limit below. Errors are typed, identify an entry only by its
position, and never echo archive content (names or file bytes) into messages.

## Limits

Named constants in `crates/package/src/limits.rs`, applied while building and
reading. The compressed size is checked first, then the optional expected
digest, and only then is anything decompressed.

| Limit | Value |
| --- | --- |
| Compressed archive | 8 MiB |
| Decompressed tar stream | 64 MiB |
| Expansion ratio (decompressed to compressed) | 100 |
| Files | 512 |
| Path length | 100 bytes |
| One file | 16 MiB |
| zstd window | 8 MiB |

Decompression writes at most `min(64 MiB, compressed size x 100)` bytes of
output before it stops. The daemon decodes only archives whose raw digest is
already authorized by `--sha256` or by a verified catalog, and `plugin link`
decodes only the archive it just built from the owner's directory. `build_archive`
re-reads its own output under the same limits, so a built archive is always one
the reader accepts.

## Build and verify

```sh
cargo xtask package build <dir> -o <archive.tar.zst> [--evidence-dir <D>]
cargo xtask package verify <dir>
cargo xtask package affected [--base REF] [--json]
cargo xtask package baseline --catalog FILE --anchor FILE --release-dir DIR --version X.Y.Z [--json]
cargo xtask package bump --catalog FILE --anchor FILE --release-dir DIR --version X.Y.Z [--json]
```

`build` writes the canonical archive of a package directory (regular files
only; symlinks are rejected; any execute bit marks a file executable) and
prints its digest. `verify` builds the directory twice and requires identical
bytes. `affected` prints which official packages a change selects for the
per-PR package jobs; any core change selects all of them. The release pipeline
uses `verify` semantics to require byte-for-byte reproducibility from
independent clean roots. `baseline` decides which packages the signed release
proves unchanged; see "Release baseline".

`build --evidence-dir <D>` also writes the builder evidence of an official
package into `D`, as deterministic JSON (sorted keys at every level, two-space
indentation, one trailing newline, no timestamps, no absolute paths, `schema`
1), so two clean clones of one commit give byte-identical files:

- `<archive>.sha256`: the archive digest in `sha256sum -c` format, naming the
  archive file.
- `<rt>.manifest.json`: `digest`, `package_id`, `runtime_id`, `version`,
  `runtime_api` and `entries` (`path`, `size`, `sha256` hex, `executable` per
  file).
- `<rt>.provenance-input.json`: `subject` (archive `name`, `digest`),
  `builder` (`command`, `xtask_version`), `source` (`commit`, `path`
  `runtime-packages/<rt>`, Git `tree` of that path) and `materials` (`entries`
  and the lock's whole `upstream` object).
- `<rt>.compat-vector.json`: the package half of a compatibility attestation
  with its field names (`package_digest`, `package_id`, `runtime`,
  `upstream_lock_digest`, `upstream_version`, `suite_version`), plus
  `upstream` (the lock's whole `upstream` object), `supported` (`min`,
  `below`) and `targets` (the runtime's `compat/matrix.json` rows).

The directory must be `runtime-packages/<rt>` of a Git work tree whose
descriptor declares runtime `<rt>`. `git status` must report no modified,
untracked or ignored file in it, in `compat/<rt>/compatibility-lock.json` or in
`compat/matrix.json`. A clean status does not prove equal bytes
(skip-worktree and assume-unchanged entries, `core.autocrlf`,
`core.fileMode=false` and filters hide differences), so the bytes are compared
with the commit `HEAD` names, resolved once with replace refs ignored: the
archive entries must be exactly the blobs of that commit's tree (same paths,
mode `100755` exactly for executable entries, the Git blob id of each entry's
bytes, SHA-1 or SHA-256 per the repository's object format), and the lock and
matrix bytes must be the blobs that commit stores.

The lock's `upstream` object is copied whatever its kind (an npm package, a
provisioned runtime) with its keys sorted; only `upstream.release` is
required. A missing lock, a lock of another runtime or package, a lock without
`upstream.release` or with a fractional number in `upstream`, a pinned release
outside the lock's range, a runtime without matrix rows, an evidence directory
inside the package directory, an archive output that is not a regular file or
whose directory does not exist, an evidence path that is not a directory, a
missing evidence directory whose parent does not exist, and an evidence file
that is a symbolic link are refused too. Every refusal happens before anything
is written. The archive and the four files are written as temporary files
next to their targets (mode `0644`) and renamed into place one by one, archive
first, only after all five were written: a refusal or a failure while staging
writes nothing, while a rename that fails midway can leave the new archive
next to a mix of new and old evidence. An evidence directory the build created
is removed again on failure while it is still empty. The evidence is CI and release workflow output only
(`package-evidence-<rt>` per pull request, `package-evidence` in the release
build); it is not a release asset.

`build` and `verify` call `package::directory::build_directory_archive(dir, limits)`
(Unix), which the CLI's `plugin link` also uses. It walks regular files only
(a symlink or special file is `UnsupportedFileType`, a non-UTF-8 name is
`InvalidPath`), charges every file against the limits from the bytes actually
read, and compares the opened file's device, inode and type with the listing so
a file swapped for a symlink during the walk is `Changed` instead of followed.
`DirectoryError` carries no path or content.

## Release baseline

`cargo xtask package baseline` decides, per official package, whether the
package published with release `v<version>` is a trusted baseline for the
checkout: proof that the package, its compatibility lock and its fixtures are
byte-identical to what that release built and attested. The baseline release
is exactly the given version (the workspace version), never "latest". Run it in
the checkout; `--catalog` is the release's `runtime-catalog.json`, `--anchor`
the trust anchor (`packaging/runtime-catalog-anchor.json`) and `--release-dir`
the directory holding the release's `pohunek-runtime-<p>-<version>.tar.zst`
archives and `attestation-<p>-<target>.json` files. It reads no network.

Checks, in order; the first failure makes the package `missing` with that
reason code:

1. The catalog verifies against the anchor at the current time (signature,
   signer window, revocation, expiry), its release version equals `--version`
   and its sequence equals the one that version implies
   (`major << 40 | minor << 20 | patch`). A failure makes every package
   missing: `catalog_absent`, `catalog_unreadable`, `catalog_unverified`,
   `release_mismatch`.
2. The package is rebuilt in memory from `runtime-packages/<p>`, and the
   catalog has an entry with runtime id `<p>` and exactly that digest. Entries
   are matched by digest, never by version: `not_in_catalog` (no entry for the
   runtime, such as a package added after the release), `digest_mismatch`.
3. The released archive hashes to that digest: `archive_absent`,
   `archive_unreadable`, `archive_mismatch`.
4. The entry's attestations cover every row of `<p>` in `compat/matrix.json`
   (`platforms_uncovered`), and each referenced attestation file hashes to its
   catalog digest and names `<p>`, its platform, the package id, the release
   commit and binary set (`attestation_absent`, `attestation_unreadable`,
   `attestation_digest_mismatch`, `attestation_invalid`), the rebuilt package
   digest (`attestation_package_mismatch`), the digest of
   `compat/<p>/compatibility-lock.json` (`attestation_lock_mismatch`) and the
   matrix suite version (`attestation_suite_mismatch`).
5. `runtime-packages/<p>` and `compat/<p>` in the work tree equal the release
   commit: no tracked difference and no untracked file, ignored ones included,
   so captured screens and the npm lock count (`changed_since_release`). A
   local `node_modules` or log below `compat/<p>` therefore reports the
   package missing; CI runs on a clean checkout. A release commit the
   repository does not contain, as in a shallow clone, is
   `release_commit_unreachable`.

The command exits 0 with a verdict for every package. Inputs from the release
side (catalog, archives, attestations, commit) only ever make a package
missing, so the caller rebuilds and tests it; fail closed means "treat as
changed", never "skip". Only an unreadable checkout (a package directory that
does not build, a lock or the matrix that does not parse, an invalid anchor or
`--version`, a Git failure) exits non-zero with nothing on stdout. `--json`
prints one object on stdout and the human report on stderr:

```json
{"version":"0.34.2","packages":{"codex":{"verdict":"missing","reason":"digest_mismatch","digest":null,"version_not_bumped":true},"pi":{"verdict":"verified","reason":null,"digest":"sha256:...","version_not_bumped":false}}}
```

`digest` is the verified baseline digest (`null` when missing).
`version_not_bumped` is `true` when the catalog lists another digest of the
runtime and the version the checkout's `runtime.toml` declares is not strictly
above every released version of that runtime (semver; a version that is not
semver proves no bump): the content changed without a version bump, or with a
lower version. `baseline` reports it and does not refuse; the bump gate below
refuses it.

`cargo xtask package bump` takes the same verdicts and decides each package:
`fail` with `version_not_bumped`; `pass` for a verified package, a package new
to the release (no released entry of its runtime) and a package whose archive
equals a released one; `undecided` when the catalog file is absent (the
download failed). A catalog that is present but does not verify against the
anchor now, or names another release or sequence, fails with its catalog
reason. The overall `verdict` is the most severe package decision (`fail`,
then `undecided`, then `pass`). The command exits 2 only for `fail`; `--json`
prints the decisions on stdout.

A verified baseline proves the package inputs only. The attestations bind the
release's own binary sets, not the candidate core, so a baseline never replaces
running the package job when the core changed.

CI use: the `package-selection` job of `ci.yml` downloads the assets of release
`v<workspace version>` with `gh release download` and the read-only run token
into a directory outside the checkout and runs `baseline --json` with the
anchor `packaging/runtime-catalog-anchor.json`. A pull request runs package
`<p>`'s job when the core changed, `<p>` changed, or `<p>` is not `verified`; a
failed or timed-out download only makes every package missing. A non-zero exit fails the
selector, which runs every package job. The selector also passes each verified
digest to the package job, which requires its rebuilt archive to hash to it: a
difference is a non-reproducible build and fails the job. Release and
rehearsal workflows never run `baseline`; a tag builds and tests every package.

The `package version bump` job of `ci.yml` runs `bump` on pull requests. It
compares with the highest published release `vX.Y.Z` (non-draft,
non-pre-release, listed by `gh release list`) whose version does not exceed the
workspace version, so a workspace version that is being prepared still compares
with the release that shipped. The job warns and leaves the gate undecided when
no such release is listed, when its `runtime-catalog.json` cannot be downloaded
within `PACKAGE_BASELINE_FETCH_TIMEOUT`, or when the catalog is absent; it fails
on a `fail` verdict. It downloads only the catalog: a changed package is judged
by the catalog's entries, and an unchanged one passes. An empty download
directory is `undecided`, never `pass`.

## Install and verify a package root

`package::install::install_archive` extracts a verified archive into
`packages/<hex>/`, where `<hex>` is the lowercase hex of the archive digest
(content-addressed, no `sha256:` prefix). Layout of one root:

```text
packages/<hex>/
  manifest.json     per-file manifest, 0600
  files/...         the extracted tree, directories 0700, files 0600 or 0700
```

The manifest lives beside `files/` because every ASCII path is a valid archive
path, so no name inside the tree could be reserved for it. It lists, per file,
the path, size, SHA-256 and executable flag, plus the archive digest, as JSON
with unknown fields denied. File mode is `0600`, or `0700` when the canonical
archive mode is `0755`.

Extraction is descriptor-relative: after the `packages` directory is opened,
every create, mode change, sync and rename goes through directory descriptors
(`TrustedDir`), no symlink is followed and every file is created exclusively.
The tree is written under `.staging-<hex>`, fsynced (files and directories),
verified against its manifest, and only then moved with a no-replace rename to
`packages/<hex>/`. A crash leaves either no root or a complete verified one;
`collect_staging` removes the residue (`.staging-*`, `.collect-*`). Installs of
one `packages` directory must be serialized by the caller (the registry holds
its lock). Installing an archive whose digest is already present is idempotent
when the root verifies and holds the same files; a root that fails verification
or holds different files is never replaced.

`package::verify::verify_root` re-verifies a root. The daemon calls it before it
loads a package, launches a session from it, or mutates an integration. It
detects added, removed, modified or truncated files, wrong modes (including a
flipped executable bit), wrong owner, hard links, a file or directory replaced
by a symlink, FIFO or directory, and a missing, malformed or foreign manifest,
and returns a typed `VerifyError`. Errors identify entries by manifest position
only and never carry paths or contents. `VerifiedRoot::read_file` returns file
bytes that are hashed against the manifest.

Extraction applies the archive limits again (file count, per-file size, total
size, path rules) to the manifest it writes and reads back.

## Registry store

`package::registry::Registry` owns one plugin root, `<state>/plugins` (exact
`0700`, owner-private):

```text
registry.json     versioned record (schema 1, unknown fields denied), 0600
registry.lock     advisory lock serializing every mutation
packages/<hex>/   package roots
```

Every mutation is a transaction under the exclusive lock: collect staging and
temporary residue, read the record, change it, bump the `generation` counter and
atomically replace the record. A held lock is a typed `Busy` error, never a
wait. Reads are lock-free and see one whole committed record. A missing record
is the empty registry; a corrupt, oversized or unknown-schema record is a typed
error and is never overwritten or repaired.

A record holds, per package: archive digest, package identity (id and
version), source (`official`, `explicit_digest`, `link`), the recorded manifest
digest, the caller-supplied install time, and the enabled flag. `selected`
maps a package id to the digest bare requests use. The registry does not read
the clock or parse the runtime manifest; the caller supplies both.

- `install` verifies the archive against the expected digest and the limits,
  extracts and verifies the root, then records it. The same digest and identity
  is idempotent; the same digest under another identity, or an identity
  installed from another digest, is refused. A recorded package whose root is
  missing is extracted again; one whose root is present but invalid is refused.
- `set_enabled` and `select` change one flag; enabling or selecting requires a
  root that verifies.
- `uninstall` takes the set of retained digests (referenced by live workers,
  live or lost sessions and durable resume bindings, supplied by the daemon) and
  refuses while the digest is in it. A modified root is refused; a missing root
  is simply unrecorded. The record is replaced before the root is deleted.
- `remove_modified` is the separate removal for a package whose root fails
  verification (changed, added, retyped or missing-entry content, a changed
  mode or manifest), which `uninstall` refuses. It applies the same
  retained-digest refusal, acts only on a failing root (a verifying or missing
  root is `RootIntact`; use `uninstall`), un-records the package and any
  selection of it in one commit, then deletes the root through the same
  quarantine. A root the platform cannot delete (for example a hard-linked
  file) yields `RemovalIncomplete` after the package is already unrecorded.
- `retained_roots` verifies each referenced digest and returns `Ready` or
  `Incompatible` with a typed reason: not registered, root missing, or root
  invalid (including a manifest that differs from the recorded one). The
  runtime is then read-only; no other package and no shell stands in.
- `unregistered_roots` lists roots on disk the record does not name (left by an
  interrupted install or uninstall). Installing the same archive adopts one;
  nothing deletes it automatically.

Install publishes the root before it records it, and uninstall un-records
before it deletes, so the record never names a root that was never published.

### Catalog state

The registry also persists what the catalog verifier needs from its caller, in
`catalog-state.json` (schema 1, unknown fields denied, `0600`, at most 16 KiB)
beside the record and under the same lock. `catalog_state()` reads it without
the lock (a missing file is the empty state) and `record_catalog(&verified)`
merges a verified catalog: the high-water mark becomes the maximum of the
stored mark and the catalog sequence and revoked key ids are united (at most
`MAX_REVOKED_KEYS`, else `TooManyRevokedKeys`). A sequence below the stored mark
changes nothing and a merge that changes nothing writes nothing. A corrupt or
unknown-schema file is a typed error and is never replaced.

## Daemon host

The daemon opens `<state>/plugins` (`pohunek_paths::PLUGINS_SUBDIR`, exact
`0700`, owner-owned) at startup through `package::registry::Registry::open_at`;
an unsafe directory fails startup with `plugin_store_unavailable`. A registry
record that cannot be read keeps the daemon running on the built-in `shell`
and is reported by the host (package-pinned sessions are then incompatible).

- A package root holds `runtime.toml` at its top level (the runtime descriptor)
  and the detection manifest the descriptor names in
  `detect_manifest`, as a package-relative file. Both are read through the
  verified root, so only authenticated bytes are parsed.
- `PackageSource` loads, for fresh launches, the selected digest of each
  package id when it is enabled, re-verifying the root first. A package that
  fails (modified root, bad descriptor, descriptor identity differing from the
  record, a reserved runtime id of a package that is not catalog-authorized, or
  a runtime id claimed by two packages) is
  left out and reported with a typed reason; it never fails other packages or
  the built-ins.
- Reload is explicit (`SessionRegistry::reload_runtimes`); nothing watches the
  directory. A failed rebuild keeps the previous registry. Each committed
  `package.*` change (install, link, enable, disable, select, uninstall) calls
  it and reports `reloaded`.
- Fresh launches and integration changes re-verify the package root and require
  it to be enabled; a package modified, disabled or uninstalled after load
  never launches. Disabling or selecting another version retires a package for
  fresh launches only: sessions pinned to its digest keep resuming from that
  digest, and running sessions stay controllable while the registry records it.
- A pinned package that is not installed answers `runtime_not_installed`;
  installed content that is missing, modified or contradicts the pin answers
  `runtime_incompatible` (fixed message, typed cause in the daemon log).
- Uninstall (`SessionRegistry::uninstall_package`) builds the retained set from
  the durable session records, resume bindings and in-memory sessions and
  refuses with `StillReferenced` while any pins the digest. Fresh launches hold
  a shared guard from verification to their durable record, uninstall holds the
  exclusive guard, so a launch cannot pin a digest mid-uninstall.
- A session pinned to a package is observed (detection rules, input framing,
  profile recovery) and controlled from that pinned definition, also after the
  package is disabled or another version is selected, and an active agent that
  is the session's own runtime is covered by the same pin. Fork holds the shared
  authority until its registration persisted. The uninstall transaction runs in
  a task of its own, so a caller that stops waiting cannot release the exclusive
  authority mid-removal. The retained-set scan of the durable store is strict: a
  record that cannot be interpreted refuses the uninstall.
  The session's recovery binding also records whether its input safety contract
  (safe-text validation, no automated input while blocked) applies, so an
  adopted session whose package no longer verifies keeps those protections.
  Agent reports and releases from a pinned session, and the process matchers
  and detector rules of its observed agent, resolve against the definition the
  session was launched from (kept on the session entry), not the registry's
  current selection.
- The `package.list|inspect|doctor|install|link|set_enabled|select|uninstall`
  wire methods are local-only: the daemon refuses them on a remote overlay
  connection with `local_only_method`. They are additive and do not change the
  protocol version. Every mutation holds the lifecycle guard, so launches never
  interleave with it.
- Install and link validate the package in memory (`runtime.toml` parsed from
  the verified archive) before anything is extracted; the package identity (id
  and version) comes from the descriptor. An installed archive is decoded only
  after its raw digest is authorized: trust `explicit_digest` compares it with
  the supplied digest, and trust `catalog` requires a verified catalog to list
  it (else `package_untrusted`), so bytes nobody vouches for never reach the
  zstd decoder. `package.link` copies a developer directory into
  content-addressed storage and installs it disabled and unselected; the
  directory itself is never loaded.
- Claim rules: `shell` is never claimable. An official alias (a reserved runtime
  id other than `shell`) is served only by a package the signed catalog
  authorizes as official for that exact package id, runtime id and digest
  (`SourceTrust::Official` in the runtime registry, whatever the source order).
  No official alias has a built-in, so disabling or uninstalling the package
  leaves its alias served by nothing. A runtime id served by a loaded package of
  a different package id is a conflict. Trust `explicit_digest` requires the
  archive digest to equal the supplied digest and is never official, and a
  linked directory is never official, so both are refused for an alias; trust
  `catalog` fails closed with `official_trust_unavailable` on a host without a
  trust anchor and with `official_trust_anchor_invalid` on a host whose anchor
  cannot be trusted. Loading a recorded package re-checks that its record is
  `official` before it may serve a reserved id.
- A session is observed and resumed only through the runtime source its launch
  pin names (`RuntimeHost::resolve_pinned`). A built-in pin is served only by a
  built-in of the same package identity the registry serves now, so a built-in
  session of an alias an official package serves is refused with
  `runtime_served_by_package` (hint: a new session with `pohunek session new
  --agent <alias>`; no rebind) and, once adopted at startup, is observed with the
  generic shell manifest, its persisted input framing and its journaled hook
  schema only. The upgrade preflight reports any pin the new binary does not
  serve as `runtime_source_not_served`.
- `package.uninstall` is refused with `package_referenced` while a live, lost or
  resumable session or a host profile pins the digest. `remove_modified` removes
  only a root that fails verification; a verified root answers
  `package_root_intact`. A modified root makes enable, select and uninstall fail
  with `package_root_invalid`. `package.doctor` reports faults, unregistered
  roots on disk and pinned digests that are not installed.

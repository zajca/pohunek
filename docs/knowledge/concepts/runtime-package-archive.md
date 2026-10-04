---
type: Concept
id: concept/runtime-package-archive
title: Runtime package archive
description: The canonical deterministic tar.zst format of a runtime package, its strict reader, the size limits, safe extraction into a verified package root, the registry store, and how to build and verify one.
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
  encoder pinned to an exact version. A `ruzstd` upgrade can change the bytes
  and therefore every package digest, so it is a deliberate format decision (a
  test pins the sample archive digest).
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

Decompression writes at most `min(64 MiB, compressed size x 100)` bytes before
it stops, so a decompression bomb costs bounded memory. `build_archive`
re-reads its own output under the same limits, so a built archive is always one
the reader accepts.

## Build and verify

```sh
cargo xtask package build <dir> -o <archive.tar.zst>
cargo xtask package verify <dir>
```

`build` writes the canonical archive of a package directory (regular files
only; symlinks are rejected; any execute bit marks a file executable) and
prints its digest. `verify` builds the directory twice and requires identical
bytes. The release pipeline uses `verify` semantics to require byte-for-byte
reproducibility from independent clean roots.

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
- `retained_roots` verifies each referenced digest and returns `Ready` or
  `Incompatible` with a typed reason: not registered, root missing, or root
  invalid (including a manifest that differs from the recorded one). The
  runtime is then read-only; no other package and no shell stands in.
- `unregistered_roots` lists roots on disk the record does not name (left by an
  interrupted install or uninstall). Installing the same archive adopts one;
  nothing deletes it automatically.

Install publishes the root before it records it, and uninstall un-records
before it deletes, so the record never names a root that was never published.

## Daemon host

The daemon opens `<state>/plugins` (`pohunek_paths::PLUGINS_SUBDIR`, exact
`0700`, owner-owned) at startup through `package::registry::Registry::open_at`;
an unsafe directory fails startup with `plugin_store_unavailable`. A registry
record that cannot be read keeps the daemon running on the built-in runtimes
and is reported by the host (package-pinned sessions are then incompatible).

- A package root holds `runtime.toml` at its top level (the runtime descriptor
  shape of the built-ins) and the detection manifest the descriptor names in
  `detect_manifest`, as a package-relative file. Both are read through the
  verified root, so only authenticated bytes are parsed.
- `PackageSource` loads, for fresh launches, the selected digest of each
  package id when it is enabled, re-verifying the root first. A package that
  fails (modified root, bad descriptor, descriptor identity differing from the
  record, a reserved runtime id, or a runtime id claimed by two packages) is
  left out and reported with a typed reason; it never fails other packages or
  the built-ins.
- Reload is explicit (`SessionRegistry::reload_runtimes`); nothing watches the
  directory. A failed rebuild keeps the previous registry. The wire methods
  that call it arrive with the CLI lifecycle (#143).
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

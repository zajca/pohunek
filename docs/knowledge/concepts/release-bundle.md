---
type: Concept
id: concept/release-bundle
title: Release bundle
description: The release bundle that cargo xtask release assemble produces from the producer artifacts - its layout, the release policy file, the catalog sequence and expiry rules, the inventory the publisher uploads and verifies, and the fail-closed input checks.
source_kind: manual
intents: [debug, help, project]
---

# Release bundle

A release is published from one flat directory, the release bundle, built by
`cargo xtask release assemble` out of the artifacts the producer jobs
uploaded. The assembler recomputes every digest from bytes and writes the
bundle only when every check passed; the publisher uploads exactly the files
listed in the bundle's inventory. Implemented in `crates/xtask/src/release*.rs`;
the signed catalog it builds is described in the
[runtime catalog](runtime-catalog.md) and the per-row evidence it verifies in
[release attestation](release-attestation.md).

## Release policy

`packaging/release-policy.json` (schema 1, unknown members refused) is the one
source of the expected inventory:

- `catalog_validity_days`: days from the commit time until the catalog
  expires (365).
- `archives`: every `{component, target}` a release contains. `cli` and
  `daemon` for `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu` and
  `x86_64-unknown-linux-musl`, and `relay` for `x86_64-unknown-linux-gnu`.
  The archive name is `pohunek-<component>-<version>-<target>.tar.gz`, the
  names `packaging/stage-archive` produces.
- `sdk_packages`: `protocol`, `sdk` and `testkit`, shipped as
  `pohunek-ts-<name>-<version>.tgz`.

The official runtime packages are the `runtime-packages/*` directories and the
attested rows are `compat/matrix.json`; neither is repeated in the policy.

## Inputs

`--inputs` is a flat directory of downloaded producer artifacts under their
existing names: every policy archive and SDK tarball with its `.sha256` file,
one `pohunek-runtime-<runtime>-<version>.tar.zst` per official runtime (the
name carries the core release version, not the package's own version) and one
`attestation-<runtime>-<target>.json` per matrix row. The assembler refuses, in
this order and naming only the file class and name:

1. an input set that differs from the expectation: a missing file, a second
   file that repeats an expected slot (for example another package archive of
   a runtime), an unexpected file, a subdirectory or a link;
2. a `.sha256` file that is not `<64 hex>  <name>` or does not match the bytes;
3. a daemon archive that cannot be safely unpacked (member paths leaving the
   tree, links and special files, repeated members, member count and size
   limits) or whose `MANIFEST` does not match its unpacked bytes, names another
   component, version or target, records `signing unsigned-development`, or
   whose `runtime-catalog-anchor.json` differs from `--anchor` byte for byte;
4. any matrix row whose attestation does not equal the one recomputed from the
   unpacked binaries (binary-set digest), the package archive, the repository's
   lock, the commit and the target (`compat verify`).

## Catalog rules

- `sequence = major << 40 | minor << 20 | patch`, so it orders like the
  version. A version component of `2^20` or more is refused. The sequence is
  never read from the environment.
- `expires_at = commit time + catalog_validity_days * 86400`. The commit time is
  also the reference time for signing and for verifying against the anchor, so
  no wall clock is read and the same inputs give the same bytes.
- `core = "=<version>"` for every entry; `release.binary_sets` covers all daemon
  targets of the policy, each digest computed from the unpacked archive.
- An entry's `platforms` are exactly the targets with an attested matrix row.
  Today these are the Linux glibc and musl targets; the macOS arm64 daemon
  archive carries the signed catalog and no package until a macOS row exists.
- Each attestation digest is the SHA-256 of the attestation file bytes.
- The catalog is signed with `--key-file` (never printed) and must verify
  against `--anchor`.

## Layout

The bundle holds the policy archives (CLI and relay unchanged), the SDK
tarballs, their `.sha256` files, `runtime-catalog.json`, every package archive,
every attestation and `release-inventory.sha256`. Each daemon archive keeps the
producer tree and adds `runtime/runtime-catalog.json`, plus, for the packages
its target attests, `runtime/packages/<runtime>.tar.zst` and
`runtime/attestations/<runtime>-<target>.json`. Its `MANIFEST` is rewritten with
the producer's signing state and minimum macOS version through
`packaging/write-manifest`, and the archive and its `.sha256` are regenerated
with `packaging/archive` stamped by the commit time, so a rerun is
byte-identical.

## Inventory

`release-inventory.sha256` lists every file of the bundle except itself as
`<64 hex>  <name>` (`sha256sum -c` format), names without directories, sorted.
It is the exact list the publisher uploads. `cargo xtask release
verify-inventory --dir <bundle> [--now <seconds>]` requires the directory to
hold exactly the listed regular files with the listed digests, verifies the
top-level catalog against the anchor inside every daemon archive (at `--now`,
default the current time), and requires each daemon archive to carry that
catalog, the binary set the catalog names for its target and exactly the
packages and attestations the catalog lists for that target, byte-identical to
the top-level files.

## Failure behavior

The bundle is built in a hidden sibling directory of `--output` and renamed
into place at the end. `--output` must not exist (an empty directory is
accepted). On any refusal nothing is left behind.

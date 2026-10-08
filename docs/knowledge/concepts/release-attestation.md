---
type: Concept
id: concept/release-attestation
title: Release attestation
description: The compatibility attestation a release emits per runtime package and target, the canonical binary-set digest, the compat/matrix.json required-row file, the xtask compat commands, and the trust model of the attestation document.
source_kind: manual
intents: [debug, help, project]
---

# Release attestation

Before a release lists an official runtime package for a target, the
out-of-process consumer suite must have run that package on that target against
the real upstream runtime. Each successful run yields one attestation: a small
JSON document binding the result to the exact bytes that were tested.
Implemented in `crates/xtask/src/attestation.rs`.

## The attested tuple

| Field | Meaning |
| --- | --- |
| `schema` | Always `1`. |
| `runtime`, `package_id` | Runtime id and package id of the package descriptor. |
| `commit` | The full 40-character lowercase hex commit the artifacts were built from. |
| `target`, `platform` | The core target triple. `platform` is the string the daemon reports as its host platform; the two are equal today. |
| `binary_set_digest` | Digest of the three core executables (see below). |
| `package_digest` | `sha256:` plus the SHA-256 of the package archive bytes (the [package digest](runtime-package-archive.md)). |
| `upstream_lock_digest` | `sha256:` plus the SHA-256 of `compat/<runtime>/compatibility-lock.json`. |
| `suite_version` | The `suite_version` of `compat/matrix.json`. |
| `upstream_version` | The upstream release the lock pins; it must lie in the lock's `[min, below)` range. |

The document is canonical: keys sorted, indented JSON, one trailing newline,
mode 0644, byte-identical on rerun.

## Binary-set digest

`sha256:<hex>` of the SHA-256 over `pohunek-binary-set-v1\n` followed, for
`pohunek`, `pohunek-sessiond` and `pohunekd` in ascending byte order, by
`<name>\0<64 lowercase hex SHA-256 of the file bytes>\n`. The file set is the
installer's `BINARIES` list. A directory missing one of them, or holding a
symbolic link or non-regular file in its place, is refused. The catalog's
`release.binary_sets` carries the same digest per target.

## Matrix file

`compat/matrix.json` is `{"schema": 1, "suite_version": N, "rows": [{"runtime",
"target"}, ...]}` with rows in strictly ascending `(runtime, target)` order. It
holds one row for every directory under `runtime-packages/` and every declared
target (`x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`). Bump
`suite_version` whenever the semantics of the consumer suite change (what it
asserts or how it drives a runtime), so older attestations stop matching.
Adding a package directory or a target requires the matching rows in the same
change.

## Commands

- `cargo xtask compat attest --report <consumer-report> --bin-dir <dir>
  --package <archive> --lock <lock> --matrix compat/matrix.json --commit <sha>
  --target <triple> --output <file>` recomputes every value from the bytes and
  writes the document only when the report agrees: its `suite_version` equals
  the matrix's (a report from an older suite is refused after a bump), same
  runtime as the lock and package descriptor, `(runtime, target)` is a matrix row, executable hashes
  equal the files in `--bin-dir` (the bytes that will be bundled), package
  digest equals the archive's, and the upstream version equals the lock's pinned
  release.
- `cargo xtask compat verify --attestation <file>` plus the same
  `--bin-dir/--package/--lock/--matrix/--commit/--target` recomputes every field
  and requires the document to equal it and to be canonically rendered. The
  release assembler runs the same check.
- `cargo xtask compat matrix-check [--root <dir>]` (offline) requires the matrix
  rows to be exactly the official package directories times the declared
  targets, and every package to have a lock for its runtime.

The consumer report is `{"schema": 1, "runtime", "suite_version", "package_digest",
"upstream_version", "executables": {name: sha256 hex}}`. `--output` must not be
the same file (path or hard link) as the report, package, lock, matrix or any
executable; such an output is refused before anything is written.

A refusal is a typed error naming the field (for example
`executables.pohunekd` or `upstream_lock_digest`); it never echoes file content.

## Trust model

The attestation JSON is untrusted data. No consumer takes a digest from it:
the assembler recomputes each digest from the bytes it downloaded and compares.
Provenance of the attestation itself (that the consumer suite really ran on the
release commit) comes from the release workflow that produces it, not from the
document.

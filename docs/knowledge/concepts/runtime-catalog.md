---
type: Concept
id: concept/runtime-catalog
title: Runtime catalog
description: The signed catalog that authorizes official runtime packages, its canonical bytes, key rotation and revocation rules, and the local explicit-digest trust that can never authorize an official runtime.
source_kind: manual
intents: [debug, help, project]
---

# Runtime catalog

Official runtime packages are authorized by a signed catalog that binds each
package archive digest (see the
[runtime package archive](runtime-package-archive.md)) to a package id, runtime
id, version, runtime API version, platforms, one attestation digest per
platform and a core version range. A release block names the core release
(version, source commit and the binary set digest of every target) the catalog
belongs to. A third-party archive is trusted only by an explicit owner-supplied
digest. Packages carry no signature
of their own. Implemented in `crates/package` (`catalog.rs`,
`canonical_json.rs`); the crate verifies catalogs, while installing,
the registry and the CLI are separate layers.

## Trust model

- The **trust anchor** is supplied by the caller: root Ed25519 public keys with
  a validity window (`not_before <= now < not_after`, Unix seconds) and the
  key ids revoked so far. The crate embeds no production key; the daemon reads
  the anchor from the file the release bundle ships beside it (see
  [Trust anchor file](#trust-anchor-file)).
- A **key id** is the lowercase hex SHA-256 of the 32 raw public key bytes.
- A catalog may carry a `key_chain` of key records. Each record names a new key
  (id, public key, window) and the earlier key that endorses it
  (`endorsed_by`), with the endorser's signature over the record. Records must
  appear after their endorser, so a chain starts at a root. This is how
  signing keys rotate without shipping a new anchor.
- A catalog is accepted when at least one of its detached signatures verifies
  (`verify_strict`) from a key that is valid at `now`, whose whole lineage up to
  the root is valid at `now`, and none of whose lineage is revoked. An expired
  root therefore ends the life of every key chained from it.
- **Revocation**: a key id is revoked by the anchor's list or by the catalog's
  own `revoked_key_ids`, which applies to the signer and its lineage (a catalog
  that revokes its own signer is rejected). The caller adds
  `VerifiedCatalog::revoked_key_ids()` to the anchor's list for later checks.
  `revoked_digests` are never authorized, and an entry whose digest is revoked
  makes the catalog invalid.
- **Anti-rollback**: `sequence` is a signed, monotonically increasing counter.
  The caller persists the highest accepted sequence and passes it as the
  high-water mark (`Registry::record_catalog` persists it together with the
  accumulated revoked key ids in `catalog-state.json`, and
  `Registry::catalog_state` reads them back); a lower sequence is rejected, an equal one is accepted so an
  installed catalog can be re-verified. `expires_at` bounds how long a frozen
  catalog is honored.

## Document format

`runtime-catalog.json` is one JSON object:

```json
{
  "schema_version": 2,
  "catalog": {
    "schema_version": 2,
    "sequence": 7,
    "expires_at": 1790000000,
    "release": {
      "version": "0.35.0",
      "commit": "<40 hex>",
      "binary_sets": [
        { "target": "x86_64-unknown-linux-gnu", "digest": "sha256:<64 hex>" }
      ]
    },
    "revoked_key_ids": [],
    "revoked_digests": [],
    "entries": [
      {
        "package_id": "pohunek.runtime.codex",
        "runtime_id": "codex",
        "version": "1.0.0",
        "digest": "sha256:<64 hex>",
        "runtime_api": 1,
        "platforms": ["x86_64-unknown-linux-gnu"],
        "attestations": [
          { "platform": "x86_64-unknown-linux-gnu", "digest": "sha256:<64 hex>" }
        ],
        "core": ">=0.33.0, <0.40.0"
      }
    ]
  },
  "key_chain": [],
  "signatures": [{ "key_id": "<64 hex>", "signature": "<128 hex>" }]
}
```

Every object rejects unknown fields. Numbers are non-negative integers only.
Hex is lowercase. `core` is a semver range that must constrain the version.
Only schema 2 is read: a document declaring another `schema_version` is refused
with `UnsupportedSchemaVersion` before its shape is judged, and there is no
compatibility path for schema 1.

- `release.version` is plain `X.Y.Z` (no pre-release or build part) and
  `release.commit` is 40 lowercase hex characters. `release.binary_sets` lists
  one `{target, digest}` per target, at least one, targets unique, each a
  `[a-z0-9._-]` name. `digest` is `sha256:<64 hex>` of the canonical set of
  CLI, daemon and session worker binaries for that target; the catalog treats it
  as opaque. The release tooling writes the list in ascending target order.
- `runtime_api` is the package's runtime API version, at least 1; the tooling
  reads it from the archive's `runtime.toml`.
- `attestations` holds one `{platform, digest}` per entry platform, with
  `digest` = `sha256:<64 hex>` of the attestation file bytes; the tooling writes
  them in ascending platform order.

The daemon does not enforce the release block, `runtime_api` or the
attestations yet: they are verified, signed and exposed (`VerifiedCatalog::release()`,
`VerifiedEntry::runtime_api()` and `VerifiedEntry::attestations()`), and the
authorization decision is unchanged.

## Canonical signed bytes

The signature covers `pohunek-runtime-catalog-v2\n` followed by the canonical
JSON of the `catalog` member: object keys in ascending byte order, no
insignificant whitespace, strings escaped by `serde_json`. A key record
endorsement covers `pohunek-catalog-key-record-v1\n` plus the canonical JSON of
the record without its `signature`. The separators keep a signature from being
replayed as another kind of signature; the catalog separator carries the schema
version, so a signature over a schema 1 document never verifies a schema 2
document and the reverse. Whitespace and key order of the file do
not matter; the verifier recomputes the canonical bytes. Input with duplicate
object keys, trailing data, floats, negative numbers or more than 1 MiB is
rejected before any signature is considered.

## Entry rules

- `shell` is never a valid `runtime_id` in a catalog and is never authorized.
- A catalog is the only way to authorize `codex`, `claude` or `hermes`; the
  daemon records such a package as `official` and lets only an `official` record
  serve the id (see the runtime packages guide):
  `VerifiedCatalog::authorize(package_id, runtime_id, digest)` returns
  `Official` only for an entry matching all three, and `NotAuthorized`
  otherwise (including revoked digests).
- A runtime id maps to exactly one package id and the reverse. `(package_id,
  version)` and `digest` are each unique. Platforms are a non-empty set of
  `[a-z0-9._-]` names.
- Every entry platform has exactly one attestation, and every attestation
  platform is an entry platform (`MissingAttestation`, `AttestationPlatform`;
  an empty, oversized, repeated or badly named list is `Attestations`).
- Every entry platform is a `release.binary_sets` target (`NoBinarySet`). The
  platform string is the core target triple the daemon reports for its host,
  such as `x86_64-unknown-linux-gnu` or `aarch64-apple-darwin`.
- Every entry's `core` range admits `release.version`
  (`CoreExcludesRelease`) and `runtime_api` is at least 1 (`RuntimeApi`).
- The release block itself is refused with `CatalogError::Release` for a bad
  version or commit, an empty or badly named binary set list, or a repeated
  target. A malformed digest (not `sha256:` plus 64 lowercase hex) is a
  `Schema` error.
- `LocalTrust::ExplicitDigest(digest)` authorizes an archive only when its
  digest equals the owner-supplied one and its runtime id is not `shell`,
  `codex`, `claude` or `hermes`. Its result type is `LocalAuthorization::Local`,
  which cannot be confused with `Authorization::Official`.

## Trust anchor file

The daemon reads its anchor once at startup from `runtime-catalog-anchor.json`
in the directory of its own executable (`<prefix>/libexec/pohunek/<version>/`
of an installed layout). No environment variable or option selects another
file, and no key is compiled in. The file holds public keys only:

```json
{
  "schema_version": 1,
  "roots": [
    {
      "key_id": "<64 hex>",
      "public_key": "<64 hex>",
      "not_before": 1,
      "not_after": 4102444800
    }
  ],
  "revoked_key_ids": []
}
```

A release lists two roots with distinct custody: an offline primary root held
by the owner, valid for the long term, and a CI secondary root that signs
catalogs from the release pipeline, valid for a shorter window and revocable
through `revoked_key_ids` without touching the primary. A catalog verifies when
either listed root signed it inside that root's window; revoking the CI key id
refuses the catalogs it signed while the primary root keeps verifying:

```json
{
  "schema_version": 1,
  "roots": [
    { "key_id": "<64 hex, CI>", "public_key": "<64 hex>", "not_before": 1790000000, "not_after": 1810000000 },
    { "key_id": "<64 hex, primary>", "public_key": "<64 hex>", "not_before": 1, "not_after": 4102444800 }
  ],
  "revoked_key_ids": []
}
```

(roots appear in ascending `key_id` order; the example values are illustrative.)

Every object rejects unknown fields; the strict JSON subset of the catalog
applies and the file is at most 16 KiB. `key_id` is a cross-check: it must equal
the SHA-256 of `public_key`, otherwise the whole anchor is refused. Up to 8
roots and 64 revoked key ids.

The daemon distinguishes three states, reported by `daemon.doctor` as the
`catalog_trust_anchor` check:

- **Absent** (no file): `plugin install --catalog` answers
  `official_trust_unavailable`; the doctor check is `warn`. Development builds
  and any install without a bundled anchor are here.
- **Invalid** (the file or its location cannot be trusted): `--catalog` answers
  `official_trust_anchor_invalid`, the doctor check is `fail`, and the rest of
  the daemon keeps running, including installs trusted by explicit digest. An
  anchor is never partly used and never treated as absent. Causes: the daemon
  executable location is unknown, the file or directory cannot be read, a
  directory on the executable's path or the file is writable by another
  account or owned by one (only the daemon's user and root are accepted, and
  directories must not be group or world writable), the file is a symbolic
  link, not a regular file, hard-linked, group or world writable, carrying a
  macOS extended ACL entry that lets another principal change it (an ACL that
  cannot be read counts as unreadable), larger than the limit, malformed, has a key id that does not match its key, an empty
  validity window or repeated roots.
- **Loaded**: the check is `ok`.

The integrity bar equals the daemon binary's: whoever can rewrite the anchor
can replace `pohunekd`. The file is public, so unlike the owner-private stores
it may be `0644` or `0444`; only group or other write permission is refused,
and on macOS an allow ACL entry carrying a write, delete, append, attribute,
security or ownership permission counts as write permission (read-only allow
entries and deny entries are accepted).
The anchor is read at startup, so changing it requires a daemon restart.

## Release tooling

`cargo xtask catalog` builds, signs and verifies catalogs; it embeds no key and
the signing key never appears in a command line, an environment variable, a
log or an output:

- `catalog build --spec <file> --output <file>` turns a spec (`schema_version`
  2, `sequence`, `expires_at`, `release` `{version, commit, binary_sets}`,
  `revoked_key_ids`, `revoked_digests` and `packages`, each `{archive,
  platforms, attestations, core}` with `archive` relative to the spec) into the
  unsigned catalog body. Digest, package id, runtime id, version and
  `runtime_api` are read from the archive and its `runtime.toml`; the spec
  cannot supply them. `core` is mandatory in the spec. The body is checked
  with the rules the verifier applies (`package::check_catalog`) before it is
  written, so a catalog the daemon would refuse is not produced. Entries are
  sorted by package id and version, and platforms, binary sets and attestations
  by name, so the output is deterministic.
- `catalog sign --catalog <file> --key-file <file> --key-id <hex> --output
  <file>` signs the body. The key file holds the 32-byte Ed25519 seed as 64
  lowercase hex characters; it must be a regular file owned by the caller with
  no group or other permission bits, one link and no symlink, and on macOS no
  extended ACL beyond deny entries, judged on the opened handle, or the command
  refuses it (an unreadable ACL is refused too). `--key-id` must equal the id
  derived from the key. The result is verified against an anchor of the
  signer's own key before it is written, so an expired catalog or an invalid
  entry is refused instead of emitted. Signatures are deterministic.
- `catalog verify --catalog <file> --anchor <file> [--high-water N]` checks a
  catalog against an anchor file exactly as the daemon does and lists its
  entries.
- `catalog anchor --root <public-key-file>:<not-before>:<not-after>...
  [--revoked-key-id <hex>]... --output <file>` writes the anchor file for one
  or more roots. Each `--root` keeps one root's three values together: the file
  holding its 64-hex public key, the first Unix second it may sign
  (inclusive) and the Unix second from which it may no longer sign
  (exclusive). The window is mandatory; there is no default. The window values
  are split off from the right, so the path may contain colons. The root count
  (1 to 8), repeated keys, empty windows and the revoked-id limit are judged by
  the same anchor validation the daemon applies, and a refused anchor writes no
  file. The roots are written in ascending key id order, so the argument order
  does not change the bytes.
- `catalog public-key --key-file <file>` prints the key id and public key.

A catalog has no per-entry expiry: `expires_at` covers the whole document, and a
catalog that lists a digest it also revokes is invalid as a whole. Signing-key
custody is the owner's; the tooling holds no key. The production anchor and its
roots are fixed by the next section.

## Production trust anchor

`packaging/runtime-catalog-anchor.json` is the anchor every release ships. It
lists two roots and no revoked key ids:

| Role | Key id | Valid from (inclusive) | Valid until (exclusive) |
| --- | --- | --- | --- |
| CI secondary, signs routine release catalogs | `e68204fe73c49ca6df7892bda5bdbb9a1694bf240e0e2d8ed80268574c8cd771` | 1791417600 (2026-10-08T00:00:00Z) | 1854576000 (2028-10-08T00:00:00Z) |
| Offline primary, held by the owner | `f52cf66e089130ee817e18db5b52db21ee6b002c29615f47d80b168931ba89fe` | 1791417600 (2026-10-08T00:00:00Z) | 2107036800 (2036-10-08T00:00:00Z) |

The inputs are public only: `packaging/catalog-trust/primary.pub` and `ci.pub`
(64 lowercase hex characters and a newline each) and
`packaging/catalog-trust/roots.txt` (one `<key-file>:<not-before>:<not-after>`
line per root, the key file relative to `roots.txt`). The finished anchor is
checked in and a test in `crates/xtask/src/catalog.rs` regenerates it with the
`catalog anchor` code path and requires byte equality, so the file cannot drift
from its inputs.

Rotation rule: the CI root is rotated before its `not_after` by a `key_chain`
record endorsed by the primary, and a new anchor is shipped with the next
release. The CI root signs routine release catalogs; the primary signs only
CI-key endorsement and CI-revoking catalogs.

Daemon archives (`pohunek-daemon-*` for glibc, musl and macOS arm64, the only
archives holding `pohunekd`) carry `runtime-catalog-anchor.json` at the archive
root, beside the `pohunekd` executable, with mode `0644`; `packaging/stage-archive`
refuses to stage a daemon archive without it. CLI and relay archives carry no
anchor.

`pohunek service install` and `upgrade` copy the staged anchor into the version
directory next to the daemon (`<prefix>/libexec/pohunek/<version>/`, mode `0644`)
so the `catalog_trust_anchor` doctor check is `ok` for an installed release
layout. An anchor that is not a valid trust anchor is refused with
`service_staged_anchor_invalid` and nothing is published; a build without one
installs and the daemon stays in the absent state. A version directory is
immutable, so republishing it with a different or missing anchor is
`service_version_conflict`.

## Limits

Named constants in `crates/package/src/catalog.rs` and `release.rs`: catalog 1
MiB, 256 entries, 16 platforms per entry (64 bytes each), 16 attestations per
entry (one per platform), 16 binary sets per release, core range 128 bytes,
16 key records,
8 signatures, 64 revoked key ids, 1024 revoked digests, 8 anchor roots.
Errors are typed (`CatalogError`) and never carry catalog content.

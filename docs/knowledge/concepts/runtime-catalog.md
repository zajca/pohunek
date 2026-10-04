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
id, version, platforms and a core version range. A third-party archive is
trusted only by an explicit owner-supplied digest. Packages carry no signature
of their own. Implemented in `crates/package` (`catalog.rs`,
`canonical_json.rs`); the crate verifies catalogs, while installing,
the registry and the CLI are separate layers.

## Trust model

- The **trust anchor** is supplied by the caller: root Ed25519 public keys with
  a validity window (`not_before <= now < not_after`, Unix seconds) and the
  key ids revoked so far. The crate embeds no production key; the release
  bundle provides the anchor.
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
  "schema_version": 1,
  "catalog": {
    "schema_version": 1,
    "sequence": 7,
    "expires_at": 1790000000,
    "revoked_key_ids": [],
    "revoked_digests": [],
    "entries": [
      {
        "package_id": "pohunek.runtime.codex",
        "runtime_id": "codex",
        "version": "1.0.0",
        "digest": "sha256:<64 hex>",
        "platforms": ["x86_64-unknown-linux-gnu"],
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

## Canonical signed bytes

The signature covers `pohunek-runtime-catalog-v1\n` followed by the canonical
JSON of the `catalog` member: object keys in ascending byte order, no
insignificant whitespace, strings escaped by `serde_json`. A key record
endorsement covers `pohunek-catalog-key-record-v1\n` plus the canonical JSON of
the record without its `signature`. The separators keep a signature from being
replayed as another kind of signature. Whitespace and key order of the file do
not matter; the verifier recomputes the canonical bytes. Input with duplicate
object keys, trailing data, floats, negative numbers or more than 1 MiB is
rejected before any signature is considered.

## Entry rules

- `shell` is never a valid `runtime_id` in a catalog and is never authorized.
- A catalog is the only way to authorize `codex`, `claude` or `hermes`:
  `VerifiedCatalog::authorize(package_id, runtime_id, digest)` returns
  `Official` only for an entry matching all three, and `NotAuthorized`
  otherwise (including revoked digests).
- A runtime id maps to exactly one package id and the reverse. `(package_id,
  version)` and `digest` are each unique. Platforms are a non-empty set of
  `[a-z0-9._-]` names.
- `LocalTrust::ExplicitDigest(digest)` authorizes an archive only when its
  digest equals the owner-supplied one and its runtime id is not `shell`,
  `codex`, `claude` or `hermes`. Its result type is `LocalAuthorization::Local`,
  which cannot be confused with `Authorization::Official`.

## Limits

Named constants in `crates/package/src/catalog.rs`: catalog 1 MiB, 256 entries,
16 platforms per entry (64 bytes each), core range 128 bytes, 16 key records,
8 signatures, 64 revoked key ids, 1024 revoked digests, 8 anchor roots.
Errors are typed (`CatalogError`) and never carry catalog content.

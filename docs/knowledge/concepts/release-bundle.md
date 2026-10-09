---
type: Concept
id: concept/release-bundle
title: Release bundle
description: The release bundle that cargo xtask release assemble produces from the producer artifacts - its layout, the release policy file, the catalog sequence and expiry rules, the inventory the publisher uploads and verifies, the fail-closed input checks, and the release workflows that build, sign and publish it with their trust and permission model.
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

## Archive smoke

`packaging/smoke-archive` takes a final daemon archive from the bundle, the
staged upstreams (`<stage>/<runtime>/` with `STAGE.sha256`) and the prebuilt
release consumer executable, and runs the consumer once per shipped package in
a network- and PID-isolated namespace (`unshare` through a user namespace, or
`sudo -n unshare` with a `setpriv` drop; no un-isolated fallback). The runtimes
are the `runtime/packages/*.tar.zst` the archive carries; a shipped package
without a staged upstream fails. Each consumer report must name the catalog
entry's package digest and the SHA-256 of the archive's own binaries. The
procedure and its limits are in `docs/development.md` ("Archive smoke").

## Release workflows

Three workflow files build and publish a release; the structure below is pinned
by `scripts/tests/test_release_workflow.py`.

| File | Role |
| --- | --- |
| `release.yml` | Tag trigger, the quality gate, the macOS jobs, two calls and the single `publish` job. |
| `release-build.yml` | Reusable producers: Linux archives, one job over `runtime-packages/*`, SDK tarballs. |
| `release-evidence.yml` | Reusable evidence: rows, provenance, assembly and signing, bundle provenance, archive smoke, verdict. |

`ci.yml` calls the two reusable files with `mode: rehearsal` (jobs
`release-rehearsal-build`, `release-rehearsal`); `release.yml` calls them with
`mode: release`.

### Flow

Producers upload workflow artifacts only: `build-<component>-<target>`,
`package-archives`, `sdk-release-assets` and the macOS `macos-signed-<component>`.
The evidence workflow plans the rows from `compat/matrix.json` with `jq`; every
row extracts the binaries from the daemon ARCHIVE of its target, stages the pinned
upstream and runs the consumer suite, then `compat attest` writes
`attestation-<runtime>-<target>.json`. `attest` attests every producer artifact;
`assemble` verifies every input with `gh attestation verify` (signer workflow
`release-evidence.yml`, signer and source digest equal to the release commit,
source ref equal to the tag, GitHub-hosted runners only), collects the flat input
directory and runs `cargo xtask release assemble` with the commit time from
`git log`, then `verify-inventory`. `attest-bundle` attests every file of the
bundle, `smoke` runs `packaging/smoke-archive` on every Linux daemon archive
with the isolation strategy a probe step proved available, and `verdict` fails
unless exactly the jobs of the mode succeeded (a skipped job counts as success
to a caller). The bundle is the artifact `release-bundle`.

`publish` is the only job with `contents: write`. It needs every producer and
the evidence call, downloads only `release-bundle`, and uses `gh` and coreutils
only: it checks the bundle against `release-inventory.sha256` (exactly the listed
files plus the inventory, every versioned name carrying the tag's version),
verifies each file's provenance, requires the tag to resolve to the run's commit
and no release of the tag to exist (the releases listing includes drafts), creates
a draft with `gh release create --draft --verify-tag`, uploads the bundle, compares
asset names, digests and sizes read back from the API with the bundle, publishes
with `gh release edit --draft=false`, re-checks the tag binding before and the
assets after, and deletes the draft it created if any step fails. No other job
creates, edits or attaches to a release, so a visible release is complete or absent.

### Trust and permissions

- Every workflow default token is `permissions: {}`; each job widens it to
  `contents: read`, except `publish` (`contents: write`) and the two attest jobs.
- The OIDC scopes (write access to `id-token` and `attestations`) exist only on
  `attest` and `attest-bundle`, which download artifacts, check checksums and attest; they
  check out nothing and run no repository code, and are skipped in a rehearsal.
  The caller jobs (`evidence` in `release.yml`, `release-rehearsal` in `ci.yml`)
  grant the same scopes because a called workflow cannot hold a scope its caller
  lacks.
- The signing seed is the secret `CATALOG_SIGNING_KEY_CI` of the environment
  `release` (deployment policy: tags `v*`), read only by the signing step of
  the `assemble` job, which exists only in release mode and restores no cache.
  `scripts/release-workflow/assemble` writes it to a 0700 tmpfs directory with
  mode 0600, removes it from the environment before the first program starts
  and shreds it on exit. The key id handed to xtask is the root of
  `packaging/runtime-catalog-anchor.json` that carries `packaging/catalog-trust/ci.pub`,
  so a seed of any other root (the offline primary) is refused, and the catalog
  must verify against the anchor.
- A rehearsal has no secret, no environment and no provenance. The daemon
  archives are staged with a throwaway anchor written over
  `packaging/runtime-catalog-anchor.json` in the build job's own checkout, and
  `assemble-rehearsal` signs with the key derived from
  `sha256("rehearsal <run id> <commit>")`; it refuses a tag, so a rehearsal key
  never signs anything a release trusts and nothing is published. It runs the
  full policy minus the targets it does not build (the macOS archives).
- Every action is pinned to a commit; untrusted values reach scripts through
  `env:`, never through expression interpolation.

What a rehearsal cannot show, and only a tag run does: signing with the production
key against the production anchor, `gh attestation verify` against real
attestations, the macOS jobs, and the draft/publish steps of `publish`.

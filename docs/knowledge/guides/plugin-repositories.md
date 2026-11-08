---
type: Guide
id: guide/plugin-repositories
title: Plugin repositories
description: Where a new runtime package lives, how its repository and package id are named, the CI and compatibility declaration it carries, how the plugin index admits it, how a core release snapshots the official set, and where its documentation lives.
source_kind: manual
intents: [setup, update, help]
---

# Plugin repositories

A plugin is a runtime package. A new plugin is never added
to the core repository `zajca/pohunek`: it lives in its own repository and
becomes installable by id through the plugin index. This page is the rule and
its details. The mechanics of installing a package are in the
[runtime packages guide](runtime-packages.md).

## What is available when

The plugin kit, the repository template, the plugin index, the runtime features
and the official plugin snapshot arrive in a later release than the one that
introduced this rule. Until they exist, the steps below that name them describe
the target workflow, and a plugin installs as today: an official package from
the signed release catalog, any other archive by the owner's digest
(`--sha256`) or from a developer directory. The four official
runtimes (Pi, Codex, Claude Code and Hermes) stay under `runtime-packages/` and
`compat/` and are changed in place until they move to their own repositories;
no other directory is added there. `cargo xtask docs check` fails a change that
adds one (`plugin-directories`).

## Where a plugin lives

| Tier | Repository | Package id | Created by |
|---|---|---|---|
| Official | `pohunekd/plugin-<name>` | `pohunek.runtime.<runtime_id>` | The owner, from `pohunekd/pohunek-plugin-template` |
| Third party | `<owner>/pohunek-plugin-<name>` in the author's account | Outside `pohunek.*`, for example `<owner>.runtime.<id>` | The author, from the template |

Only the owner creates `pohunekd/*` repositories. An agent proposes one in an
issue and waits for the grant. The `pohunek.` namespace and the
`pohunekd/plugin-*` names are reserved to the official tier.

## Naming

- An official plugin repository is `pohunekd/plugin-<name>`.
  - `<name>` matches `^[a-z][a-z0-9]*(-[a-z0-9]+)*$` and is 2-32 characters
    long.
  - `<name>` is the upstream product's name in kebab-case:
    `plugin-claude-code`, `plugin-codex`, `plugin-pi`, `plugin-hermes`.
  - `<name>` never contains `pohunek`, `plugin` or `runtime`.
- One repository holds one plugin and builds one package id. A release is
  tagged `v<package version>` (semver).
- The package id is `pohunek.runtime.<runtime_id>`. The runtime id is the `--agent` value and may
  differ from `<name>`: `plugin-claude-code` builds `pohunek.runtime.claude`.
  The README's first table maps the repository, the package id, the runtime id
  and the upstream.
- Topics: `pohunek-plugin` and `pohunek-runtime`.
- Supporting repositories use the `pohunek-plugin-` prefix:
  `pohunekd/pohunek-plugin-template`, `pohunekd/pohunek-plugin-kit` and
  `pohunekd/pohunek-plugin-index`.
- A third-party plugin is `<owner>/pohunek-plugin-<name>`, with a package id
  outside `pohunek.*`.

## Repository layout

A plugin repository created from the template holds:

- the descriptor (`runtime.toml` and `detect.toml`) of the runtime package;
- the upstream lock for a runtime package, which pins the upstream release the
  package is proven against;
- a README whose first table maps the repository, the package id, the runtime
  id and the upstream;
- the template's CI and release workflows, which call the reusable workflows of
  `zajca/pohunek` pinned to a core release tag.

## Required CI

The template's CI is required on every pull request and is not edited away:

- lint of the descriptor or manifest against the core schema and the runtime
  features it requires;
- a reproducible double build of the archive, then verification;
- a smoke run of the built package against the core release and the nightly
  core, from the plugin kit, in an isolated root;
- the screen detection corpus for a runtime package;
- a version-bump check, so a changed archive carries a new version.

A release is published on a version tag `vX.Y.Z` by the template's release
workflow, which calls the reusable
`zajca/pohunek/.github/workflows/plugin-release.yml@<core tag>`:

| Input | Meaning |
|---|---|
| `core-tag` (required) | The core release tag whose `xtask` builds the archive; it equals the tag in the caller's `uses:`. The workflow checks out that tag only, never a branch or a commit sha. |
| `package-dir` (default `package`) | The package directory, a path below the repository root. |

The workflow builds the archive with `cargo xtask plugin build` of that core
tag in a job with read-only permissions, and requires the pushed tag to equal
`v` plus the archive's version. A second job, which runs no build code, checks
the checksums, attests `<package_id>-<version>.tar.zst`, and publishes it with
`SHA256SUMS` as a GitHub release of the plugin repository; only that job has
write access to `contents` and `attestations` and the OIDC `id-token`
permission. The workflow restores no cache, uses no secret and has no access to
the plugin index.

The same two commands run locally:

- `cargo xtask plugin build --dir <package dir> --output-dir <dir>` writes the
  canonical archive and prints
  `plugin build ok: <package_id> <version> <digest> <path>`. Two builds of one
  directory are byte-identical, and an existing archive is never replaced.
- `cargo xtask plugin prove --archive <file> --entry <entry.toml> --release-dir
  <dir> --evidence <file>` installs the archive through the index path on an
  isolated daemon started from the `pohunek`, `pohunekd` and `pohunek-sessiond`
  in `--release-dir`, and writes the verdict as evidence JSON. It never launches
  the runtime program and never talks to your own daemon. The
  [index runbook](../runbooks/plugin-index-keys.md#6-build-and-prove-a-plugin-archive)
  lists the steps, the evidence fields and the exit statuses.

## Compatibility declaration

A plugin declares what it needs from core, and core refuses an install that
does not fit once these checks ship:

- `runtime_api`, the major number of the runtime contract;
- the runtime features it requires, each an append-only core id;
- a core version range.

A plugin that needs a core capability that does not exist gets it in a core pull
request first, behind a runtime feature, and adopts it against the nightly kit.
There is no paired core-and-plugin merge, and no plugin-specific patch to core.

## Admission through the index

The plugin index `pohunekd/pohunek-plugin-index` is the list of plugins that
install by id.

- A plugin is admitted, updated or revoked by a reviewed pull request to the
  index repository. No core release is needed for that.
- The pull request adds one entry for the package id, version, archive digest,
  release URL, source repository, tag, platforms, `runtime_api`, required
  features and core range.
- The index CI checks that the submitter controls the source repository, the
  archive digest, the release attestation, and runs the kit proof against the
  current core release and the nightly core. A named reviewer approves.
- An official entry (a `pohunek.*` id) is admitted to the official tier. Every
  other entry is admitted to the verified tier.
- A plugin archive is accepted only when both of these hold:
  - `gh attestation verify <archive> --repo <source.repo> --signer-workflow
    zajca/pohunek/.github/workflows/plugin-release.yml --signer-digest <commit
    of the zajca/pohunek tag source.built_with> --source-ref
    refs/tags/<source.tag> --source-digest <source.commit>
    --deny-self-hosted-runners` succeeds. The commit is resolved only through
    the qualified tag ref of `zajca/pohunek` itself, never through a bare sha,
    so a commit of a fork network cannot satisfy it. Whether `--signer-digest`
    matches the commit of a reusable workflow is checked by the first plugin
    release; admission is not trusted before that check has passed.
  - `cargo xtask plugin build` at `zajca/pohunek` tag `source.built_with`, run on
    `source.commit`, reproduces the entry digest. A build by other code cannot
    match this rebuild, so the `core-tag` input and the workflow commit do not
    decide integrity.
- The pure predicates of an added entry are `cargo xtask index check-entry
  --entries <tree of the entry> --existing <admitted tree> --core-version
  <X.Y.Z> --stage schema|core-range`, built at the pinned core tag. Stage
  `schema` refuses a package id that is not `pohunek.runtime.` plus the runtime
  id, a refused runtime id, an identity or digest already admitted, a version
  that is not a release semver above every admitted version of the package id,
  refuses an entry that would take the index past its entry or size limit, refuses a `url` outside letters, digits and `:/._+-`, and prints the validated
  fields as `key=value` lines for `IFS== read -r`, never for `eval`. Stage `core-range`
  refuses a range that does not parse, names a pre-release, does not admit the
  pinned core or admits a core below `0.40.0`. A refusal exits 1 and prints
  `predicate=<name>: <detail>`, where `<name>` is one of `schema`, `existing-tree`, `package-id-binding`, `refused-runtime-id`, `url-charset`, `identity-conflict`, `duplicate-digest`, `version-not-semver`, `version-not-release`, `duplicate-version`, `version-not-greater`, `core-range-unparsed`, `core-prerelease`, `core-excludes-core-tag`, `core-lower-bound` (the `Predicate` enum in
  `crates/xtask/src/plugin_index/admit.rs`).
- Without admission, an owner can still install a published archive by its
  digest (community tier); such an install never serves a reserved runtime id.

## Release snapshot

From the release that ships the snapshot, a core release bundles the official
set of its day from `packaging/official-plugins.lock.json`, a snapshot of the
official tier of the index. The release proves every locked plugin against its
exact artifacts before it ships, so an offline install of a release carries the
official plugins that were proven with it. Plugins admitted after the snapshot
install from the index.

## Documentation split

- A plugin's own repository documents the plugin: what it integrates with, its
  install, configuration, limits, and what was verified against the real
  upstream.
- Core documents the contract plugins build on: the descriptor and manifest
  schemas, `runtime_api` and the runtime features, the integration handlers,
  the plugin kit, the reusable workflows, the index format and this page. Core
  adds no page for a new plugin.
- The existing pages for the four official runtimes ([Pi](pi-package.md),
  [Codex](codex-package.md), [Claude Code](claude-package.md) and
  [Hermes](hermes-package.md)) stay until each moves.

## Core boundary

Core owns the descriptor and manifest schemas, `runtime_api` and the runtime
features, the integration handlers, the plugin kit, the reusable plugin
workflows, the index format and verifier, and the release catalog. Change core
only for a core capability a plugin needs; do not add a directory under
`runtime-packages/` or `compat/`, a package job to
`.github/workflows/ci.yml`, or a test about one plugin to a core crate.

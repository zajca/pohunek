---
type: Guide
id: guide/plugin-repositories
title: Plugin repositories
description: Where a new runtime package or workflow plugin lives, how its repository and package id are named, the CI and compatibility declaration it carries, how the plugin index admits it, how a core release snapshots the official set, and where its documentation lives.
source_kind: manual
intents: [setup, update, help]
---

# Plugin repositories

A plugin is a runtime package or a workflow plugin. A new plugin is never added
to the core repository `zajca/pohunek`: it lives in its own repository and
becomes installable by id through the plugin index. This page is the rule and
its details. The mechanics of installing a package are in the
[runtime packages guide](runtime-packages.md) and the
[workflow plugins guide](workflow-plugins.md).

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
| Official | `pohunekd/plugin-<name>` | `pohunek.runtime.<runtime_id>` or `pohunek.workflow.<name>` | The owner, from `pohunekd/pohunek-plugin-template` |
| Third party | `<owner>/pohunek-plugin-<name>` in the author's account | Outside `pohunek.*`, for example `<owner>.runtime.<id>` | The author, from the template |

Only the owner creates `pohunekd/*` repositories. An agent proposes one in an
issue and waits for the grant. The `pohunek.` namespace and the
`pohunekd/plugin-*` names are reserved to the official tier.

## Naming

- An official plugin repository is `pohunekd/plugin-<name>`.
  - `<name>` matches `^[a-z][a-z0-9]*(-[a-z0-9]+)*$` and is 2-32 characters
    long.
  - For a runtime package, `<name>` is the upstream product's name in
    kebab-case: `plugin-claude-code`, `plugin-codex`, `plugin-pi`,
    `plugin-hermes`. For a workflow plugin it is the plugin's function:
    `plugin-work-queue`.
  - `<name>` never contains `pohunek`, `plugin`, `runtime` or `workflow`. The
    kind is recorded in the manifest and in the topics.
- One repository holds one plugin and builds one package id. A release is
  tagged `v<package version>` (semver).
- Package ids are `pohunek.runtime.<runtime_id>` and
  `pohunek.workflow.<name>`. The runtime id is the `--agent` value and may
  differ from `<name>`: `plugin-claude-code` builds `pohunek.runtime.claude`.
  The README's first table maps the repository, the package id, the runtime id
  and the upstream.
- Topics: `pohunek-plugin`, plus `pohunek-runtime` or `pohunek-workflow`.
- Supporting repositories use the `pohunek-plugin-` prefix:
  `pohunekd/pohunek-plugin-template`, `pohunekd/pohunek-plugin-kit` and
  `pohunekd/pohunek-plugin-index`.
- A third-party plugin is `<owner>/pohunek-plugin-<name>`, with a package id
  outside `pohunek.*`.

## Repository layout

A plugin repository created from the template holds:

- the descriptor (`runtime.toml` and `detect.toml`) for a runtime package, or
  `workflow.toml` and its payload for a workflow plugin;
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

A release is published by the template's release workflow, which builds the
archive, attests it, and publishes it with its digest and a compatibility
vector.

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
- Without admission, an owner can still install a published archive by its
  digest (community tier); such an install never serves a reserved runtime id.

## Release snapshot

From the release that ships the snapshot, a core release bundles the official set of its day from
`packaging/official-plugins.lock.json`, a snapshot of the official tier of the
index. The release proves every locked plugin against its exact artifacts before
it ships, so an offline install of a release carries the official plugins that
were proven with it. Plugins admitted after the snapshot install from the index.

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
`runtime-packages/` or `compat/`, a package job to `.github/workflows/ci.yml`, or
a test about one plugin to a core crate.

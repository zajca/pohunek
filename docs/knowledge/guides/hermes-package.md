---
type: Guide
id: guide/hermes-package
title: Hermes runtime package
description: The official Hermes Agent runtime package source (Hermes Agent 0.20.0) with its operator plugin payload, what it declares and keeps equal to the built-in Hermes runtime, where releases ship it, and how its tests, CI and the release keep it honest.
source_kind: manual
intents: [setup, update, debug, help]
---

# Hermes runtime package

The Hermes package (`runtime-packages/hermes`) is the package form of the
Hermes Agent runtime that is compiled into the daemon today, plus the operator
plugin payload that `pohunek integration` installs into a Hermes profile. The
directory holds exactly what the archive holds:

```text
runtime-packages/hermes/
  runtime.toml    runtime descriptor (package pohunek.runtime.hermes 1.0.0)
  detect.toml     detection manifest named by `detect_manifest`
  operator.toml   closed sidecar: every operator payload file and its SHA-256
  operator/       the operator plugin payload the sidecar lists
```

Its compatibility lock, the PTY goldens captured from a real Hermes and the
evidence notes stay outside the archive, in `compat/hermes/`
(`compatibility-lock.json`, `goldens/`, `README.md`). See [runtime
packages](runtime-packages.md) for the generic commands and the [Hermes
operator](hermes-operator.md) guide for the plugin, its policy and the
integration lifecycle.

## Status: official package on Linux; built-in still present

`hermes` is a reserved runtime id. A package serves it only with official
trust: a `--catalog` install whose signed catalog entry binds the package id,
runtime id and digest, verified against the host's catalog trust anchor. An
install trusted by `--sha256` is refused with `package_runtime_not_claimable`.

Releases build the package like every other official package
(`pohunek-runtime-hermes-<version>.tar.zst` from `runtime-packages/hermes`).
`compat/matrix.json` attests it for `x86_64-unknown-linux-gnu` and
`x86_64-unknown-linux-musl`, so the signed catalog entry lists those platforms
and every Linux daemon archive ships `runtime/packages/hermes.tar.zst` and
`runtime/attestations/hermes-<target>.json` beside
`runtime/runtime-catalog.json`.
[Installing official packages](runtime-packages.md#install-official-packages-from-a-release)
has the procedure:

```bash
cd pohunek-daemon-<version>-<target>
pohunek plugin install runtime/packages/hermes.tar.zst --catalog runtime/runtime-catalog.json
pohunek plugin install runtime/packages/hermes.tar.zst --catalog runtime/runtime-catalog.json --yes
```

The first installed version is enabled and selected and takes the `hermes`
alias over from the built-in runtime (see [official
aliases](runtime-packages.md#official-aliases)). `pohunek integration install
--agent hermes --hermes-profile <name>` (target and access options in the
[Hermes operator](hermes-operator.md) guide) then installs the operator plugin
from the selected package and reports `provenance` `source=package` with the
package digest.

The package is the only source of the operator plugin: the CLI carries no
copy of its payload, and `pohunek integration install|update --agent hermes`
without an installed package fails with `hermes_package_not_installed`, whose
recovery hint names the install command above.

macOS has no Hermes package row: the Apple Silicon daemon archive carries the
signed catalog and no runtime package
([#929](https://github.com/zajca/pohunek/issues/929)), so a macOS host cannot
install the operator plugin until darwin packages are attested.

Until the built-in is removed
([#147](https://github.com/zajca/pohunek/issues/147)), the built-in Hermes
descriptor, detection manifest and `hermes-v1` version parser serve `hermes`
sessions on a host that has no installed, enabled and selected official
package. An operator plugin that an older CLI installed from its compiled copy
reports `provenance` `source=embedded`; `integration update` moves it to the
package.

## What the package declares

The descriptor equals the built-in `crates/daemon/src/agent/builtin/hermes.toml`
except for two fields: `detect_manifest` names the package's `detect.toml`
instead of the embedded `hermes` manifest, and the version probe is the
data-driven `semver-line-v1` parser instead of the compiled `hermes-v1` one.
`detect.toml` is byte for byte the built-in
`crates/daemon/src/detect/manifests/hermes.toml`.

- Launch: `hermes chat`; resume is `hermes chat --resume <reference>` with an
  `id` reference; fork is unsupported. Later input is a bracketed paste
  followed by Enter 150 ms later, filtered by the `hermes_safe_text` policy.
- Version: `version_probe = { parser = "semver-line-v1", args = ["--version"],
  min = "0.20.0", below = "0.20.1", line = "Hermes Agent v{version}
  {annotation}" }`, which accepts the locked banner `Hermes Agent v0.20.0
  (2026.8.3)`. The range equals the lock's `supported` range; `Hermes Agent
  v0.20.1` and every other release are unsupported.
- Native reference: `strategy = "hook"`.
- Integration: handler `hermes-hook-v1` with hook schema `identity-v1`. The
  package names the compiled handler and schema; its lifecycle runs in the CLI
  (`pohunek integration ... --agent hermes`).
- Detection (`detect.toml`): process matchers are the kernel name `hermes`, a
  command line whose program is `hermes`, or a `python`/`python3[.N]` command
  line running `-m hermes_cli`, `-m hermes_cli.main` or `hermes_cli/main.py`.
  Rules, highest priority first:
  - `fatal_startup` (`blocked`): `Failed to initialize agent:` or `required
    API key(s) missing:` anywhere in the recent screen.
  - `dangerous_command_approval` and `tui_approval` (`blocked`): the classic
    `Dangerous Command` panel or the alternate-screen TUI's `approval required`
    heading, each with `Allow once` and `Deny`, in the bottom 16 non-empty
    lines.
  - `active_turn_prompt` (`working`): the Hermes staff symbol `⚕` before the
    prompt glyph in the bottom 8 non-empty lines; `active_turn_progress` and
    `tui_active_turn` (`working`): a `[thinking]` line, `◆ aggregating` or the
    TUI's `Ctrl+C to interrupt…` there.
  - `idle_prompt` and `tui_idle_status` (`idle`): the classic prompt glyph,
    optionally after a Hermes profile name, or the TUI's `─ ready` rule in the
    bottom 4 non-empty lines.

### Operator payload

`operator.toml` (schema 1) is a closed allowlist: it lists the eight payload
files, `plugin.yaml`, `__init__.py`, `cli.py`, `hooks.py`, `policy.py`,
`redact.py`, `tools.py` and `skills/pohunek/SKILL.md`, each with its SHA-256.
The CLI verifies every path and digest before it installs the plugin, and an
unlisted, missing or altered file, or a policy-path placeholder in
`__init__.py` that is absent or repeated, fails with `hermes_invalid_asset`
(the [Hermes operator](hermes-operator.md) guide lists the source errors).
The package carries no hook reporter script: the reporter
`_pohunek_reporter.py` is core-owned, compiled into the CLI, and installed
beside the operator payload.

`operator/skills/pohunek/SKILL.md` is generated from
`docs/knowledge/guides/hermes-operator.md`. `cargo xtask hermes
generate-skill` writes it and refreshes its checksum in `operator.toml`;
`cargo xtask hermes check-skill` and `cargo xtask docs check` fail when either
differs. A change of that guide therefore changes the package bytes and its
digest.

## How the package is kept honest

- `crates/cli/tests/hermes_package.rs` always runs. It builds the directory
  into an archive and installs it through the CLI and a real daemon with a
  throwaway signed catalog (`crates/test-support/src/catalog.rs`, feature
  `catalog`); the packaged skill gets one extra line, so the installed bytes
  show which source supplied them. Its scenarios:
  - `the_signed_hermes_package_is_selected_and_failed_installs_preserve_it`:
    descriptor and manifest parity with the built-in files above, the supported
    range equal to `compat/hermes/compatibility-lock.json`, the probe accepting
    the locked banner and refusing `Hermes Agent v0.20.1 (future)`, and a
    `--sha256` install (`package_runtime_not_claimable`) and a corrupt archive
    whose digest the catalog does not list (`package_untrusted`, refused before
    decoding) that leave the selection unchanged.
  - `package_backed_install_writes_the_selected_package_bytes`: the installed
    plugin, its provenance and its policy binding come from the selected
    package.
  - `without_a_hermes_package_install_refuses_and_writes_nothing`: with no
    registry and with a registry that holds no Hermes package, install and
    update refuse with `hermes_package_not_installed` and write nothing.
  - `an_embedded_install_reports_the_missing_package_and_moves_to_it_on_update`:
    a version 1, 2 or 3 marker with no package source reports the missing package
    in status and doctor, moves to the package on `integration update`, and
    uninstalls without a package.
  - `an_unusable_selection_without_an_install_is_a_typed_source_error`,
    `losing_the_recorded_package_refuses_update_without_mutation` and
    `update_moves_an_install_to_the_newly_selected_package`: the other source
    errors, and the move between package versions on `integration update`.
  - `the_package_backed_hermes_lifecycle_is_isolated_and_model_free`: the
    complete operator lifecycle through the `pohunek` binary, including policy
    damage, provenance drift, invalid and earlier markers, modified assets and
    a confirmed uninstall.
  - `sidecar_violations_in_a_signed_package_fail_install`: a signed package
    whose sidecar contract is violated never reaches the plugin tree.
- `a_real_hermes_turn_uses_the_selected_package_and_public_session_screen` is
  `#[ignore]`d. It installs the package, installs the operator plugin from it
  into an isolated `HERMES_HOME`, launches a real pinned Hermes with `pohunek
  session new` against a loopback model provider
  (`crates/cli/tests/support/hermes_provider.rs`), submits one prompt with
  `session input`, reads the reply from `session screen`, requires the
  reported native reference to be the conversation Hermes stored, and resumes
  it after `session stop`. Run it with the provisioned stage's executable:
  `POHUNEK_HERMES_E2E=1 POHUNEK_HERMES_BIN=<abs>/venv/bin/hermes
  POHUNEK_PYTHON_BIN=<abs>/venv/bin/python cargo nextest run --profile package
  -p pohunek-cli --test hermes_package --run-ignored all --test-threads 1`.
  Without the variables it fails by name instead of skipping.
- `cargo xtask hermes compatibility --bin-dir ABS --package ARCHIVE` is the
  model-free gate of the pinned release: it installs this archive through a
  copied daemon with a throwaway catalog and checks the CLI, plugin and PTY
  golden evidence (`compat/hermes/README.md`). `scripts/build-hermes-package
  <archive>` builds `ARCHIVE` from this directory.
- The `hermes-package` CI job ("pinned Hermes compatibility (model-free)")
  follows the runtime package selection like the other package jobs. It
  stages the locked release with `cargo xtask compat stage-upstream`, builds
  the daemon, session worker and CLI, builds the archive with `cargo xtask
  package build --evidence-dir` and uploads `package-evidence-hermes`, requires
  a verified release baseline digest to match, runs the compatibility gate,
  restores the untouched stage, runs the real-Hermes scenario under the
  `package` nextest profile and uploads `package-junit-hermes`. The macOS job
  `hermes-compatibility-macos` runs the gate and the packaged plugin smoke on
  Apple Silicon.
- A release runs the consumer suite on the Linux daemon archives for the
  `hermes` rows of `compat/matrix.json` and attests them, and the archive smoke
  installs the package from each Linux daemon archive's own catalog in a
  network namespace (see the [release bundle](../concepts/release-bundle.md)).

Update procedure for a new Hermes release: move the lock
(`compat/hermes/compatibility-lock.json`) and the provisioner's pins together,
move the descriptor's `min`/`below` (and `line` if the banner changes) to the
lock's `supported` range, refresh the goldens with `cargo xtask hermes
refresh-goldens --hermes-bin ABS`, rerun the compatibility gate and the
real-Hermes scenario, and bump the package `version`.

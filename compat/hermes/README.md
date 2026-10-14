# Hermes compatibility evidence

`compatibility-lock.json` is the canonical machine-readable M2 pin: Hermes
Agent `0.20.0`, tag `v2026.8.3`, source commit
`3c27eb6234bf91b8ceee9e9071591b31e9b148cb`, and canonical source-archive
SHA-256 `1e9319c58a7f5e95808546af1091d58472be7437adc63fae0cbb53316e2711aa`.
Runtime code that needs the supported Hermes version should consume this lock
rather than repeat `0.20.0` in another mutable source.

The lock has the shape of the other official-package locks
(`compat/<runtime>/compatibility-lock.json`), so `cargo xtask compat attest`
and `compat verify` read it through `--lock` like any other lock, and
`compat matrix-check` reads it for the official package in
`runtime-packages/hermes/`:

| Field | Meaning |
| --- | --- |
| `schema`, `package`, `runtime` | Lock schema `1`, package `pohunek.runtime.hermes`, runtime `hermes`. |
| `upstream` | The pinned release. Kind `provisioner` means the script named by `provisioner` (`scripts/provision-hermes-compat`) installs it from the verified source archive of `repository` at `tag`, `commit` and `tree` (`source_archive`), not from an npm lockfile. `release`, `binary` and `version_output` (the `--version` banner with one `{release}` placeholder) are as in the npm locks. |
| `supported` | The supported release range `[min, below)`, equal to the package descriptor's `version_probe` range. |
| `verified_with` | What the pinned release was checked with. |
| `launch` | The command contract the gate checks: program, fresh and resume arguments, display modes. |
| `cli_checks`, `plugin_contract`, `golden_states` | The CLI, plugin and PTY evidence contracts of the gate. |

`cargo xtask hermes compatibility` and the provisioner both pin the lock's
exact SHA-256, so any lock edit changes both digests and the CI runtime cache
key, which hashes the lock.

The official Hermes runtime package lives in `runtime-packages/hermes/`
(`runtime.toml`, `detect.toml`, `operator.toml`, `operator/`; see
`docs/knowledge/guides/hermes-package.md`). Its descriptor uses the data-driven
`semver-line-v1` probe for the locked banner and limits support to
`[0.20.0, 0.20.1)`, the lock's `supported` range
(`crates/cli/tests/hermes_package.rs` keeps the two equal). The lock remains
authoritative for the upstream release and source digest.
`builtin-v0.34/` holds golden copies of the built-in Hermes descriptor and
detection manifest of release 0.34.2 (`runtime.toml`, `detect.toml`); the
daemon compiles no Hermes in, and the package tests compare the package with
them.
The reserved `hermes` runtime id can be served by a package only when a signed
catalog authorizes its digest. Releases attest the package for the Linux
x86_64 glibc and musl targets of `compat/matrix.json` and ship it with that
catalog in every Linux daemon archive; the macOS daemon archive carries no
runtime package.

The package's `operator.toml` is the closed sidecar of the allowlisted Hermes
operator payload in its `operator/` directory. The CLI verifies the sidecar and
its payload before installing; the core-owned `_pohunek_reporter.py` remains
outside the sidecar and is compiled into the CLI. The CLI installs the plugin
only from an official Hermes package that is installed, enabled, selected and
catalog-authorized; without one, install and update fail with
`hermes_package_not_installed`.

The skill payload is generated from `docs/knowledge/guides/hermes-operator.md`.
Run `cargo xtask hermes generate-skill` after changing that guide; the command
writes `runtime-packages/hermes/operator/skills/pohunek/SKILL.md` and refreshes
its checksum in `runtime-packages/hermes/operator.toml`. `cargo xtask hermes
check-skill` and `cargo xtask docs check` verify that exact payload and
checksum.

Run the model-free CI gate with an installed pinned executable on `PATH`,
the directory of built Pohunek binaries, and the Hermes runtime package
archive built from `runtime-packages/hermes/`:

```bash
scripts/build-hermes-package /abs/hermes.tar.zst
cargo xtask hermes compatibility --bin-dir ABS_BIN_DIR --package /abs/hermes.tar.zst
```

`ABS_BIN_DIR` must be an absolute directory holding `pohunek`, `pohunekd`, and
`pohunek-sessiond`, each a canonical regular executable with no symlink
components or group/world-write permissions. `scripts/build-hermes-package`
builds `runtime-packages/hermes/` with `cargo xtask package build`. For local
diagnosis, `--hermes-bin <path>` selects another Hermes executable.

The gate installs the operator plugin from that package, the way an operator
host does, and never through a source switch. It refuses an archive whose
descriptor is not `pohunek.runtime.hermes` for runtime `hermes`, copies the
three binaries into a private prefix, writes beside the copied daemon a trust
anchor for a signing key that exists only in the gate process, signs a catalog
with that key that authorizes exactly the archive, starts the copied daemon,
and runs `pohunek plugin install --catalog`. The daemon must then list the
archive digest as the one enabled, selected official Hermes package. Every
`integration` result of the locked install, status, and doctor steps must
report `provenance` `source=package` with `pohunek.runtime.hermes` and the
archive digest, and uninstall must report none; a result with `source=embedded`,
the source an install from an older CLI's compiled copy records, fails the
gate. The daemon must exit cleanly on SIGTERM afterwards. The gate creates
temporary `HOME`, `HERMES_HOME`, XDG, Python user, bytecode, and
uv-cache locations. It clears the ambient environment except for the minimum
process-launch variables, bounds every command by time and output size, checks
the exact lock digest and source-archive contract, rejects a wrong Hermes
version or CLI shape (including the used profile subcommands and their
positional-argument ordering), and validates every committed golden record. It
uses that isolated profile to exercise plugin list/enable/disable, tool, skill,
and hook registration, plus Pohunek integration install/status/doctor/uninstall.
It never runs a model turn, reads an operator profile, or accesses provider
credentials. Validation does not trust a matching checksum alone: every
`captured` fixture must satisfy the state-specific prompt, response, tool,
approval, interruption, exit, resume, or alternate-screen evidence contract
recorded by the refresh.

GitHub Actions stages the executable with `cargo xtask compat stage-upstream
--runtime hermes`, which runs `scripts/provision-hermes-compat` in place and
records the stage in `STAGE.sha256` and `STAGE.links`; on a runtime cache hit
the restored stage goes back to the same absolute path and `cargo xtask compat
verify-stage` re-checks it before the gate runs
(`docs/knowledge/concepts/upstream-staging.md`). The provisioner verifies the reviewed lock
digest before any upstream command runs, then verifies the locked tag, commit,
tree, and canonical source archive before installing the hash-locked upstream
environment. It clears provider and GitHub credential variables and the `uv`
interpreter redirects, and builds the environment from one pinned
python-build-standalone CPython: the release, version, flavor and a SHA-256 per
supported host (Linux x86_64, macOS arm64) are fixed at the top of the script,
the archive is fetched over HTTPS only and checked against its digest before it
is extracted into the isolated runtime root required by Pohunek's
executable-containment checks. A framework build, another version, or an
unsupported host is refused, because the frozen launch rule needs the
entrypoint's shebang interpreter to run the script itself. The host `python3`
only runs the provisioner's helper programs. It uses the reviewed frozen
upstream resolution, and its network-free fail-closed test is
`scripts/tests/provision-hermes-compat.sh`. Both pinned Hermes CI jobs also
drive a real Hermes session through the package
(`crates/cli/tests/hermes_package.rs` with `POHUNEK_HERMES_E2E=1`). The Linux
job then runs the release consumer suite's Hermes driver in row mode on a fresh
restore of the stage (`docs/development.md`, "Release consumer suite").

The same gate runs natively on Apple Silicon in the
`hermes-compatibility-macos` CI job. That job provisions the locked runtime
with the stock macOS `/bin/bash` 3.2 and BSD userland (the provisioner uses
`curl`, `tar`, `sha256sum` or `shasum -a 256`, whichever exists, and no GNU
`realpath`),
runs both shell tests, the xtask Hermes unit tests (through nextest), the compatibility gate, and
`scripts/smoke-hermes-plugin-release` against a packaged development daemon
archive extracted outside the checkout. The smoke takes the extracted daemon
archive, the Hermes package archive, a signed catalog that authorizes it, and
the anchor that trusts the catalog's signer. It copies the three binaries into
a private prefix with that anchor beside the daemon, starts the daemon with a
clean isolated home, installs the package with `pohunek plugin install
--catalog`, and requires every installed-state `integration` result to report
the package and its digest as the provenance. CI and the release workflows run
this smoke on daemon archives that carry no signed release catalog (CI's
development archives, and the release build's archives before assembly adds
the catalog), so they build the package with `scripts/build-hermes-package`
and write the catalog and anchor with `cargo xtask hermes throwaway-catalog`,
whose signing key is random and never written.
The smoke resolves symlinked temporary parents (macOS `/tmp` and `/var/tmp`)
before validating them. The harness resolves symlinks in its isolation root so redaction matches
the paths child processes print, and uses `en_US.UTF-8` where macOS lacks
`C.UTF-8`. The production-plugin isolation root and the smoke root are chosen
short enough that `$XDG_RUNTIME_DIR/pohunek/daemon.sock` fits the platform
socket path limit (103 bytes on macOS); when no candidate parent fits, the run
fails with the byte budget. A missing prerequisite fails the job.

Refresh PTY evidence explicitly with the real pinned Hermes process and PTY
against the repository-owned deterministic model mock:

```bash
cargo xtask hermes refresh-goldens --hermes-bin ABS
```

`ABS` must be the absolute path to the pinned Hermes executable. The harness
still launches that real Hermes binary in a real PTY, but satisfies every
relevant model and remote-metadata dependency locally. Its OpenAI-compatible
endpoint binds only to IPv4 loopback, requires no provider credentials and
incurs no provider cost. Credential-source suppression normally produces no
Copilot startup probe. If the pinned background exchange still starts, the mock
admits at most its three-attempt budget of `CONNECT api.github.com:443`
requests plus three `CONNECT api.githubcopilot.com:443` fallbacks. Fast process
shutdown may shorten those probes or interleave them with scenario traffic. The
mock accepts only the two exact request lines and matching `Host` headers,
returns HTTP 403 before TLS begins, and therefore never receives an
authorization header or token. An over-budget attempt, any other `CONNECT`, any
extra header, or any absolute-form external request fails the scenario. Each of the
six model-bearing classic scenarios must then make this exact localhost
sequence: five ordered detection GETs to `/api/v1/models`,
`/api/tags`, `/v1/props`, `/props`, and `/version`, each receiving a
deterministic HTTP 404; then exactly one `POST /v1/chat/completions`. Discovery
is not cached across those processes. The isolated config statically pins
`pohunek-compat-v1`, `context_length: 64000`, and `discover_models: false`, so
Hermes does not request `/v1/models` and the mock does not permit that path.
Every isolated Hermes home also receives fresh, deterministic
`models_dev_cache.json` and `cache/model_catalog.json` files, while the generated
configuration disables remote model-catalog refreshes. Its isolated `auth.json`
suppresses every Copilot credential source, including the `gh auth token`
fallback. A repository-owned noncredential value is selected before that
subprocess; its pinned three-attempt token exchange is the locally denied probe
described above. An unreachable isolated D-Bus address also prevents child
processes from opening the operator's desktop keyring. Harness-owned HTTP,
HTTPS, and all-protocol proxy
variables point at the loopback mock while `NO_PROXY` is restricted to
localhost. The exact denied Copilot probe is the only admitted non-local proxy
authority; it never opens a tunnel. This is a fail-closed application-level
defense, not OS-level network containment.
The `prompt-ready` and `exit` classic scenarios issue no model API requests.
The mock rejects a wrong order or count and validates the POST model identifier
and last user prompt, plus the terminal tool for terminal scenarios.

The refresh clears the ambient environment except for the minimum
process-launch variables, uses temporary `HOME`, `HERMES_HOME`, XDG, Python,
and cache locations, and never reads the operator's Hermes home or `state.db`.
It sets `HERMES_SKIP_NODE_BOOTSTRAP=1`; the TUI-only process also receives an
empty isolated `PATH`. Missing Node/npm therefore produces reviewable local
`unsupported` evidence instead of installing TUI dependencies or contacting a
package registry. Classic terminal-tool scenarios retain the normal executable
path for their exact repository-owned commands.
Each scenario uses a fixed repository-owned prompt, bounded semantic
readiness/state waits, and bounded output. Classic captures must prove the
expected response or UI state before they can be marked `captured`: multiline
input produces one exact response and exactly one pinned Hermes submitted-user
boundary; bounded normalization accepts Rich continuation lines belonging to
that single preview. Model responses require exactly one ordered terminal
render sequence for the pinned streaming response frame: rounded Hermes header,
exact response content, and rounded footer. The matcher tolerates only the
prompt-toolkit redraws that interleave those real render events and stores a
bounded normalized panel as golden evidence. Working and interruption show a running-tool marker,
approval shows the real approval panel, completion yields the exact response
and native reference, and resume restores that exact reference. The harness
contains each child in a process group, terminates the complete group on every
exit path, and bounds pipe/PTY reader shutdown. Before writing, output is
stripped of terminal control sequences, personal and temporary paths, session
identifiers, timestamps, and credential-shaped assignments. Any remaining
secret-shaped output fails the refresh closed.

Prompt readiness, short and multiline turns, working, approval, completion,
interruption, and native resume are regenerated through the deterministic local
model responses. Resume is derived only from the fresh isolated capture; no
ambient session is continued. The alternate-screen TUI is attempted separately.
Only a recognized, nonempty local Node/workspace/build-unavailable diagnosis
may produce `unsupported`; the harness never bootstraps the missing dependency.
Crashes, rejected model requests, missing alternate-screen evidence, timeouts,
and harness failures abort the refresh. Pending records make `cargo xtask hermes
compatibility` fail; review every refreshed text file before committing it.

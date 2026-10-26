# Runtime packages

Source of the four official runtime packages that still live in this repository
(Pi, Codex, Claude Code and Hermes). A new plugin is never added here; see
[plugin repositories](../docs/knowledge/guides/plugin-repositories.md). Each
subdirectory is one package and holds exactly what its archive holds, nothing
else:

```text
runtime-packages/<runtime>/
  runtime.toml   runtime descriptor (schema 1, runtime_api 1)
  detect.toml    detection manifest named by `detect_manifest`
  operator.toml  optional: closed sidecar listing the operator plugin payload
  operator/      optional: the operator plugin payload, exactly the files the sidecar lists
```

`operator.toml` and `operator/` exist only in a package that ships an operator
plugin for its agent (Hermes); the core CLI verifies every listed path and
SHA-256 before it installs the plugin.

- The directory name is the runtime id; the package id is
  `pohunek.runtime.<runtime>`.
- Upstream evidence stays outside the archive, in `compat/<runtime>/`: the
  compatibility lock (pinned release, supported range, verification record)
  and any captured screens the manifest is tested against.
- Build and check an archive with `cargo xtask package build
  runtime-packages/<runtime> --output <file>` and `cargo xtask package verify
  runtime-packages/<runtime>`. The build is deterministic; its digest is what an
  owner passes to `pohunek plugin install --sha256`. The repository never
  records an archive digest, because the bytes depend on the compressor.
- A package is trusted locally by explicit digest, never as official, until the
  signed catalog (`pohunek plugin install --catalog`) authorizes it.
- Every package ships with an always-running test that parses its directory
  through the daemon's install path, pins its supported range to its lock, and
  runs its manifest on the captured screens (the Hermes manifest is instead
  required to equal the v0.34.2 built-in golden in
  `compat/hermes/builtin-v0.34/` byte for byte), and with an opt-in test
  that drives the real agent through the installed package (see
  `crates/cli/tests/pi_package.rs` and the `pi-package` CI job).
- A package never carries the hook reporter scripts of an agent integration: the
  integration handler it names is compiled core code that owns them. An
  operator plugin payload is not a reporter; the Hermes reporter is core-owned
  and compiled into the CLI, which installs it beside the package's payload.

Packages: [`pi`](pi) (Pi coding agent 1.x from 1.0.0; install steps in
`docs/install.md`), [`codex`](codex) (Codex 0.x from 0.160.0, verified on 0.164.0; serves the reserved
`codex` id only through the signed catalog that every Linux daemon archive ships
since 0.34.2; see `docs/knowledge/guides/codex-package.md`),
[`claude`](claude) (Claude Code 2.x from 2.1.289, verified on 2.1.309; serves the reserved `claude`
id only through the same signed catalog; see
`docs/knowledge/guides/claude-package.md`) and [`hermes`](hermes) (Hermes Agent
0.x from 0.20.0 with its operator plugin payload; serves the reserved `hermes` id only
through the signed catalog that every Linux daemon archive ships; see
`docs/knowledge/guides/hermes-package.md`).

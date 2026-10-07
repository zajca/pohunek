# Runtime packages

Source of the official runtime packages. Each subdirectory is one package and
holds exactly what its archive holds, nothing else:

```text
runtime-packages/<runtime>/
  runtime.toml   runtime descriptor (schema 1, runtime_api 1)
  detect.toml    detection manifest named by `detect_manifest`
```

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
  runs its manifest on the captured screens, and with an opt-in test that
  drives the real agent through the installed package (see
  `crates/cli/tests/pi_package.rs` and the `pi-package` CI job).
- A package never carries the hook reporter scripts of an agent integration: the
  integration handler it names is compiled core code that owns them.

Packages: [`pi`](pi) (Pi coding agent 1.0.x; install steps in
`docs/install.md`) and [`codex`](codex) (Codex 0.160.x; serves the reserved
`codex` id only through a signed catalog, and no release catalog or signing key
exists yet; see `docs/knowledge/guides/codex-package.md`) and
[`claude`](claude) (Claude Code 2.1.x from 2.1.289; serves the reserved `claude`
id only through a signed catalog under the same condition; see
`docs/knowledge/guides/claude-package.md`).

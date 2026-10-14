## Unreleased (2026-10-12, provisioned upstream staging, #147)

- `cargo xtask compat stage-upstream` stages a lock whose `upstream.kind` is
  `provisioner`, a Python virtual environment at `venv/`: it runs the script
  `upstream.provisioner` names (only
  `scripts/<name>`, a regular executable file, never a link) in place at
  `<out>/<runtime>` with a cleared environment, links `bin/<binary>` to the
  entrypoint the script prints, copies the lock and script into `inputs/`,
  requires the entrypoint's first line to be exactly
  `#!<stage>/venv/bin/python` or `#!<stage>/venv/bin/python3` (uv writes the
  latter on macOS),
  writes `STAGE.sha256` and `STAGE.links`, and requires the banner as the first
  line of `--version`. A stage path at which uv would write a `#!/bin/sh`
  trampoline is refused before the script runs. `compat verify-stage` also
  refuses a provisioned stage whose `inputs/` differ from the committed files,
  a trampoline entrypoint, another interpreter and a moved stage. Hermes is staged this way; its
  stage is never moved. Hermes writes into its editable checkout on launch,
  and a daemon-launched session and the CLI's `-I` plugin probes write
  bytecode into it, so a stage is verified from a pristine copy before each
  use. The pinned Hermes CI jobs stage through the command, cache a copy
  taken before any consumer runs, run `verify-stage` on a cache hit, and
  restore and verify that copy again before each later consumer. Documented in `concepts/upstream-staging`,
  `compat/hermes/README.md` and `docs/development.md`.

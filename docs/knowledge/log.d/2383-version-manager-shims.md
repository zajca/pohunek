## Unreleased (2026-11-16, version-manager shims, #2383)

- A profile `program` that is a version-manager shim (a symlink to the `mise`
  manager under the agent's name) is probed through the installed executable
  the manager names and launched through the shim link, so `argv[0]` keeps the
  configured name. The lookup (`mise which`, owner location variables,
  `MISE_OFFLINE=1`, `MISE_AUTO_INSTALL=0`, sandbox working directory, its own
  deadline) is read-only; the probe of the named executable keeps the private
  sandbox. A shim with no installed release reports `available: true`,
  `supported: false` and refuses the launch with `agent_runtime_unsupported`
  naming the empty install set; other `argv[0]` dispatchers behind a symlink
  are probed through their link in the private sandbox ([Agent
  profiles](concepts/agent-profiles.md#wrapper-programs-and-version-probes),
  [Version managers](guides/runtime-packages.md#version-managers)).

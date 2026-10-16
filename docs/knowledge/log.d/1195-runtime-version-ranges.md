## Unreleased (2026-10-16, official runtime packages accept every release up to the next major, #1195)

- Each official runtime package now accepts every upstream release from its
  verified minimum up to the next major release, and no longer one minor:
  codex `[0.160.0, 1.0.0)` (package 1.1.1), claude `[2.1.289, 3.0.0)` (package
  1.0.2), hermes `[0.20.0, 1.0.0)` (package 1.0.1) and pi `[1.0.0, 2.0.0)`
  (package 1.0.3). Hosts update the agent CLIs independently of pohunek, and a
  Codex auto-update to `codex-cli 0.162.1` refused every Codex launch with
  `agent_runtime_unsupported`. `min` stays the tested minimum (the verified
  release for codex, claude and hermes; pi accepts from 1.0.0 and was verified
  on 1.0.2), and releases above the verified one are accepted by policy, not
  verified. Each compatibility lock's
  `supported` range equals its descriptor's range. See
  [the Codex package guide](guides/codex-package.md),
  [the Claude package guide](guides/claude-package.md),
  [the Hermes package guide](guides/hermes-package.md) and
  [the Pi package guide](guides/pi-package.md).
- A refused launch (`session.new`, resume, fork) and the upgrade preflight
  keep the code `agent_runtime_unsupported`, but the message now names the
  parsed release and the supported `[min, below)` for a release outside the
  range, or says that the output does not match the runtime's declared format,
  with a `recover` hint. `pohunek doctor` reports the same in the profile and
  optional-runtime checks. Probe output is never echoed. A failing or
  timed-out probe keeps the generic message. See
  [agent profiles](concepts/agent-profiles.md).
- The Hermes integration (`pohunek integration install|update|doctor --agent
  hermes`, and setup) reads `hermes --version` through the selected Hermes
  package's `version_probe`, with the grammar the daemon's launch gate uses,
  instead of comparing it with one banner. Every build of a supported release
  is accepted, and the CLI accepts exactly the releases a Hermes launch
  accepts. See [the Hermes operator guide](guides/hermes-operator.md).
- A profile or a stopped session pinned to the digest of an older official
  package keeps that package's range. Repin a profile with
  `pohunek plugin profile migrate <name> --yes`. Carrying pinned profiles and
  stopped sessions over to the new package automatically is tracked in #1199.

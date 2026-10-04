# Pohunek Knowledge

This directory is the hand-authored source bundle for the Universal Pohunek
Assistant. It is public-safe Markdown that can be read by humans and by an
ordinary agent session after the bundle is materialized.

Start here:

- [Architecture](concepts/architecture.md) explains the assistant model and data
  flow.
- [Optional team relay](concepts/team-relay.md) records the implemented reduced
  relay foundation and the deferred multi-team design, clearly separated from
  shipped protocol v4.
- [Host identity and local governance](concepts/host-governance.md) describes
  the shipped stable host ID, safe v4 inspection result, and owner-private
  governance persistence boundary.
- [Sessions](concepts/sessions.md), [projects](concepts/projects.md),
  [worktrees](concepts/worktrees.md), and
  [agent profiles](concepts/agent-profiles.md) describe the operating model.
  The [runtime package archive](concepts/runtime-package-archive.md) defines the
  canonical deterministic `tar.zst` format, its strict reader, and its limits.
  The [runtime catalog](concepts/runtime-catalog.md) defines how the signed
  catalog authorizes official packages and how local digest trust differs.
- [Setup](guides/setup.md), [project setup](guides/project-setup.md),
  and [remote hosts](guides/remote-hosts.md)
  cover common configuration paths. The
  [runtime packages guide](guides/runtime-packages.md) covers installing,
  updating, selecting, disabling, and removing runtime packages with
  `pohunek plugin`.
  [Environment and executable resolution](guides/environment-resolution.md)
  documents the macOS `PATH` policy. The
  [Hermes operator](guides/hermes-operator.md) documents the managed plugin,
  policy, typed tools, lifecycle reporting, and recovery boundaries. The
  [Pohunek agent skill](guides/agent-skill.md) is the hand-authored source of
  the bundled agent-facing CLI skill: safe discovery, explicit targeting, JSON
  state reads, event subscriptions, send-and-wait flows, and owner-first
  safety boundaries. `pohunek agent-skill` prints that bundled skill from the
  binary: the skill text verbatim by default, or with `--json` one envelope
  carrying the skill text and its `content_sha256`. The command is fully
  local, so the global `--host` flag is accepted and ignored.
- [TypeScript SDK](guides/ts-sdk.md) covers the TypeScript package surfaces,
  the WebSocket relay transport contract, the test relay, and runtime paths.
- [Debug daemon](runbooks/debug-daemon.md),
  [debug session runtime](runbooks/debug-session-runtime.md),
  [update after release](runbooks/update-after-release.md), and
  [install on macOS](runbooks/install-on-macos.md) are operational runbooks.
- [Trust model](safety/trust-model.md), [secrets](safety/secrets.md), and
  [repo `.pohunek/`](safety/repo-pohunek.md) are safety rules.
- [Assistant system prompt](assistant/system.md) and
  [source map](assistant/source-map.md) define assistant navigation and source
  verification.

Generated reference documentation is intentionally not committed here. It will
be produced by the documentation build pipeline and merged into the materialized
bundle later.

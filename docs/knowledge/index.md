# Pohunek Knowledge

This directory is the hand-authored source bundle for the Universal Pohunek
Assistant. It is public-safe Markdown that can be read by humans and by an
ordinary agent session after the bundle is materialized.

Start here:

- [Architecture](concepts/architecture.md) explains the assistant model and data
  flow.
- [Sessions](concepts/sessions.md), [projects](concepts/projects.md),
  [worktrees](concepts/worktrees.md), and
  [agent profiles](concepts/agent-profiles.md) describe the operating model.
- [Setup](guides/setup.md), [project setup](guides/project-setup.md),
  [remote hosts](guides/remote-hosts.md), [launcher](guides/launcher.md), and
  [GUI setup](guides/gui.md) cover common configuration paths.
- [Debug daemon](runbooks/debug-daemon.md),
  [debug launcher](runbooks/debug-launcher.md), and
  [update after release](runbooks/update-after-release.md) are operational
  runbooks.
- [Trust model](safety/trust-model.md), [secrets](safety/secrets.md), and
  [repo `.pohunek/`](safety/repo-pohunek.md) are safety rules.
- [Assistant system prompt](assistant/system.md) and
  [source map](assistant/source-map.md) define assistant navigation and source
  verification.

Generated reference documentation is intentionally not committed here. It will
be produced by the documentation build pipeline and merged into the materialized
bundle later.

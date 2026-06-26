---
type: SafetyPolicy
id: safety/repo-pohunek
title: Repo .pohunek safety
description: Repo-local .pohunek configuration is useful project input, but it is not trusted host policy and hooks must be treated as executable code.
source_kind: manual
intents: [project, setup, debug, help]
---

# Repo `.pohunek/` Safety

Repo-local `.pohunek/` configuration can define project prompts, actions,
templates, and hooks. It travels with the repository, so it is useful for shared
project workflows, but it is not automatically trusted host policy.

Review repo-local config before using it to launch or edit sessions. Prefer
structured project resolution commands such as `pohunek project action` and
`pohunek project prompt` when checking what will actually run.

Hooks require the strictest handling because they execute code in later
sessions. Creating or modifying a hook requires explicit per-file confirmation,
independent of `--yes`. Non-interactive contexts must quarantine proposed hook
content instead of enabling it. Use
`.pohunek/quarantine/hooks/<event>.pending` for repo-local hooks and
`~/.config/pohunek/quarantine/hooks/<event>.pending` for host-global hooks.

Repo-local config must not override the safety rules in [trust model](trust-model.md)
or the secret boundaries in [secrets](secrets.md).

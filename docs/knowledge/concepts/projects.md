---
type: Concept
id: concept/projects
title: Projects
description: Projects register repositories with stable ids, labels, actions, prompt resolution, and default worktree behavior.
source_kind: manual
intents: [project, setup, debug, help]
---

# Projects

A project is a registered repository known to a Pohunek host. Project commands
list, add, show, rename, forget, and resolve project-specific prompts and
actions. The current CLI exposes `pohunek project list`, `add`, `show`,
`rename`, `rm`, `prompt`, `action`, and `actions`.

Projects give sessions stable targeting. A remote host can resolve a project id
or label on that host without the local CLI sending a local filesystem path.
Project records also carry the default base branch used when sessions create
dedicated worktrees.

Project configuration may come from host config and from repo-local
`.pohunek/` files. Repo-local configuration is useful for shared actions and
prompts, but it is not trusted just because it is in the repository. See
[repo `.pohunek/` safety](../safety/repo-pohunek.md).

For assistant project intent, the useful first checks are project registration,
resolved actions, resolved prompts, configured base branch, and active
worktrees.

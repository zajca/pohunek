---
type: Guide
id: guide/project-setup
title: Project setup
description: Register a repository as a Pohunek project and verify its prompts, actions, and worktree behavior.
source_kind: manual
intents: [project, setup, help]
---

# Project Setup

Register a repository on the host that will run sessions:

1. Use `pohunek project add <path>` for a specific local path, or omit the path
   on the local host to use the current directory.
2. Add `--name <name>` when the default label is not clear.
3. Add `--base-branch <branch>` when worktree sessions should consistently start
   from a specific base branch.
4. Verify the record with `pohunek project show <id-or-label> --json`.

For project launch behavior, inspect the resolved action and action list:

- `pohunek project actions <id-or-label> --json`
- `pohunek project action <id-or-label> <action> --json`
- `pohunek project prompt <id-or-label> <prompt> --json`

Repo-local `.pohunek/` files can define prompts and actions for the project.
Treat those files as repository code, not trusted host policy. Review them before
using or editing them, especially hooks. See [repo `.pohunek/`](../safety/repo-pohunek.md).

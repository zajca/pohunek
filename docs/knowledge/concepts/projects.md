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
`rename`, `rm`, `prompt`, `action`, `actions`, and `candidates`.

Projects give sessions stable targeting. A remote host can resolve a project id
or label on that host without the local CLI sending a local filesystem path.
Project records also carry the default base branch used when sessions create
dedicated worktrees.

A client of an older build shows a project source, an action provider or a
prompt layer that a newer daemon adds as `unknown`; an unknown provider is never
treated as the provider-less `none` kind. The daemon's own project store stays
strict.

## Project candidates

`pohunek project candidates [--sources pohunek,history] [--json]` proposes
repositories to register. It is read-only: it registers nothing, and
`pohunek project add` stays the only way to register. The default source is the
working directories of the host's own sessions; worktrees collapse onto their
repository, and `registered` says whether a project record exists.

`--sources history` reads agent CLI history instead of the session source (name
both with `--sources pohunek,history`), and naming it is the consent to do so. The privacy contract:

- Only the locations the installed runtime packages declare (the `[transcripts]`
  table of the runtime descriptor) are opened, and only `jsonl-head-v1` files.
- From each file only the head is read, and only two values are kept: the
  working directory of the first record that names one, and the file's
  modification time. Message bodies are never interpreted, kept or logged.
- Symlinks below a history root are never followed.
- The history source is served on the local control socket only; a remote
  connection is refused with `local_only_method`.
- A path outside the user's home, missing, in a temporary directory or outside a
  git repository is counted in `dropped` and never named. The origin URL is
  returned without userinfo.
- Bounds on files, entries, bytes, records per file and time are set by the daemon; a
  bound that is reached is reported in `limits_reached`, not as an error.

The output fields are `repo_root`, `origin`, `last_used_at`, `sessions` (per
agent `owned` and `history` counts), `registered` and `exists`, plus the
top-level `dropped` and `limits_reached`. A client such as an onboarding guide
can use the result to propose projects on a fresh install without reading any
transcript itself.

Project configuration may come from host config and from repo-local
`.pohunek/` files. Repo-local configuration is useful for shared actions and
prompts, but it is not trusted just because it is in the repository. See
[repo `.pohunek/` safety](../safety/repo-pohunek.md).

For assistant project intent, the useful first checks are project registration,
resolved actions, resolved prompts, configured base branch, and active
worktrees.

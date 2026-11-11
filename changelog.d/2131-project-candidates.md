---
kind: added
---
`pohunek project candidates [--sources pohunek,history] [--json]` (method `project.candidates`) proposes repositories you work in without registering anything. The default source is the working directories of your pohunek sessions, with worktrees collapsed onto their repository. `--sources history` (or `--sources pohunek,history` for both) reads the agent CLI history that your installed runtime packages declare (Claude Code, Codex) and is your consent to do so: only the working directory and modification time of each history file are read, symlinks are not followed, paths outside your home, missing, temporary or outside a git repository are only counted, and origin URLs are shown without credentials.

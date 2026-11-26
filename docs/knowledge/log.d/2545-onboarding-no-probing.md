## Unreleased (2026-11-26, onboarding guide does not probe the disk or git, #2545)

- `guides/onboarding.md` (Profiles) takes a second Claude or Codex config home only
  from the user; `pohunek integration status --all-profiles --json` only excludes
  homes that a profile already uses. The agent no longer loops over
  `~/.claude-*` and `~/.codex-*` to test for settings files.
- `guides/onboarding.md` (Projects, step 5) adds `--base-branch` only when the user
  names one; the agent no longer runs `git symbolic-ref` for it.
- Both commands are outside the automatic-mode allowlist and stopped the session on a
  permission prompt (pohunekd/omarchy#40).

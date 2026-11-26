## Unreleased (2026-11-26, onboarding no longer probes the disk for agent homes, #40)

- `guides/onboarding.md` (Profiles) takes a second Claude or Codex config home only
  from the user; `pohunek integration status --all-profiles --json` only excludes
  homes that a profile already uses. The onboard agent no longer loops over
  `~/.claude-*` and `~/.codex-*` to test for settings files: that shell loop is
  outside the automatic-mode allowlist and stopped the session on a permission
  prompt (pohunekd/omarchy#40).

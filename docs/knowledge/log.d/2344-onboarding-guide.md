## Unreleased (2026-11-14, Onboarding guide, #2344)

- New [Onboarding](guides/onboarding.md) guide (`guide/onboarding`, intents
  `setup` and `project`) for an assistant session marked with `onboard.mode`:
  the host checks with their pass condition and fix owner, project proposals
  from `project candidates` (exists, not registered, used within 90 days or
  with at least 3 sessions), session groups through `companion.group`, profiles
  only through `plugin profile create`, the guided and automatic mode rules
  with the grant ids and the non-grantable floor, the hand-over wording, the
  `onboard.state=done` end marker, and the Claude allow and deny rules and the
  Codex arguments of the `pohunek-onboard` launch profile.

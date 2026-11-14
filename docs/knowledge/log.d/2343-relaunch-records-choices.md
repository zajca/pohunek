## Unreleased (2026-11-14, profile option follow-ups, #2343)

- A resume records the option choices its profile resolution took on the
  session (`SessionInfo.agent_options`, the resume binding): the recorded ones,
  and the default of an option the record lacks. A relaunch accepted with
  `accept_profile_change` after an option was added therefore lists that
  option's default, and the record always matches the frozen profile revision.
  New scenarios pin the recorded choices on the adopted built-in, newer-version
  and repinned resume paths. See [Agent profiles](concepts/agent-profiles.md).

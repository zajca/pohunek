## Unreleased (2026-11-14, plugin profile rm, #2346)

- `pohunek plugin profile rm <name> --digest <file digest> [--json]` and the
  local-only `package.remove_profile` method (behind
  `package_remove_profile_supported`) remove a host profile or preset file only
  while it still has the `file_digest` that `plugin profile list --json --all`
  reports; the removal is daemon-mediated and bound to the inode it read. New
  error codes: `package_profile_builtin`, `package_profile_package_runtime`,
  `package_profile_in_use`, `package_profile_has_presets` (both follow the file a
  name opens, including in-tree links and case-folded spellings); a changed file
  is `package_profile_digest_mismatch`. A create that resolved a profile removed
  before it took the launch guard fails with `agent_profile_not_found`. See
  [Agent profiles](concepts/agent-profiles.md) and
  [Runtime packages](guides/runtime-packages.md).

## Unreleased (2026-10-15, preflight profile warnings, #1040)

- `pohunekd upgrade-preflight` reports `profile_warnings`: each host profile
  whose detection-manifest override lacks the screen idle rule its `screen_idle`
  base declares. Each profile is judged as the new release loads it, the planned
  pinned text when the release plan rewrites it, else the profile on disk.
- `pohunek service check` and the upgrade print the warnings next to the plan
  and the session verdicts, and `--json` carries the field. A warning never
  refuses the upgrade and needs no `--accept-runtime-loss`.
- The public API document and the update runbook describe the field and the
  code.

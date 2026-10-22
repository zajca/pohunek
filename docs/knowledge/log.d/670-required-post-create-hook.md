## Unreleased (2026-10-22, required post-create hook, #670)

`service.toml` `[hooks] required = ["post-create"]` makes a failing `post-create`
hook abort `session.new` with the new error code `hook_required_failed`
instead of leaving a `hook` warning. The failing script ends the hook (a later
repository script does not run), the create is rolled back with the remove
hooks, and the error names the event, the layer, the script, how it ended, the
scripts that did not run, the stderr tail and, after a `--from-remote` reset,
the backup ref. The key is optional and omitted while empty, so the schema does
not change; an invalid, unsupported or repeated entry fails the configuration
load, and `pohunek service check` reports it. See the hooks guide in
[worktrees](concepts/worktrees.md), [setup](guides/setup.md) and
`docs/public-api.md`.

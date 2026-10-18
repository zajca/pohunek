## Unreleased (2026-10-18, a response budget for plugin install and update, #1205)

- `pohunek plugin install` and `pohunek plugin update` now give one daemon
  response up to 30 s (named `INSTALL_RESPONSE_TIMEOUT` in the plugin command
  module) instead of the SDK's 5 s default: validating and unpacking a whole
  package archive can exceed 5 s on a slow host, which failed the command with
  `request_timeout` while the daemon went on installing. `plugin link`,
  `list`, `inspect` and every other command keep the default, there is no
  retry, and the SDK default is unchanged. The budget is the CLI's tolerance
  for a slower install, not a daemon install deadline; a timed-out command may
  still have installed server-side, so reconcile with `pohunek plugin list`.
- [Runtime packages](guides/runtime-packages.md) documents the budget, and the
  release consumer suite gains a regression guard that drives the real CLI and
  daemon through a transparent Unix-socket relay holding the daemon's real
  `package.install` responses for 6 s: install and update succeed past the
  former 5 s budget, and without the budget the delayed preview response
  times out.

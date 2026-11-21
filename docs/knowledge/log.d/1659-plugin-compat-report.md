## Unreleased (2026-11-21, plugin compat report, #1659)

- `pohunek plugin compat [--json]` prints the local core's side of the runtime
  package contract: the core version (the daemon's `daemon_version`), the
  protocol version, the `runtime_api` generation and every runtime feature id.
  `host.inspect` carries `runtime_api` and `runtime_features` as optional
  fields, also with `skip_runtime_probes`; a daemon that omits them is
  unknown, and the command fails with `plugin_compat_unsupported` instead of
  printing an empty list. See [Compatibility
  report](guides/runtime-packages.md#compatibility-report).

## Unreleased (2026-11-27, list count bounds, #1110)

- `project.actions` resolves at most 3200 actions (`MAX_PROJECT_ACTIONS` in
  `crates/daemon/src/project/config.rs`); above the bound, or when the reply
  would exceed one control line, the daemon answers
  `runtime/project_actions_too_large` instead of an unframeable line. Action
  and template names are capped at 128 bytes (`MAX_CONFIG_NAME_BYTES` in the
  same file); an over-cap name is refused with `runtime/invalid_name`, never
  truncated.
- The daemon's `host.discover` reply is bounded at 1500 peer records
  (`MAX_HOST_DISCOVER_RECORDS` in `crates/daemon/src/api/handler/host.rs`) and
  checked against the control-line limit; above either bound it answers
  `runtime/host_discover_too_large`. Peer records cap their supplied fields at
  build time (`MAX_HOST_NAME_BYTES` 63, `MAX_HOST_FQDN_BYTES` 253,
  `MAX_HOST_PEER_ID_BYTES` 64, `MAX_HOST_DAEMON_VERSION_BYTES` 32 in
  `crates/protocol/src/discovery.rs`): an over-cap field is omitted and named by
  the record's bounded `reason` (`identity_too_large`,
  `daemon_version_too_large`), never echoed, so one hostile peer cannot
  inflate the reply. Discovery itself, its cache and its range forwarding are
  unchanged. Both codes are additive and need no protocol bump.
- `package.list` keeps its existing registry bound: at most `MAX_PACKAGES`
  (1024) installed packages (`crates/package/src/registry.rs`), whose records
  are text-free, so a full registry lists in one control line.
  `session.runtime_inventory` is not a wire method and stays removed.
  See [Projects](concepts/projects.md) and [Remote hosts](guides/remote-hosts.md).

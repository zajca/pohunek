## Unreleased (2026-11-27, list count bounds, #1110)

- `project.actions` resolves at most 1024 actions (`MAX_PROJECT_ACTIONS` in
  `crates/daemon/src/project/config.rs`); above the bound, or when the reply
  would exceed one control line, the daemon answers
  `runtime/project_actions_too_large` instead of an unframeable line.
- The daemon's `host.discover` reply is bounded at 1024 peer records
  (`MAX_HOST_DISCOVER_RECORDS` in `crates/daemon/src/api/handler/host.rs`) and
  checked against the control-line limit; above either bound it answers
  `runtime/host_discover_too_large`. Discovery itself, its cache and its range
  forwarding are unchanged. Both codes are additive and need no protocol bump.
- `package.list` keeps its existing registry bound: at most `MAX_PACKAGES`
  (1024) installed packages (`crates/package/src/registry.rs`), whose records
  are text-free, so a full registry lists in one control line.
  `session.runtime_inventory` is not a wire method and stays removed.
  See [Projects](concepts/projects.md) and [Remote hosts](guides/remote-hosts.md).

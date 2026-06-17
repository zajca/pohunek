# Phase 2: Remote Hosts over NetBird

## Objective

Extend `zagentmesh` from durable local sessions to your other machines on a
NetBird (WireGuard) network. The CLI connects directly to a remote host's daemon
over NetBird, reusing the Phase 1 protocol unchanged. Hosts are discovered from
local NetBird state with no provider API token.

## User Value

You can move agent work to any of your machines on the NetBird network and drive
it with the same CLI workflow as local sessions — without a central server, an
SSH bridge, or any cloud API token.

## Scope

- Daemon TCP listener bound **only** to the host's NetBird address (`100.x.y.z`),
  never `0.0.0.0`. Same control protocol and attach-stream model as Phase 1.
- CLI remote targeting using a consistent grammar (`host/session-id`), so every
  local command works against a remote host.
- NetBird discovery: enumerate peers from `netbird status --json` (tokenless),
  probe which peers run a reachable `zagentmesh` daemon, and obtain capabilities
  by a live `host inspect` query.
- Host commands: `host list`, `host discover`, `host inspect`.
- Remote session lifecycle: new, list, inspect, attach, detach, stop, status —
  reusing the Phase 1 implementation across the NetBird connection.

## Out of Scope

- SSH bridge / SSH transport (explicitly replaced by NetBird direct transport).
- Tailscale or other VPN adapters (NetBird only).
- Signed manifests, mesh snapshot sync, key rotation (dropped from the design).
- Multi-user authorization (single operator).
- Provider integrations, native GUI, ACP.

## Deliverables

- Daemon TCP listener on the NetBird interface, sharing the Phase 1 control +
  attach protocol; bind validated to refuse non-NetBird interfaces.
- CLI host targeting:
  ```bash
  zagentmesh host discover            # peers from `netbird status --json`
  zagentmesh host list                # known/reachable hosts + daemon status
  zagentmesh host inspect <host>      # live capability query over NetBird
  zagentmesh session new --host <host> --agent <codex|claude>
  zagentmesh attach <host>/<session-id>
  zagentmesh status --host <host>
  zagentmesh session stop <host>/<session-id>
  ```
- NetBird discovery adapter: defensive parsing of `netbird status --json`
  (optional fields default, unknown fields ignored), pinned with fixtures.
- Port probe to classify peers: candidate / reachable-daemon / unreachable.
- Live capability query (`host inspect`) returning daemon version, supported
  agents, available runtimes, repos/worktrees as applicable.
- Structured logs and event-log records for remote connections, discovery runs,
  and remote command failures, with `request_id` correlation across hosts.

## Architecture Impact

Phase 2 proves the peer-over-NetBird direction. Each host remains authoritative
for its own daemon, PTYs, metadata, logs, and live sessions. The local CLI
controls a remote host by dialing that host's daemon over NetBird — there is no
central service and no second protocol. NetBird/WireGuard provides encryption and
network authentication; NetBird policies decide which peers can reach the daemon
port. The attach byte stream uses the same separate-connection model as Phase 1,
now over NetBird TCP.

## CLI/UX Implications

- One grammar for local and remote work; `host/session-id` selects the host.
- Output shows whether a command is local or remote, which host is authoritative,
  and whether a failure came from NetBird reachability, the remote daemon, or the
  remote agent.
- `--json` preserves stable fields for host identity, NetBird address, daemon
  status, session identity, agent type, and state.
- Discovery output distinguishes candidates, reachable daemons, and hosts whose
  daemon is absent or a version mismatch.

## Data/Protocol Implications

- Host records: alias, NetBird address, NetBird name, daemon version (when
  known), last successful connection time, capability hints. No secrets/keys.
- The remote daemon is the protocol authority; request/response/error/event
  envelopes match Phase 1. Protocol version negotiation handles daemon skew.
- Discovery records: source (NetBird), observed addresses/names, last seen,
  reachable-daemon flag.
- Live PTY streams stay direct to the owning host's daemon over NetBird.

## Testing and Verification

- Unit-test `netbird status --json` parsing from recorded fixtures: missing
  fields, unknown fields, duplicate/offline peers, absent NetBird CLI.
- Unit-test host target normalization and routing for local vs `host/session-id`.
- Integration-test the daemon TCP listener binds only to the NetBird interface
  and refuses other interfaces.
- Integration-test remote session create/list/inspect/attach/detach/stop over a
  NetBird connection (or a loopback TCP stand-in in CI).
- Verify remote attach/detach does not kill the remote daemon-owned process.
- Verify remote `--json` output and `request_id` correlation across both hosts'
  logs.
- Verify NetBird-unreachable, remote-daemon-absent, and version-mismatch produce
  clear, distinct errors.

## Success Criteria

- Discover your NetBird peers with no provider API token and see which run a
  `zagentmesh` daemon.
- Start a Codex or Claude Code PTY/TUI session on a selected remote host.
- Attach, detach, and reattach to a remote session without killing the remote
  process.
- Local and remote commands share consistent CLI shapes and `--json` behavior.
- Remote control works over NetBird directly, with no central server and no SSH
  bridge.
- The daemon port is reachable only on the NetBird interface.

## Risks

- NetBird local state format may drift or differ across versions/distros.
  Mitigation: defensive parsing + fixtures; use documented `status --json`.
- Daemon-as-network-server surface. Mitigation: bind only to NetBird; rely on
  NetBird policies; never `0.0.0.0`.
- Attach latency/resize over NetBird may expose stream issues. Mitigation: the
  separate attach connection isolates byte streaming from control.
- Remote error classification can blur NetBird vs daemon vs agent failures.
  Mitigation: typed error classes carried end-to-end with `request_id`.

## Exit Criteria

- Tokenless NetBird discovery and live capability inspection work through the CLI.
- Remote daemon commands reuse the Phase 1 protocol over a direct NetBird
  connection.
- Remote PTY/TUI sessions for Codex and Claude Code support the full lifecycle.
- Logs and event records give enough auditability for single-operator debugging.
- The implementation is ready for later optional work (providers, GUI) without
  protocol changes.

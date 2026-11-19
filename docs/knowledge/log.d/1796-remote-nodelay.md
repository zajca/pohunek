## Unreleased (2026-11-19, remote TCP_NODELAY, #1796)

- Every connection the daemon accepts on an overlay (NetBird or Tailscale)
  address now has `TCP_NODELAY` set next to the TCP keepalive from the
  `service.toml` `[waits]` keys, so small daemon replies leave without
  waiting for the previous ACK instead of leaving in RTT-spaced bursts on a
  real link. A failure to set the option is logged as a warning
  (`remote TCP_NODELAY setup failed`, with the peer and the errno) and the
  connection is still served. See [setup](guides/setup.md).

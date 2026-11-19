---
kind: fixed
---
The daemon now disables Nagle (`TCP_NODELAY`) on each connection it accepts
on an overlay (NetBird or Tailscale) address, next to the existing TCP
keepalive setup. Small daemon replies no longer wait for the ACK of the
previous unacknowledged segment, so they stop leaving in RTT-spaced bursts on
a real link. A failure to set the option is logged as a warning
(`remote TCP_NODELAY setup failed`, with the peer and the errno) and the
connection is still served. ([#1796](https://github.com/zajca/pohunek/issues/1796))

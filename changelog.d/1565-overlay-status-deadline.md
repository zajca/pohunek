---
kind: fixed
---
A wedged NetBird or Tailscale daemon no longer hangs host listing, discovery, `--host` resolution or the daemon's remote-listener retry. The provider status command now runs under one deadline, `status_timeout_secs` in `overlays.toml` (default 10, range 1 to 120 seconds), and is killed with everything it started when the deadline passes. An address that the provider lists for more than one peer, or for this machine, is no longer used for routing: selecting those peers fails as `overlay_host_ambiguous`, and `host list` shows them as candidates without an address and with the reason `address_ambiguous`.
